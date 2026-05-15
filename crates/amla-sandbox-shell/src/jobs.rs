//! Job control.
//!
//! Tracks background jobs and provides `jobs`, `fg`, `bg` functionality.

use amla_scheduler::TaskHandle;

/// Job state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Job is running.
    Running,
    /// Job has completed with exit code.
    Done(i32),
}

/// A background job.
pub struct Job {
    /// Job ID (1-based, for %1, %2, etc.).
    pub id: usize,
    /// Command string (for display).
    pub command: String,
    /// Task handles for all tasks in the job.
    pub handles: Vec<TaskHandle>,
    /// Job state.
    pub state: JobState,
}

impl Job {
    /// Check if all tasks are complete.
    pub fn is_complete(&self) -> bool {
        self.handles
            .iter()
            .all(amla_scheduler::TaskHandle::is_complete)
    }

    /// Update state based on task completion.
    pub fn update_state(&mut self) {
        if self.is_complete() {
            // Get exit code from last task
            if let Some(handle) = self.handles.last() {
                if let Some(result) = handle.try_get() {
                    self.state = JobState::Done(result.map(|e| e.code).unwrap_or(1));
                }
            } else {
                self.state = JobState::Done(0);
            }
        }
    }

    /// Kill the job by cancelling all its tasks.
    ///
    /// Uses structured concurrency: cancelling a task cascades to all its children.
    pub fn kill(&self) {
        for handle in &self.handles {
            handle.cancel();
        }
    }
}

/// Job table.
#[derive(Default)]
pub struct JobTable {
    /// Active jobs.
    jobs: Vec<Option<Job>>,
    /// Next job ID.
    next_id: usize,
}

impl JobTable {
    /// Create a new job table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new background job.
    ///
    /// Returns the job ID.
    pub fn add(&mut self, command: String, handles: Vec<TaskHandle>) -> usize {
        self.next_id += 1;
        let id = self.next_id;

        let job = Job {
            id,
            command,
            handles,
            state: JobState::Running,
        };

        // Find an empty slot or push
        let mut slot_idx = None;
        for (i, slot) in self.jobs.iter().enumerate() {
            if slot.is_none() {
                slot_idx = Some(i);
                break;
            }
        }

        if let Some(i) = slot_idx {
            self.jobs[i] = Some(job);
        } else {
            self.jobs.push(Some(job));
        }

        id
    }

    /// Get a job by ID.
    pub fn get(&self, id: usize) -> Option<&Job> {
        self.jobs.iter().flatten().find(|j| j.id == id)
    }

    /// Get a mutable job by ID.
    pub fn get_mut(&mut self, id: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().flatten().find(|j| j.id == id)
    }

    /// Remove a job by ID.
    pub fn remove(&mut self, id: usize) -> Option<Job> {
        for slot in &mut self.jobs {
            if slot.as_ref().is_some_and(|j| j.id == id) {
                return slot.take();
            }
        }
        None
    }

    /// Kill a job by ID.
    ///
    /// Cancels all tasks in the job using structured concurrency, then removes it.
    /// Returns true if the job existed and was killed.
    pub fn kill(&mut self, id: usize) -> bool {
        // First kill the tasks
        if let Some(job) = self.get(id) {
            job.kill();
        }
        // Then remove from table
        self.remove(id).is_some()
    }

    /// Iterate over all active jobs.
    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter().flatten()
    }

    /// Update states and reap completed jobs.
    ///
    /// Returns list of (`job_id`, `exit_code`) for completed jobs.
    pub fn reap(&mut self) -> Vec<(usize, i32)> {
        let mut completed = Vec::new();

        for job in self.jobs.iter_mut().flatten() {
            job.update_state();
            if let JobState::Done(code) = job.state {
                completed.push((job.id, code));
            }
        }

        // Remove completed jobs
        for (id, _) in &completed {
            self.remove(*id);
        }

        completed
    }

    /// Number of active jobs.
    pub fn len(&self) -> usize {
        self.jobs.iter().filter(|j| j.is_some()).count()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Basic job table operations
    // =========================================================================

    #[test]
    fn job_table_basic() {
        let mut table = JobTable::new();
        assert!(table.is_empty());

        let id = table.add("sleep 10".into(), vec![]);
        assert_eq!(id, 1);
        assert_eq!(table.len(), 1);

        assert!(table.get(id).is_some());
        assert_eq!(table.get(id).unwrap().command, "sleep 10");
    }

    #[test]
    fn job_table_multiple() {
        let mut table = JobTable::new();

        let id1 = table.add("cmd1".into(), vec![]);
        let id2 = table.add("cmd2".into(), vec![]);
        let id3 = table.add("cmd3".into(), vec![]);

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn job_table_remove() {
        let mut table = JobTable::new();

        let id1 = table.add("cmd1".into(), vec![]);
        let id2 = table.add("cmd2".into(), vec![]);

        table.remove(id1);
        assert_eq!(table.len(), 1);
        assert!(table.get(id1).is_none());
        assert!(table.get(id2).is_some());
    }

    // =========================================================================
    // Job ID management
    // =========================================================================

    #[test]
    fn job_ids_monotonic() {
        let mut table = JobTable::new();

        // IDs should always increase, even after removal
        let id1 = table.add("cmd1".into(), vec![]);
        let id2 = table.add("cmd2".into(), vec![]);
        table.remove(id1);
        let id3 = table.add("cmd3".into(), vec![]);

        assert!(id3 > id2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn slot_reuse() {
        let mut table = JobTable::new();

        // Add some jobs
        let id1 = table.add("cmd1".into(), vec![]);
        let _id2 = table.add("cmd2".into(), vec![]);

        // Remove first job - creates empty slot
        table.remove(id1);
        assert_eq!(table.len(), 1);

        // New job should reuse the slot
        let id3 = table.add("cmd3".into(), vec![]);
        assert_eq!(id3, 3); // ID still increases
        assert_eq!(table.len(), 2);
    }

    // =========================================================================
    // Job state
    // =========================================================================

    #[test]
    fn job_initial_state() {
        let mut table = JobTable::new();
        let id = table.add("cmd".into(), vec![]);

        let job = table.get(id).unwrap();
        assert_eq!(job.state, JobState::Running);
    }

    #[test]
    fn job_state_display() {
        // JobState should implement Debug and be comparable
        assert_eq!(JobState::Running, JobState::Running);
        assert_eq!(JobState::Done(0), JobState::Done(0));
        assert_ne!(JobState::Done(0), JobState::Done(1));
        assert_ne!(JobState::Running, JobState::Done(0));
    }

    // =========================================================================
    // Job completion with no handles
    // =========================================================================

    #[test]
    fn job_no_handles_is_complete() {
        // Job with no handles is immediately "complete"
        let job = Job {
            id: 1,
            command: "empty".into(),
            handles: vec![],
            state: JobState::Running,
        };
        assert!(job.is_complete());
    }

    #[test]
    fn job_no_handles_update_state() {
        let mut job = Job {
            id: 1,
            command: "empty".into(),
            handles: vec![],
            state: JobState::Running,
        };
        job.update_state();
        // Empty job gets exit code 0
        assert_eq!(job.state, JobState::Done(0));
    }

    // =========================================================================
    // Iteration
    // =========================================================================

    #[test]
    fn iter_empty() {
        let table = JobTable::new();
        assert_eq!(table.iter().count(), 0);
    }

    #[test]
    fn iter_all_jobs() {
        let mut table = JobTable::new();
        table.add("cmd1".into(), vec![]);
        table.add("cmd2".into(), vec![]);
        table.add("cmd3".into(), vec![]);

        let jobs: Vec<_> = table.iter().collect();
        assert_eq!(jobs.len(), 3);
    }

    #[test]
    fn iter_skips_removed() {
        let mut table = JobTable::new();
        let id1 = table.add("cmd1".into(), vec![]);
        table.add("cmd2".into(), vec![]);
        table.add("cmd3".into(), vec![]);

        table.remove(id1);

        let jobs: Vec<_> = table.iter().collect();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.id != id1));
    }

    // =========================================================================
    // Get mutable
    // =========================================================================

    #[test]
    fn get_mut_exists() {
        let mut table = JobTable::new();
        let id = table.add("cmd".into(), vec![]);

        let job = table.get_mut(id).unwrap();
        job.state = JobState::Done(42);

        assert_eq!(table.get(id).unwrap().state, JobState::Done(42));
    }

    #[test]
    fn get_mut_not_exists() {
        let mut table = JobTable::new();
        assert!(table.get_mut(999).is_none());
    }

    // =========================================================================
    // Remove edge cases
    // =========================================================================

    #[test]
    fn remove_nonexistent() {
        let mut table = JobTable::new();
        table.add("cmd".into(), vec![]);

        // Should return None and not panic
        assert!(table.remove(999).is_none());
    }

    #[test]
    fn remove_returns_job() {
        let mut table = JobTable::new();
        let id = table.add("cmd".into(), vec![]);

        let removed = table.remove(id);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, id);
    }

    #[test]
    fn remove_twice() {
        let mut table = JobTable::new();
        let id = table.add("cmd".into(), vec![]);

        assert!(table.remove(id).is_some());
        assert!(table.remove(id).is_none()); // Already removed
    }

    // =========================================================================
    // Reap (with empty handles - completion simulation)
    // =========================================================================

    #[test]
    fn reap_empty_table() {
        let mut table = JobTable::new();
        let completed = table.reap();
        assert!(completed.is_empty());
    }

    #[test]
    fn reap_completes_empty_jobs() {
        let mut table = JobTable::new();
        table.add("cmd1".into(), vec![]);
        table.add("cmd2".into(), vec![]);

        // Jobs with no handles complete immediately
        let completed = table.reap();
        assert_eq!(completed.len(), 2);
        assert!(table.is_empty());
    }

    #[test]
    fn reap_returns_ids_and_codes() {
        let mut table = JobTable::new();
        let id1 = table.add("cmd1".into(), vec![]);
        let id2 = table.add("cmd2".into(), vec![]);

        let completed = table.reap();

        // Both should complete with code 0 (empty handles)
        assert!(completed.contains(&(id1, 0)));
        assert!(completed.contains(&(id2, 0)));
    }

    // =========================================================================
    // Length and empty checks
    // =========================================================================

    #[test]
    fn len_after_operations() {
        let mut table = JobTable::new();
        assert_eq!(table.len(), 0);

        let id1 = table.add("cmd1".into(), vec![]);
        assert_eq!(table.len(), 1);

        table.add("cmd2".into(), vec![]);
        assert_eq!(table.len(), 2);

        table.remove(id1);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn is_empty_after_operations() {
        let mut table = JobTable::new();
        assert!(table.is_empty());

        let id = table.add("cmd".into(), vec![]);
        assert!(!table.is_empty());

        table.remove(id);
        assert!(table.is_empty());
    }

    // =========================================================================
    // Job struct
    // =========================================================================

    #[test]
    fn job_fields() {
        let job = Job {
            id: 42,
            command: "test command".into(),
            handles: vec![],
            state: JobState::Running,
        };

        assert_eq!(job.id, 42);
        assert_eq!(job.command, "test command");
        assert!(job.handles.is_empty());
        assert_eq!(job.state, JobState::Running);
    }
}
