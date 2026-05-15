# amla-constraints

Constraint system for capability enforcement with fail-closed semantics.

## Design Principles

- **Attenuation is conjunction**: Attenuating adds new clauses AND-ed to existing ones
- **Or allowed within clauses**: Individual constraints can use `Or` for flexibility
- **No regex**: Regex containment is undecidable; use `StartsWith`, `EndsWith`, `Contains` instead
- **Fail-closed**: Missing parameters cause constraint failure

> **Future**: Regex support could be added since AND-ing regexes together preserves decidability.

## Constraint Types

| Type | Syntax | Description |
|------|--------|-------------|
| `Lt` | `x < 100` | Less than |
| `Le` | `x <= 100` | Less than or equal |
| `Gt` | `x > 0` | Greater than |
| `Ge` | `x >= 1` | Greater than or equal |
| `Eq` | `x == "USD"` | Equal |
| `Ne` | `x != "deleted"` | Not equal |
| `In` | `x in ["USD", "EUR"]` | Set membership |
| `NotIn` | `x not in ["DELETE"]` | Set exclusion |
| `StartsWith` | `x starts with "/api/"` | String prefix |
| `EndsWith` | `x ends with ".json"` | String suffix |
| `Contains` | `x contains "SELECT"` | String contains |
| `Exists` | `x exists` | Parameter must exist |
| `NotExists` | `x not exists` | Parameter must not exist |
| `And` | `(a AND b)` | All sub-constraints must pass |
| `Or` | `(a OR b)` | At least one sub-constraint must pass |

## Usage

```rust
use amla_constraints::{Constraint, ConstraintSet};
use serde_json::json;

// Create a constraint set (implicit AND)
let constraints = ConstraintSet::new(vec![
    Constraint::Ge { param: "amount".into(), value: json!(100) },
    Constraint::Le { param: "amount".into(), value: json!(10000) },
    Constraint::In {
        param: "currency".into(),
        values: vec![json!("USD"), json!("EUR")]
    },
]);

// Evaluate against parameters
let params = json!({"amount": 500, "currency": "USD"});
assert!(constraints.evaluate(&params).is_ok());

// Fails: amount too small
let bad = json!({"amount": 50, "currency": "USD"});
assert!(constraints.evaluate(&bad).is_err());
```

## Attenuation (Subsumption)

Capabilities can be attenuated (restricted) but never expanded. The `subsumes` method validates this:

```rust
use amla_constraints::{Constraint, ConstraintSet};
use serde_json::json;

let parent = ConstraintSet::new(vec![
    Constraint::Le { param: "amount".into(), value: json!(100) },
]);

// Valid: child is more restrictive (50 <= 100)
let child = ConstraintSet::new(vec![
    Constraint::Le { param: "amount".into(), value: json!(50) },
]);
assert!(parent.subsumes(&child));

// Invalid: child is less restrictive (200 > 100)
let bad_child = ConstraintSet::new(vec![
    Constraint::Le { param: "amount".into(), value: json!(200) },
]);
assert!(!parent.subsumes(&bad_child));

// Valid: child can add new constraints
let child_with_extra = ConstraintSet::new(vec![
    Constraint::Le { param: "amount".into(), value: json!(50) },
    Constraint::Eq { param: "currency".into(), value: json!("USD") },
]);
assert!(parent.subsumes(&child_with_extra));
```

## Why No Disjunction?

Attenuation checking for capabilities requires determining if one constraint is a subset of another. With disjunction (`Or`), this becomes the satisfiability problem, which is NP-complete. By restricting to conjunction-only, attenuation checking is polynomial.

## License

See the repository root for license information.
