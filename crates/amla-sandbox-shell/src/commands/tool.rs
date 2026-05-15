//! tool - Invoke external tools via shell syntax
//!
//! Provides CLI-style access to MCP/external tools:
//!   tool stripe.charge --amount 500 --currency usd
//!
//! This is an ergonomic alternative to JavaScript tool calls.

use amla_scheduler::Exit;
use amla_tools::{ParamType, ToolDef};
use serde_json::{Map, Value};

use super::CommandResult;
use crate::CmdContext;

/// `tool <name> [--param value]...`
///
/// Invoke an external tool with CLI-style arguments.
///
/// # Syntax
///
/// ```text
/// tool <provider>.<action> [--param value]...
/// tool <provider>.<action> --json '{...}'
/// tool --list [provider]
/// tool --help <name>
/// ```
///
/// # Examples
///
/// ```text
/// tool stripe.charge --amount 500 --currency usd
/// tool slack.send --channel general --message "Hello"
/// tool notion.search --query "roadmap"
/// tool --list stripe
/// tool --help stripe.charge
/// ```
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    if args.is_empty() {
        return show_usage(&ctx).await;
    }

    match args[0].as_str() {
        "--list" | "-l" => list_tools(&ctx, args.get(1).map(String::as_str)).await,
        "--help" | "-h" => {
            if let Some(name) = args.get(1) {
                show_tool_help(&ctx, name).await
            } else {
                show_usage(&ctx).await
            }
        }
        name if name.starts_with('-') => {
            ctx.eprintln(&format!("tool: unknown option: {name}"))
                .await?;
            Ok(Exit::code(1))
        }
        name => invoke_tool(&ctx, name, &args[1..]).await,
    }
}

/// List available tools, optionally filtered by provider.
async fn list_tools(ctx: &CmdContext, provider_filter: Option<&str>) -> CommandResult {
    let catalog = match ctx.tool_catalog() {
        Some(c) => c,
        None => {
            ctx.eprintln("tool: no tools available").await?;
            return Ok(Exit::code(1));
        }
    };

    let mut count = 0;
    for tool in catalog.list() {
        // Normalize name: accept both stripe.charge and stripe:charge
        let display_name = tool.name.replace(':', ".");

        // Filter by provider if specified
        if let Some(filter) = provider_filter {
            let provider = display_name.split('.').next().unwrap_or("");
            if provider != filter {
                continue;
            }
        }

        let line = format!("  {:<30} {}\n", display_name, tool.description);
        ctx.write_stdout(line.as_bytes()).await?;
        count += 1;
    }

    if count == 0 {
        if let Some(filter) = provider_filter {
            ctx.println(&format!("No tools found for provider: {filter}"))
                .await?;
        } else {
            ctx.println("No tools registered.").await?;
        }
    }

    Ok(Exit::success())
}

/// Show detailed help for a specific tool.
async fn show_tool_help(ctx: &CmdContext, name: &str) -> CommandResult {
    let catalog = match ctx.tool_catalog() {
        Some(c) => c,
        None => {
            ctx.eprintln("tool: no tools available").await?;
            return Ok(Exit::code(1));
        }
    };

    // Catalog normalizes names, so lookup works with either format
    match catalog.get(name) {
        Some(tool) => {
            let display_name = tool.name.replace(':', ".");
            let mut out = format!("Tool: {display_name}\n");
            out.push_str(&format!("Description: {}\n", tool.description));

            if !tool.parameters.is_empty() {
                out.push_str("\nParameters:\n");
                for p in &tool.parameters {
                    let req = if p.required { "required" } else { "optional" };
                    let type_str = format!("{:?}", p.param_type).to_lowercase();
                    out.push_str(&format!(
                        "  --{:<20} ({}, {}) {}\n",
                        p.name, type_str, req, p.description
                    ));
                }
            }

            out.push_str(&format!("\nUsage:\n  tool {display_name}"));
            for p in &tool.parameters {
                if p.required {
                    out.push_str(&format!(" --{} <value>", p.name));
                }
            }
            out.push('\n');

            ctx.write_stdout(out.as_bytes()).await?;
            Ok(Exit::success())
        }
        None => {
            ctx.eprintln(&format!("tool: unknown tool: {name}")).await?;
            Ok(Exit::code(1))
        }
    }
}

/// Invoke a tool with the given arguments.
async fn invoke_tool(ctx: &CmdContext, name: &str, args: &[String]) -> CommandResult {
    let catalog = match ctx.tool_catalog() {
        Some(c) => c,
        None => {
            ctx.eprintln("tool: no tools available").await?;
            return Ok(Exit::code(1));
        }
    };

    // Catalog normalizes names, so lookup works with either format
    let tool = match catalog.get(name) {
        Some(t) => t,
        None => {
            ctx.eprintln(&format!("tool: unknown tool: {name}")).await?;
            ctx.eprintln("Use 'tool --list' to see available tools.")
                .await?;
            return Ok(Exit::code(1));
        }
    };

    // Parse arguments into JSON params
    let params = match parse_args(tool, args) {
        Ok(p) => p,
        Err(e) => {
            ctx.eprintln(&format!("tool: {e}")).await?;
            return Ok(Exit::code(1));
        }
    };

    // Validate required parameters
    for p in &tool.parameters {
        if p.required && !params.contains_key(&p.name) {
            ctx.eprintln(&format!("tool: missing required parameter: --{}", p.name))
                .await?;
            return Ok(Exit::code(1));
        }
    }

    // Invoke via host channel (same path as JavaScript toolCall)
    // Add mcp: prefix to match capability patterns (MCP = Model Context Protocol)
    // Use the tool's canonical name from the catalog
    let method_name = if tool.name.contains(':') {
        tool.name.clone()
    } else {
        format!("mcp:{}", tool.name)
    };
    let params_bytes = serde_json::to_vec(&params).unwrap_or_default();
    let result = ctx
        .scheduler()
        .host()
        .custom(method_name.clone(), params_bytes)
        .await;

    match result {
        Ok(result_bytes) => {
            // Parse and pretty-print the result
            match serde_json::from_slice::<Value>(&result_bytes) {
                Ok(value) => {
                    let output = serde_json::to_string_pretty(&value).unwrap_or_default();
                    ctx.println(&output).await?;
                    Ok(Exit::success())
                }
                Err(_) => {
                    // Raw bytes - just output as string
                    let output = String::from_utf8_lossy(&result_bytes);
                    ctx.println(&output).await?;
                    Ok(Exit::success())
                }
            }
        }
        Err(e) => {
            ctx.eprintln(&format!("tool: {} failed: {e}", tool.name))
                .await?;
            Ok(Exit::code(1))
        }
    }
}

/// Parse CLI arguments into JSON parameters.
///
/// Supports:
/// - `--param value` pairs
/// - `--json '{...}'` for raw JSON input
/// - `--flag` for boolean true
/// - Type inference from tool schema
fn parse_args(tool: &ToolDef, args: &[String]) -> Result<Map<String, Value>, String> {
    let mut params = Map::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg == "--json" {
            // Next arg is raw JSON
            i += 1;
            if i >= args.len() {
                return Err("--json requires a value".to_string());
            }
            let json_str = &args[i];
            let value: Value =
                serde_json::from_str(json_str).map_err(|e| format!("invalid JSON: {e}"))?;
            if let Value::Object(obj) = value {
                for (k, v) in obj {
                    params.insert(k, v);
                }
            } else {
                return Err("--json value must be an object".to_string());
            }
            i += 1;
            continue;
        }

        if let Some(param_name) = arg.strip_prefix("--") {
            // Look up parameter type in schema
            let param_def = tool.parameters.iter().find(|p| p.name == param_name);

            // Check if next arg exists and doesn't start with --
            let has_value = i + 1 < args.len() && !args[i + 1].starts_with("--");

            if has_value {
                i += 1;
                let value_str = &args[i];
                let value = parse_value(value_str, param_def.map(|p| &p.param_type));
                params.insert(param_name.to_string(), value);
            } else {
                // No value - treat as boolean flag
                params.insert(param_name.to_string(), Value::Bool(true));
            }
            i += 1;
        } else {
            return Err(format!("unexpected argument: {arg}"));
        }
    }

    Ok(params)
}

/// Parse a string value into JSON, using type hints from schema.
fn parse_value(s: &str, param_type: Option<&ParamType>) -> Value {
    match param_type {
        Some(ParamType::Integer) => s
            .parse::<i64>()
            .map_or_else(|_| Value::String(s.to_string()), Value::from),
        Some(ParamType::Number) => s
            .parse::<f64>()
            .map_or_else(|_| Value::String(s.to_string()), Value::from),
        Some(ParamType::Boolean) => match s.to_lowercase().as_str() {
            "true" | "1" | "yes" => Value::Bool(true),
            "false" | "0" | "no" => Value::Bool(false),
            _ => Value::String(s.to_string()),
        },
        Some(ParamType::Array) => {
            // Try to parse as JSON array, otherwise split by comma
            if let Ok(arr) = serde_json::from_str::<Value>(s) {
                arr
            } else {
                let items: Vec<Value> = s
                    .split(',')
                    .map(|x| Value::String(x.trim().to_string()))
                    .collect();
                Value::Array(items)
            }
        }
        Some(ParamType::Object) => {
            // Try to parse as JSON object
            serde_json::from_str(s).unwrap_or(Value::String(s.to_string()))
        }
        Some(ParamType::String) | None => {
            // Try to infer type from value
            if let Ok(n) = s.parse::<i64>() {
                Value::Number(n.into())
            } else if let Ok(n) = s.parse::<f64>() {
                Value::Number(serde_json::Number::from_f64(n).unwrap_or_else(|| 0.into()))
            } else if s == "true" {
                Value::Bool(true)
            } else if s == "false" {
                Value::Bool(false)
            } else {
                Value::String(s.to_string())
            }
        }
    }
}

async fn show_usage(ctx: &CmdContext) -> CommandResult {
    let help = b"tool - Invoke external tools via shell syntax

Usage:
  tool <name> [--param value]...    Invoke a tool
  tool --list [provider]            List available tools
  tool --help <name>                Show tool details

Examples:
  tool stripe.charge --amount 500 --currency usd
  tool slack.send --channel general --message \"Hello\"
  tool notion.search --query roadmap
  tool --json '{\"amount\": 500}' stripe.charge
  tool --list stripe
";
    ctx.write_stdout(help).await?;
    Ok(Exit::success())
}
