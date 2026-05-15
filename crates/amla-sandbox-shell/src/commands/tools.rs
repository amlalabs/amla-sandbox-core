//! tools - search and discover available tools
//!
//! Provides BM25 + semantic search over a tool catalog using Model2Vec embeddings.

use amla_scheduler::Exit;
use amla_tools::{ParamDef, ParamType, ToolCatalog, ToolDef};

use super::CommandResult;
use crate::CmdContext;

/// tools [subcommand] [args...]
///
/// Tool catalog search and discovery.
///
/// Subcommands:
///   search <query>     Search for tools matching query (hybrid BM25 + semantic)
///   list               List all available tools
///   info <name>        Show detailed info about a tool
///   embed <text>       Embed text and show similarity to sample texts
///
/// Options:
///   -n <limit>         Maximum results to return (default: 5)
///   -c <category>      Filter by category
///   --bm25             Use pure BM25 search (no semantic)
///   --semantic         Use pure semantic search (no BM25)
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut limit = 5usize;
    let mut category: Option<String> = None;
    let mut search_mode = SearchMode::Hybrid;
    let mut args: Vec<String> = Vec::new();

    // Parse arguments
    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('n'))) => {
                if let Ok(val) = parser.value() {
                    limit = val.to_string_lossy().parse().unwrap_or(5);
                }
            }
            Ok(Some(lexopt::Arg::Short('c'))) => {
                if let Ok(val) = parser.value() {
                    category = Some(val.to_string_lossy().into_owned());
                }
            }
            Ok(Some(lexopt::Arg::Long("bm25"))) => search_mode = SearchMode::Bm25,
            Ok(Some(lexopt::Arg::Long("semantic"))) => search_mode = SearchMode::Semantic,
            Ok(Some(lexopt::Arg::Value(val))) => {
                args.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Get subcommand
    let subcommand = args.first().map(String::as_str).unwrap_or("help");

    // Get catalog from context (set by runtime) or build demo catalog
    let catalog = ctx
        .tool_catalog()
        .map(|rc| (*rc).clone())
        .unwrap_or_else(build_demo_catalog);
    let catalog = if let Some(cat) = &category {
        catalog.filter_by_category(cat)
    } else {
        catalog
    };

    match subcommand {
        "search" => {
            let query = args.get(1..).map(|a| a.join(" ")).unwrap_or_default();
            if query.is_empty() {
                ctx.write_stderr(b"Usage: tools search <query>\n").await?;
                return Ok(Exit::code(1));
            }
            search_tools(&ctx, &catalog, &query, limit, search_mode).await
        }
        "list" => list_tools(&ctx, &catalog).await,
        "info" => {
            let name = args.get(1).map(String::as_str).unwrap_or("");
            if name.is_empty() {
                ctx.write_stderr(b"Usage: tools info <name>\n").await?;
                return Ok(Exit::code(1));
            }
            tool_info(&ctx, &catalog, name).await
        }
        "embed" => {
            let text = args.get(1..).map(|a| a.join(" ")).unwrap_or_default();
            if text.is_empty() {
                ctx.write_stderr(b"Usage: tools embed <text>\n").await?;
                return Ok(Exit::code(1));
            }
            embed_text(&ctx, &text).await
        }
        _ => {
            let help = b"tools - search and discover available tools

Usage: tools <subcommand> [options]

Subcommands:
  search <query>   Search for tools (hybrid BM25 + semantic)
  list             List all available tools
  info <name>      Show detailed info about a tool
  embed <text>     Embed text and show vector

Options:
  -n <limit>       Maximum results (default: 5)
  -c <category>    Filter by category
  --bm25           Use pure BM25 search
  --semantic       Use pure semantic search
";
            ctx.write_stdout(help).await?;
            Ok(Exit::success())
        }
    }
}

#[derive(Clone, Copy)]
enum SearchMode {
    Hybrid,
    Bm25,
    Semantic,
}

async fn search_tools(
    ctx: &CmdContext,
    catalog: &ToolCatalog,
    query: &str,
    limit: usize,
    mode: SearchMode,
) -> CommandResult {
    let results = match mode {
        SearchMode::Hybrid => catalog.search_smart(query, limit),
        SearchMode::Bm25 => catalog.search_bm25(query, limit),
        SearchMode::Semantic => {
            if let Ok(embedder) = amla_tools::Embedder::global() {
                if let Ok(emb) = embedder.embed(query) {
                    catalog.search_semantic(&emb, limit)
                } else {
                    catalog.search_bm25(query, limit)
                }
            } else {
                catalog.search_bm25(query, limit)
            }
        }
    };

    if results.is_empty() {
        ctx.write_stdout(b"No matching tools found.\n").await?;
    } else {
        for (i, result) in results.iter().enumerate() {
            let line = format!(
                "{}. {} (score: {:.3})\n   {}\n\n",
                i + 1,
                result.tool.name,
                result.score,
                result.tool.description
            );
            ctx.write_stdout(line.as_bytes()).await?;
        }
    }

    Ok(Exit::success())
}

async fn list_tools(ctx: &CmdContext, catalog: &ToolCatalog) -> CommandResult {
    if catalog.is_empty() {
        ctx.write_stdout(b"No tools registered.\n").await?;
    } else {
        for tool in catalog.list() {
            let cat = tool.category.as_deref().unwrap_or("-");
            let line = format!("{:<25} [{}] {}\n", tool.name, cat, tool.description);
            ctx.write_stdout(line.as_bytes()).await?;
        }
    }
    Ok(Exit::success())
}

async fn tool_info(ctx: &CmdContext, catalog: &ToolCatalog, name: &str) -> CommandResult {
    match catalog.get(name) {
        Some(tool) => {
            let mut out = format!("Tool: {}\n", tool.name);
            out.push_str(&format!("Description: {}\n", tool.description));
            if let Some(cat) = &tool.category {
                out.push_str(&format!("Category: {cat}\n"));
            }
            if !tool.keywords.is_empty() {
                out.push_str(&format!("Keywords: {}\n", tool.keywords.join(", ")));
            }
            if !tool.parameters.is_empty() {
                out.push_str("\nParameters:\n");
                for p in &tool.parameters {
                    let req = if p.required { "required" } else { "optional" };
                    out.push_str(&format!(
                        "  {} ({}, {}): {}\n",
                        p.name,
                        format!("{:?}", p.param_type).to_lowercase(),
                        req,
                        p.description
                    ));
                }
            }
            ctx.write_stdout(out.as_bytes()).await?;
            Ok(Exit::success())
        }
        None => {
            let msg = format!("Tool '{name}' not found.\n");
            ctx.write_stderr(msg.as_bytes()).await?;
            Ok(Exit::code(1))
        }
    }
}

async fn embed_text(ctx: &CmdContext, text: &str) -> CommandResult {
    match amla_tools::Embedder::global() {
        Ok(embedder) => match embedder.embed(text) {
            Ok(embedding) => {
                let dim = embedding.len();
                let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                let first_5: Vec<String> = embedding
                    .iter()
                    .take(5)
                    .map(|x| format!("{x:.4}"))
                    .collect();

                let out = format!(
                    "Text: \"{}\"\nDimension: {}\nL2 Norm: {:.4}\nFirst 5 values: [{}]\n",
                    text,
                    dim,
                    norm,
                    first_5.join(", ")
                );
                ctx.write_stdout(out.as_bytes()).await?;

                // Show similarity to some sample concepts
                let samples = ["payment", "search", "file", "database"];
                let mut similarities = String::from("\nSimilarity to concepts:\n");
                for sample in samples {
                    if let Ok(sample_emb) = embedder.embed(sample) {
                        let sim = amla_tools::cosine_similarity(&embedding, &sample_emb);
                        similarities.push_str(&format!("  {sample}: {sim:.3}\n"));
                    }
                }
                ctx.write_stdout(similarities.as_bytes()).await?;

                Ok(Exit::success())
            }
            Err(e) => {
                let msg = format!("Embedding failed: {e}\n");
                ctx.write_stderr(msg.as_bytes()).await?;
                Ok(Exit::code(1))
            }
        },
        Err(e) => {
            let msg = format!("Embedder not available: {e}\n");
            ctx.write_stderr(msg.as_bytes()).await?;
            Ok(Exit::code(1))
        }
    }
}

/// Build a demo tool catalog for testing.
fn build_demo_catalog() -> ToolCatalog {
    let tools = vec![
        ToolDef {
            name: "stripe:charge".to_string(),
            description: "Create a payment charge using Stripe".to_string(),
            parameters: vec![
                ParamDef::new("amount", ParamType::Integer, "Amount in cents", true),
                ParamDef::new(
                    "currency",
                    ParamType::String,
                    "Currency code (USD, EUR)",
                    true,
                ),
                ParamDef::new("customer", ParamType::String, "Customer ID", true),
            ],
            category: Some("payments".to_string()),
            keywords: vec![
                "payment".to_string(),
                "credit card".to_string(),
                "money".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "stripe:refund".to_string(),
            description: "Refund a previous payment charge".to_string(),
            parameters: vec![
                ParamDef::new(
                    "charge_id",
                    ParamType::String,
                    "ID of charge to refund",
                    true,
                ),
                ParamDef::new(
                    "amount",
                    ParamType::Integer,
                    "Amount to refund in cents",
                    false,
                ),
            ],
            category: Some("payments".to_string()),
            keywords: vec!["refund".to_string(), "money back".to_string()],
            embedding: None,
        },
        ToolDef {
            name: "notion:search".to_string(),
            description: "Search Notion pages and databases".to_string(),
            parameters: vec![
                ParamDef::new("query", ParamType::String, "Search query", true),
                ParamDef::new("limit", ParamType::Integer, "Maximum results", false),
            ],
            category: Some("productivity".to_string()),
            keywords: vec![
                "documents".to_string(),
                "notes".to_string(),
                "wiki".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "notion:create_page".to_string(),
            description: "Create a new Notion page".to_string(),
            parameters: vec![
                ParamDef::new(
                    "parent_id",
                    ParamType::String,
                    "Parent page or database ID",
                    true,
                ),
                ParamDef::new("title", ParamType::String, "Page title", true),
                ParamDef::new(
                    "content",
                    ParamType::String,
                    "Page content in markdown",
                    false,
                ),
            ],
            category: Some("productivity".to_string()),
            keywords: vec![
                "create".to_string(),
                "page".to_string(),
                "document".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "github:create_issue".to_string(),
            description: "Create a new GitHub issue".to_string(),
            parameters: vec![
                ParamDef::new("repo", ParamType::String, "Repository (owner/name)", true),
                ParamDef::new("title", ParamType::String, "Issue title", true),
                ParamDef::new("body", ParamType::String, "Issue body", false),
                ParamDef::new("labels", ParamType::Array, "Labels to add", false),
            ],
            category: Some("development".to_string()),
            keywords: vec![
                "bug".to_string(),
                "feature request".to_string(),
                "issue".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "github:create_pr".to_string(),
            description: "Create a GitHub pull request".to_string(),
            parameters: vec![
                ParamDef::new("repo", ParamType::String, "Repository (owner/name)", true),
                ParamDef::new("title", ParamType::String, "PR title", true),
                ParamDef::new("head", ParamType::String, "Branch with changes", true),
                ParamDef::new("base", ParamType::String, "Target branch", true),
            ],
            category: Some("development".to_string()),
            keywords: vec![
                "pull request".to_string(),
                "merge".to_string(),
                "code review".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "postgres:query".to_string(),
            description: "Execute a SQL query on PostgreSQL database".to_string(),
            parameters: vec![
                ParamDef::new("query", ParamType::String, "SQL query to execute", true),
                ParamDef::new("params", ParamType::Array, "Query parameters", false),
            ],
            category: Some("database".to_string()),
            keywords: vec![
                "sql".to_string(),
                "database".to_string(),
                "select".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "s3:upload".to_string(),
            description: "Upload a file to AWS S3".to_string(),
            parameters: vec![
                ParamDef::new("bucket", ParamType::String, "S3 bucket name", true),
                ParamDef::new("key", ParamType::String, "Object key (path)", true),
                ParamDef::new("content", ParamType::String, "File content", true),
            ],
            category: Some("storage".to_string()),
            keywords: vec![
                "file".to_string(),
                "upload".to_string(),
                "cloud".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "slack:send_message".to_string(),
            description: "Send a message to a Slack channel".to_string(),
            parameters: vec![
                ParamDef::new("channel", ParamType::String, "Channel ID or name", true),
                ParamDef::new("text", ParamType::String, "Message text", true),
            ],
            category: Some("communication".to_string()),
            keywords: vec![
                "message".to_string(),
                "chat".to_string(),
                "notify".to_string(),
            ],
            embedding: None,
        },
        ToolDef {
            name: "openai:complete".to_string(),
            description: "Generate text completion using OpenAI".to_string(),
            parameters: vec![
                ParamDef::new("prompt", ParamType::String, "Input prompt", true),
                ParamDef::new("model", ParamType::String, "Model to use", false),
                ParamDef::new("max_tokens", ParamType::Integer, "Maximum tokens", false),
            ],
            category: Some("ai".to_string()),
            keywords: vec!["llm".to_string(), "generate".to_string(), "gpt".to_string()],
            embedding: None,
        },
    ];

    ToolCatalog::from_tools_with_embeddings(tools)
}
