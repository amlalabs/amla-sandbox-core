# amla-audit

Structured audit logging for AI agent observability.

## Overview

`amla-audit` provides a lightweight, structured logging system designed for observability of AI agent operations. All log entries are serializable to JSONL format for easy ingestion by log aggregation systems.

## Features

- **Structured entries**: Typed log entries with consistent schema
- **JSONL output**: One JSON object per line for streaming
- **Session tracking**: All entries tied to a session ID
- **Hashing for privacy**: Sensitive data can be hashed before logging
- **Extensible**: Custom events for application-specific logging

## Log Entry Types

| Type | Description |
|------|-------------|
| `SessionStart` | Session created with initial capabilities |
| `SessionEnd` | Session terminated |
| `Shell` | Shell command executed |
| `JsStart` | JavaScript execution started |
| `JsEnd` | JavaScript execution completed |
| `ToolCall` | Tool invocation (with params hash) |
| `MemoryRead` | Memory/state read operation |
| `MemoryWrite` | Memory/state write operation |
| `MemoryDelete` | Memory/state delete operation |
| `Spawn` | Child session spawned |
| `ConstraintViolation` | Authorization constraint violated |
| `FileRead` | VFS file read |
| `FileWrite` | VFS file write |
| `Custom` | User-defined event |

## Usage

```rust
use amla_audit::{AuditLog, LogEntry};

// Create a new audit log
let mut log = AuditLog::new();

// Log session lifecycle
log.log(LogEntry::session_start("sess_123", vec!["tool-call".to_string()]));

// Log operations
log.log(LogEntry::tool_call(
    "sess_123",
    "stripe.charge",
    &serde_json::json!({"amount": 5000}),
    true,
    None,
));

log.log(LogEntry::shell("sess_123", "ls", vec!["/tools".to_string()], 0));

log.log(LogEntry::session_end("sess_123"));

// Get JSONL output for streaming to log aggregator
let jsonl = log.to_jsonl();
println!("{jsonl}");
```

Output:

```jsonl
{"type":"session_start","session_id":"sess_123","timestamp":"2024-...","capabilities":["tool-call"]}
{"type":"tool_call","session_id":"sess_123","timestamp":"2024-...","tool":"stripe.charge","params_hash":"a1b2c3...","success":true,"error":null}
{"type":"shell","session_id":"sess_123","timestamp":"2024-...","command":"ls","args":["/tools"],"exit_code":0}
{"type":"session_end","session_id":"sess_123","timestamp":"2024-..."}
```

## Privacy

Tool call parameters are hashed by default to avoid logging sensitive data:

```rust
// Params are hashed, not stored in plaintext
let entry = LogEntry::tool_call(
    "sess_123",
    "stripe.charge",
    &serde_json::json!({"card_number": "4242..."}),
    true,
    None,
);
// Produces: {"params_hash":"8f3a...","..."}
```

## Filtering

```rust
// Filter entries by session
let sess_entries = log.filter_by_session("sess_123");

// Take and clear entries (for periodic flushing)
let entries = log.take();
assert!(log.is_empty());
```

## Custom Events

For application-specific logging:

```rust
log.log(LogEntry::custom(
    "sess_123",
    "user_action",
    serde_json::json!({
        "action": "button_click",
        "element": "submit_form"
    }),
));
```

## License

See the repository root for license information.
