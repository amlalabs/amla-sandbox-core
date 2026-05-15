# amla-tools

Tool catalog with semantic search for AI agent sandboxes.

## Why Semantic Search

Agents may have access to hundreds of tools. Finding the right one by exact name is brittle:

```
Agent: "I need to send an email"
Tools: ["sendgrid:send_email", "mailchimp:send_campaign", "ses:send_raw_email", ...]
```

Semantic search matches intent to tool:

```rust
let catalog = ToolCatalog::new();
catalog.add_tool(Tool {
    name: "sendgrid:send_email",
    description: "Send an email via SendGrid API",
    // ...
});

let results = catalog.search("notify customer about order shipped");
// Returns sendgrid:send_email ranked by semantic similarity
```

## Embedded Model

The catalog embeds a Model2Vec model (~8MB) for local inference:

| Component | Size |
|-----------|------|
| `model.safetensors` | 7.3 MB |
| `tokenizer.json` | 668 KB |

**No external API calls.** Embeddings computed locally in WASM.

This adds ~8MB to the runtime binary but enables:

- Offline operation
- Low latency (<10ms per query)
- No API keys or network dependency
- Deterministic results

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        ToolCatalog                              │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                      Embedder                              │  │
│  │  ┌─────────────────┐  ┌─────────────────────────────────┐ │  │
│  │  │   Tokenizer     │  │      Model2Vec Weights          │ │  │
│  │  │  (BPE vocab)    │  │  (potion-base-2M, 256 dims)     │ │  │
│  │  └────────┬────────┘  └───────────────┬─────────────────┘ │  │
│  │           │                           │                    │  │
│  │           ▼                           ▼                    │  │
│  │  ┌────────────────────────────────────────────────────┐   │  │
│  │  │              embed("send email to customer")        │   │  │
│  │  │                      → [0.12, -0.45, 0.78, ...]     │   │  │
│  │  └────────────────────────────────────────────────────┘   │  │
│  └───────────────────────────────────────────────────────────┘  │
│                              │                                   │
│  ┌───────────────────────────┴───────────────────────────────┐  │
│  │                      Tool Index                            │  │
│  │  tool_id → (name, description, embedding, schema)         │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## Usage

### Creating a Catalog

```rust
use amla_tools::{ToolCatalog, Tool, ToolSchema};

let mut catalog = ToolCatalog::new();

catalog.add_tool(Tool {
    provider: "stripe".into(),
    name: "charge".into(),
    description: "Create a charge on a credit card".into(),
    schema: ToolSchema {
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "amount": { "type": "integer", "description": "Amount in cents" },
                "currency": { "type": "string", "default": "usd" },
                "source": { "type": "string", "description": "Card token" }
            },
            "required": ["amount", "source"]
        }),
    },
});
```

### Searching

```rust
// Semantic search
let results = catalog.search("bill customer for subscription");

for result in results.iter().take(5) {
    println!("{}: {} (score: {:.3})",
        result.tool.name,
        result.tool.description,
        result.score
    );
}

// Exact name lookup
let tool = catalog.get("stripe", "charge");
```

### From MCP Definitions

```rust
use amla_tools::ToolCatalog;

let mcp_tools = r#"[
    {
        "name": "sendgrid:send_email",
        "description": "Send an email",
        "inputSchema": {...}
    }
]"#;

let catalog = ToolCatalog::from_mcp_json(mcp_tools)?;
```

## Embedding Process

1. **Tokenize**: Split text into BPE tokens using the vocabulary
2. **Lookup**: Get token embeddings from the model weights
3. **Pool**: Average token embeddings to get document embedding
4. **Normalize**: L2 normalize for cosine similarity

```rust
// Simplified flow
fn embed(&self, text: &str) -> Vec<f32> {
    let tokens = self.tokenizer.encode(text);
    let embeddings: Vec<Vec<f32>> = tokens.iter()
        .map(|t| self.weights.get_embedding(*t))
        .collect();

    let pooled = mean_pool(embeddings);
    l2_normalize(pooled)
}
```

Search computes cosine similarity between query embedding and all tool embeddings.

## Performance

On typical hardware:

| Operation | Time |
|-----------|------|
| Embed query | ~2ms |
| Search 100 tools | ~5ms |
| Search 1000 tools | ~15ms |

The embedded model is optimized for speed:

- Small vocabulary (32K tokens)
- Low-dimensional embeddings (256 dims)
- Static quantization (f32)

## Tool Schema

Tools follow MCP (Model Context Protocol) schema:

```rust
pub struct Tool {
    pub provider: String,      // e.g., "stripe", "github"
    pub name: String,          // e.g., "charge", "create_issue"
    pub description: String,   // Natural language description
    pub schema: ToolSchema,    // JSON Schema for parameters
}

pub struct ToolSchema {
    pub parameters: Value,     // JSON Schema object
}
```

## VFS Integration

The catalog can generate tool stubs for the VFS:

```rust
// Generates files like:
// /tools/stripe/charge.js      - JavaScript stub with JSDoc
// /tools/stripe/charge.d.ts    - TypeScript definitions
// /tools/stripe/charge.md      - README

let stubs = catalog.generate_stubs("stripe", "charge");
for (path, content) in stubs {
    vfs.insert_file(&path, content.as_bytes(), Permission::ReadOnly)?;
}
```

Generated JavaScript stub:

```javascript
/**
 * Create a charge on a credit card
 *
 * @param {Object} params
 * @param {number} params.amount - Amount in cents
 * @param {string} params.currency - Currency code (default: "usd")
 * @param {string} params.source - Card token
 * @returns {Promise<Object>} Charge result
 */
export async function charge(params) {
    return await __amla__.toolCall("stripe:charge", params);
}
```

## Feature Flags

```toml
[features]
default = ["tokenizers"]
tokenizers = ["tokenizers-dep", "safetensors"]  # Enables semantic search
```

Without `tokenizers`, only exact name lookup is available. Binary shrinks by ~8MB.

## Building

```bash
# With semantic search (default)
cargo build -p amla-tools

# Without semantic search (smaller binary)
cargo build -p amla-tools --no-default-features

# Run tests
cargo test -p amla-tools
```

## Model Details

**potion-base-2M**: A Model2Vec distillation of sentence-transformers.

- Vocabulary: 32,000 BPE tokens
- Embedding dimensions: 256
- Training: Distilled from `all-MiniLM-L6-v2`
- License: MIT

The model is vendored in `models/potion-base-2M/`.

## License

AGPL-3.0-or-later OR BUSL-1.1
