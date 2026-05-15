# amla-protocol

Rust implementation of the Amla capability protocol for secure agent-tool communication.

## Crates

| Crate | Description |
|-------|-------------|
| `amla-protocol` | CBOR message protocol for PCA tokens and capability chains |
| `amla-capabilities` | Capability definitions (`ToolCallCap`, pattern matching, subsumption) |
| `amla-constraints` | Constraint expression language for parameter validation |

## Features

- **PCA (Permanent Capability Authority)** tokens with Ed25519 signatures
- **Capability chaining** with cryptographic validation
- **Pattern matching** for tool method patterns (`stripe/**`, `api/v1/*`)
- **Constraint expressions** for parameter validation (`amount <= 1000`)
- **Subsumption checking** to verify capability attenuation

## Installation

```toml
[dependencies]
amla-protocol = { git = "https://github.com/amlalabs/amla-protocol" }
```

## Quick Start

```rust
use amla_protocol::{KeyPair, PcaBuilder, Pca};
use amla_capabilities::ToolCallCap;
use amla_constraints::{Constraint, Param};

// Create a root authority
let root = KeyPair::generate();

// Build a PCA with capabilities
let pca = PcaBuilder::new()
    .add_capability(ToolCallCap::new("stripe/charges/*"))
    .add_capability(
        ToolCallCap::new("api/**")
            .with_constraint(Param("amount").le(1000))
            .with_max_calls(100)
    )
    .sign(&root)?;

// Serialize for transmission
let bytes = pca.to_cbor()?;
```

## Security Model

The protocol implements capability-based security:

1. **Authority** - Root key pair that signs PCAs
2. **PCA** - Contains granted capabilities, signed by authority
3. **Capabilities** - Define what methods can be called with what constraints
4. **Attenuation** - Capabilities can only be narrowed, never expanded

## License

AGPL-3.0-or-later OR BUSL-1.1
