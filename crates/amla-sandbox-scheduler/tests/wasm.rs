//! WASM tests for amla-scheduler.
//!
//! These tests run under wasm-pack test --node

#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

use amla_scheduler::{
    AsyncPipe, Error, Executor, Exit, HostChannel, RunState, Spawner, TaskResult, join_all_tasks,
    select_first_task,
};
use std::cell::RefCell;
use std::rc::Rc;

fn complete_slot(slot: &Rc<RefCell<amla_scheduler::TaskSlot>>, result: Result<Exit, Error>) {
    let mut s = slot.borrow_mut();
    s.result = Some(result);
    if let Some(waker) = s.waker.take() {
        waker.wake();
    }
}

#[wasm_bindgen_test]
fn wasm_executor_simple() {
    let exec = Executor::new();
    exec.spawn(async { Ok(Exit::success()) });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 1);
            match &results[0] {
                TaskResult::Ok(exit) => assert_eq!(exit.code, 0),
                TaskResult::Err(_) => panic!("unexpected error"),
            }
        }
        RunState::Blocked => panic!("unexpected blocked"),
    }
}

#[wasm_bindgen_test]
fn wasm_pipe_backpressure() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(16); // Small pipe
    let pipe_writer = pipe.clone();
    let pipe_reader = pipe;

    let received = Rc::new(RefCell::new(Vec::new()));
    let received_clone = received.clone();

    // Writer task
    exec.spawn(async move {
        // Write more than pipe capacity
        let data = b"hello world, this is a longer message";
        let mut written = 0;
        while written < data.len() {
            let n = pipe_writer.write(&data[written..]).await?;
            written += n;
        }
        pipe_writer.close();
        Ok(Exit::success())
    });

    // Reader task
    exec.spawn(async move {
        let mut buf = [0u8; 8];
        loop {
            let n = pipe_reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            received_clone.borrow_mut().extend_from_slice(&buf[..n]);
        }
        Ok(Exit::success())
    });

    let state = exec.run();
    assert!(matches!(state, RunState::Done(_)));
    assert_eq!(
        &*received.borrow(),
        b"hello world, this is a longer message"
    );
}

#[wasm_bindgen_test]
fn wasm_host_ops() {
    let exec = Executor::new();
    let channel = HostChannel::new(10);
    let channel_clone = channel.clone();

    let result_data = Rc::new(RefCell::new(Vec::new()));
    let result_data_clone = result_data.clone();

    exec.spawn(async move {
        let data = channel_clone.file_read("/test.txt").await?;
        *result_data_clone.borrow_mut() = data;
        Ok(Exit::success())
    });

    // Run until blocked
    let state = exec.run();
    assert!(matches!(state, RunState::Blocked));

    // Complete the operation
    let req = channel.take_pending().unwrap();
    channel.complete(req.id, b"wasm works!".to_vec());

    // Run to completion
    let state = exec.run();
    assert!(matches!(state, RunState::Done(_)));
    assert_eq!(&*result_data.borrow(), b"wasm works!");
}

#[wasm_bindgen_test]
fn wasm_spawner_and_pipeline() {
    let exec = Executor::new();
    let spawner = Spawner::new();
    let spawner_clone = spawner.clone();

    let pipe = AsyncPipe::new(64);
    let pipe_writer = pipe.clone();
    let pipe_reader = pipe;

    let final_output = Rc::new(RefCell::new(Vec::new()));
    let final_output_clone = final_output.clone();

    exec.spawn(async move {
        // Stage 1: writes to pipe
        let h1 = spawner_clone.spawn(async move {
            pipe_writer.write(b"wasm pipeline test").await?;
            pipe_writer.close();
            Ok(Exit::success())
        });

        // Stage 2: reads from pipe
        let h2 = spawner_clone.spawn(async move {
            let mut buf = [0u8; 64];
            let n = pipe_reader.read(&mut buf).await?;
            final_output_clone.borrow_mut().extend_from_slice(&buf[..n]);
            Ok(Exit::success())
        });

        // Wait for both
        let results = join_all_tasks(vec![h1, h2]).await;
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());

        Ok(Exit::success())
    });

    let _ = exec.run();

    // Process spawn requests
    while let Some(request) = spawner.take_request() {
        let slot = request.result_slot.clone();
        exec.spawn(async move {
            let result = request.future.await;
            complete_slot(&slot, result);
            Ok(Exit::success())
        });
    }

    let _ = exec.run();

    assert_eq!(&*final_output.borrow(), b"wasm pipeline test");
}

#[wasm_bindgen_test]
fn wasm_select_first() {
    let exec = Executor::new();
    let spawner = Spawner::new();
    let spawner_clone = spawner.clone();

    let winner_code = Rc::new(RefCell::new(0i32));
    let winner_code_clone = winner_code.clone();

    exec.spawn(async move {
        let h1 = spawner_clone.spawn(async { Ok(Exit::code(1)) });
        let h2 = spawner_clone.spawn(async { Ok(Exit::code(2)) });
        let h3 = spawner_clone.spawn(async { Ok(Exit::code(3)) });

        let (idx, result) = select_first_task(vec![h1, h2, h3]).await;
        if let Ok(exit) = result {
            *winner_code_clone.borrow_mut() = exit.code;
        }
        // idx tells us which completed first
        assert!(idx < 3);

        Ok(Exit::success())
    });

    let _ = exec.run();

    // Complete task 1 (index 1, code 2) first
    let _req0 = spawner.take_request().unwrap();
    let req1 = spawner.take_request().unwrap();
    let _req2 = spawner.take_request().unwrap();

    let slot = req1.result_slot.clone();
    exec.spawn(async move {
        let result = req1.future.await;
        complete_slot(&slot, result);
        Ok(Exit::success())
    });

    let _ = exec.run();

    assert_eq!(*winner_code.borrow(), 2);
}
