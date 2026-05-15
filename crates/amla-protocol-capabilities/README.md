# amla-capabilities

Capability types for AI agent sandboxing.

## Overview

`amla-capabilities` provides concrete capability types for the amla runtime:

- **ToolCallCap** - Call tools with parameter constraints
- **MemoryReadCap** - Read from partitioned storage
- **MemoryWriteCap** - Write to partitioned storage with size limits
- **MemoryDeleteCap** - Delete from partitioned storage
- **SpawnCap** - Spawn child sessions with attenuated capabilities

## Partition Patterns

Memory capabilities use `PartitionPattern` for path-based access control:

```rust
use amla_capabilities::PartitionPattern;

// Recursive - matches all descendants
let pattern = PartitionPattern::new("tenant/alice/**");
assert!(pattern.matches("tenant/alice/foo"));
assert!(pattern.matches("tenant/alice/foo/bar/baz"));

// Non-recursive - only direct children
let pattern = PartitionPattern::new("tenant/alice/*");
assert!(pattern.matches("tenant/alice/foo"));
assert!(!pattern.matches("tenant/alice/foo/bar"));

// Exact match
let pattern = PartitionPattern::new("tenant/alice");
assert!(pattern.matches("tenant/alice"));
assert!(!pattern.matches("tenant/alice/foo"));
```

## Tool Call Constraints

Tool calls integrate with `amla-constraints` for parameter validation:

```rust
use amla_capabilities::ToolCallCap;
use amla_constraints::{Constraint, ConstraintSet};
use serde_json::json;

let cap = ToolCallCap::with_constraints(
    "stripe:charge",
    ConstraintSet::with_constraints(vec![
        Constraint::Ge { param: "amount".into(), value: json!(100) },
        Constraint::Le { param: "amount".into(), value: json!(10000) },
    ]),
);

// Valid parameters
assert!(cap.check(&json!({"amount": 500})).is_ok());

// Invalid - exceeds limit
assert!(cap.check(&json!({"amount": 50000})).is_err());
```

## Capability Set

Collect capabilities for a session:

```rust
use amla_capabilities::{CapabilitySet, ToolCallCap, MemoryReadCap, MemoryWriteCap};
use serde_json::json;

let caps = CapabilitySet::new()
    .add_tool_call(ToolCallCap::new("stripe:charge"))
    .add_memory_read(MemoryReadCap::new("user/**"))
    .add_memory_write(MemoryWriteCap::with_max_bytes("user/alice/**", 1024));

// Check permissions
caps.check_tool_call("stripe:charge", &json!({}))?;
caps.check_memory_read("user/alice/prefs")?;
caps.check_memory_write("user/alice/data", 500)?;
```

## Attenuation (Delegation)

Capabilities support the `is_superset_of` check for attenuation:

```rust
use amla_capabilities::{ToolCallCap, Capability};
use amla_constraints::{Constraint, ConstraintSet};
use serde_json::json;

let parent = ToolCallCap::with_constraints(
    "stripe:charge",
    ConstraintSet::with_constraints(vec![
        Constraint::Le { param: "amount".into(), value: json!(10000) },
    ]),
);

// Child with stricter constraint - valid attenuation
let child = ToolCallCap::with_constraints(
    "stripe:charge",
    ConstraintSet::with_constraints(vec![
        Constraint::Le { param: "amount".into(), value: json!(5000) },
    ]),
);
assert!(parent.is_superset_of(&child));

// Child with looser constraint - invalid attenuation
let child = ToolCallCap::with_constraints(
    "stripe:charge",
    ConstraintSet::with_constraints(vec![
        Constraint::Le { param: "amount".into(), value: json!(20000) },
    ]),
);
assert!(!parent.is_superset_of(&child));
```

## License

See the repository root for license information.
