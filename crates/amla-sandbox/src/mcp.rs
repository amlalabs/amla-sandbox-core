//! MCP (Model Context Protocol) tool schema loading.
//!
//! This module provides support for loading tool definitions from MCP-compatible
//! JSON schemas. MCP tools follow this structure:
//!
//! ```json
//! {
//!   "name": "get_weather",
//!   "description": "Get current weather for a city",
//!   "inputSchema": {
//!     "type": "object",
//!     "properties": {
//!       "city": { "type": "string", "description": "City name" }
//!     },
//!     "required": ["city"]
//!   }
//! }
//! ```
//!
//! ## Example
//!
//! ```rust
//! use amla_sandbox::mcp::{McpTool, load_mcp_tools};
//!
//! let json = r#"[
//!   {
//!     "name": "search",
//!     "description": "Search for documents",
//!     "inputSchema": {
//!       "type": "object",
//!       "properties": {
//!         "query": { "type": "string" },
//!         "limit": { "type": "integer" }
//!       },
//!       "required": ["query"]
//!     }
//!   }
//! ]"#;
//!
//! let tools = load_mcp_tools(json).unwrap();
//! assert_eq!(tools[0].name, "search");
//! assert_eq!(tools[0].params.len(), 2);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

use crate::stubs::{ParamMetadata, ToolMetadata};
use amla_capabilities::ToolCallCap;
use amla_constraints::{Constraint, ConstraintSet};

/// Error type for MCP schema parsing.
#[derive(Debug, Error)]
pub enum McpError {
    /// JSON parsing error
    #[error("JSON parse error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// Invalid schema structure
    #[error("Invalid schema: {0}")]
    InvalidSchema(String),
}

/// JSON Schema property definition (subset of JSON Schema spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonSchemaProperty {
    /// Property type (string, integer, number, boolean, array, object)
    #[serde(rename = "type", default)]
    pub prop_type: Option<String>,

    /// Human-readable description
    #[serde(default)]
    pub description: Option<String>,

    /// For enums - allowed values
    #[serde(rename = "enum", default)]
    pub enum_values: Option<Vec<serde_json::Value>>,

    /// Default value
    #[serde(default)]
    pub default: Option<serde_json::Value>,

    /// Minimum value (for numbers/integers)
    #[serde(default)]
    pub minimum: Option<f64>,

    /// Maximum value (for numbers/integers)
    #[serde(default)]
    pub maximum: Option<f64>,

    /// Minimum length (for strings)
    #[serde(rename = "minLength", default)]
    pub min_length: Option<u64>,

    /// Maximum length (for strings)
    #[serde(rename = "maxLength", default)]
    pub max_length: Option<u64>,

    /// Pattern (for strings)
    #[serde(default)]
    pub pattern: Option<String>,

    /// Array items schema
    #[serde(default)]
    pub items: Option<Box<JsonSchemaProperty>>,
}

/// JSON Schema for MCP tool input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSchema {
    /// Must be "object"
    #[serde(rename = "type")]
    pub schema_type: String,

    /// Property definitions
    #[serde(default)]
    pub properties: HashMap<String, JsonSchemaProperty>,

    /// Required property names
    #[serde(default)]
    pub required: Vec<String>,
}

/// MCP tool definition (raw from JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolRaw {
    /// Tool name (unique identifier)
    pub name: String,

    /// Human-readable description
    #[serde(default)]
    pub description: Option<String>,

    /// Input parameter schema
    #[serde(rename = "inputSchema")]
    pub input_schema: InputSchema,

    /// Output schema (optional)
    #[serde(rename = "outputSchema", default)]
    pub output_schema: Option<InputSchema>,
}

/// Parsed MCP tool with extracted metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    /// Tool name (unique identifier)
    pub name: String,

    /// Provider extracted from name (before colon) or "mcp"
    pub provider: String,

    /// Action extracted from name (after colon) or full name
    pub action: String,

    /// Human-readable description
    pub description: String,

    /// Parameter metadata
    pub params: Vec<ParamMetadata>,

    /// Constraints derived from JSON Schema
    pub constraints: Vec<Constraint>,

    /// Raw input schema
    pub input_schema: InputSchema,
}

impl McpTool {
    /// Parse an MCP tool from raw JSON representation.
    pub fn from_raw(raw: McpToolRaw) -> Self {
        // Extract provider:action from name
        let (provider, action) = if let Some(idx) = raw.name.find(':') {
            (raw.name[..idx].to_string(), raw.name[idx + 1..].to_string())
        } else {
            ("mcp".to_string(), raw.name.clone())
        };

        // Convert properties to params
        let mut params = Vec::new();
        let mut constraints = Vec::new();

        for (name, prop) in &raw.input_schema.properties {
            let is_required = raw.input_schema.required.contains(name);

            // Extract type
            let param_type = prop.prop_type.clone().unwrap_or_else(|| "any".to_string());

            // Build description
            let description = prop
                .description
                .clone()
                .unwrap_or_else(|| format!("Parameter: {name}"));

            params.push(ParamMetadata {
                name: name.clone(),
                param_type: param_type.clone(),
                description,
                required: is_required,
            });

            // Generate constraints from schema
            if let Some(min) = prop.minimum {
                constraints.push(Constraint::Ge {
                    param: name.clone(),
                    value: serde_json::json!(min),
                });
            }
            if let Some(max) = prop.maximum {
                constraints.push(Constraint::Le {
                    param: name.clone(),
                    value: serde_json::json!(max),
                });
            }
            if let Some(ref enum_vals) = prop.enum_values {
                constraints.push(Constraint::In {
                    param: name.clone(),
                    values: enum_vals.clone(),
                });
            }
            if is_required {
                constraints.push(Constraint::Exists {
                    param: name.clone(),
                });
            }
        }

        Self {
            name: raw.name,
            provider,
            action,
            description: raw.description.unwrap_or_else(|| "MCP Tool".to_string()),
            params,
            constraints,
            input_schema: raw.input_schema,
        }
    }

    /// Convert to `ToolMetadata` for stub generation.
    pub fn to_tool_metadata(&self) -> ToolMetadata {
        ToolMetadata {
            provider: self.provider.clone(),
            name: self.action.clone(),
            description: self.description.clone(),
            params: self.params.clone(),
            constraints: self.constraints.clone(),
        }
    }

    /// Convert to `ToolCallCap` for capability checking.
    pub fn to_tool_call_cap(&self) -> ToolCallCap {
        ToolCallCap::with_constraints(
            self.name.clone(),
            ConstraintSet::new(self.constraints.clone()),
        )
    }
}

/// Load MCP tools from a JSON string (array of tool definitions).
pub fn load_mcp_tools(json: &str) -> Result<Vec<McpTool>, McpError> {
    let raw_tools: Vec<McpToolRaw> = serde_json::from_str(json)?;
    Ok(raw_tools.into_iter().map(McpTool::from_raw).collect())
}

/// Load a single MCP tool from a JSON string.
pub fn load_mcp_tool(json: &str) -> Result<McpTool, McpError> {
    let raw: McpToolRaw = serde_json::from_str(json)?;
    Ok(McpTool::from_raw(raw))
}

/// Example MCP tools for testing - Notion tools.
pub fn example_notion_tools() -> Vec<McpTool> {
    let json = r#"[
        {
            "name": "notion:search",
            "description": "Search for pages and databases in Notion",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query string"
                    },
                    "filter": {
                        "type": "string",
                        "enum": ["page", "database"],
                        "description": "Filter by object type"
                    },
                    "page_size": {
                        "type": "integer",
                        "description": "Number of results to return",
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "notion:get_page",
            "description": "Retrieve a page by ID",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page_id": {
                        "type": "string",
                        "description": "The UUID of the page"
                    }
                },
                "required": ["page_id"]
            }
        },
        {
            "name": "notion:create_page",
            "description": "Create a new page in Notion",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "parent_id": {
                        "type": "string",
                        "description": "Parent page or database ID"
                    },
                    "title": {
                        "type": "string",
                        "description": "Page title"
                    },
                    "content": {
                        "type": "string",
                        "description": "Page content in markdown"
                    }
                },
                "required": ["parent_id", "title"]
            }
        }
    ]"#;

    load_mcp_tools(json).expect("Example tools should parse")
}

/// Example MCP tools - Stripe payment tools.
pub fn example_stripe_tools() -> Vec<McpTool> {
    let json = r#"[
        {
            "name": "stripe:create_charge",
            "description": "Create a new charge on a customer's card",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": {
                        "type": "integer",
                        "description": "Amount in cents",
                        "minimum": 50,
                        "maximum": 99999999
                    },
                    "currency": {
                        "type": "string",
                        "enum": ["usd", "eur", "gbp"],
                        "description": "Three-letter ISO currency code"
                    },
                    "customer": {
                        "type": "string",
                        "description": "Customer ID"
                    },
                    "description": {
                        "type": "string",
                        "description": "Charge description"
                    }
                },
                "required": ["amount", "currency", "customer"]
            }
        },
        {
            "name": "stripe:create_refund",
            "description": "Create a refund for a charge",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "charge": {
                        "type": "string",
                        "description": "Charge ID to refund"
                    },
                    "amount": {
                        "type": "integer",
                        "description": "Amount to refund in cents (optional, defaults to full refund)",
                        "minimum": 1
                    },
                    "reason": {
                        "type": "string",
                        "enum": ["duplicate", "fraudulent", "requested_by_customer"],
                        "description": "Reason for refund"
                    }
                },
                "required": ["charge"]
            }
        }
    ]"#;

    load_mcp_tools(json).expect("Example tools should parse")
}

/// Example MCP tools - GitHub tools.
pub fn example_github_tools() -> Vec<McpTool> {
    let json = r#"[
        {
            "name": "github:create_issue",
            "description": "Create a new issue in a repository",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "owner": {
                        "type": "string",
                        "description": "Repository owner"
                    },
                    "repo": {
                        "type": "string",
                        "description": "Repository name"
                    },
                    "title": {
                        "type": "string",
                        "description": "Issue title"
                    },
                    "body": {
                        "type": "string",
                        "description": "Issue body content"
                    },
                    "labels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Labels to apply"
                    }
                },
                "required": ["owner", "repo", "title"]
            }
        },
        {
            "name": "github:list_repos",
            "description": "List repositories for a user or organization",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "owner": {
                        "type": "string",
                        "description": "User or organization name"
                    },
                    "type": {
                        "type": "string",
                        "enum": ["all", "public", "private", "forks", "sources"],
                        "description": "Type filter"
                    },
                    "per_page": {
                        "type": "integer",
                        "description": "Results per page",
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["owner"]
            }
        }
    ]"#;

    load_mcp_tools(json).expect("Example tools should parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mcp_tool() {
        let json = r#"{
            "name": "test:hello",
            "description": "Say hello",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name to greet"
                    }
                },
                "required": ["name"]
            }
        }"#;

        let tool = load_mcp_tool(json).unwrap();
        assert_eq!(tool.name, "test:hello");
        assert_eq!(tool.provider, "test");
        assert_eq!(tool.action, "hello");
        assert_eq!(tool.description, "Say hello");
        assert_eq!(tool.params.len(), 1);
        assert!(tool.params[0].required);
    }

    #[test]
    fn test_parse_mcp_tools_array() {
        let json = r#"[
            {"name": "a:one", "inputSchema": {"type": "object"}},
            {"name": "b:two", "inputSchema": {"type": "object"}}
        ]"#;

        let tools = load_mcp_tools(json).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "a:one");
        assert_eq!(tools[1].name, "b:two");
    }

    #[test]
    fn test_constraints_from_schema() {
        let json = r#"{
            "name": "test:bounded",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "active", "done"]
                    }
                },
                "required": ["count"]
            }
        }"#;

        let tool = load_mcp_tool(json).unwrap();

        // Should have constraints for: count >= 1, count <= 100, count exists, status in [...]
        assert!(!tool.constraints.is_empty());

        // Convert to capability and test
        let cap = tool.to_tool_call_cap();
        assert_eq!(cap.tool, "test:bounded");

        // Valid params
        assert!(
            cap.check(&serde_json::json!({"count": 50, "status": "active"}))
                .is_ok()
        );

        // Invalid - count too low
        assert!(
            cap.check(&serde_json::json!({"count": 0, "status": "active"}))
                .is_err()
        );

        // Invalid - count too high
        assert!(
            cap.check(&serde_json::json!({"count": 200, "status": "active"}))
                .is_err()
        );

        // Invalid - bad enum value
        assert!(
            cap.check(&serde_json::json!({"count": 50, "status": "invalid"}))
                .is_err()
        );
    }

    #[test]
    fn test_to_tool_metadata() {
        let json = r#"{
            "name": "stripe:charge",
            "description": "Charge a card",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": {"type": "integer", "description": "Amount in cents"},
                    "currency": {"type": "string"}
                },
                "required": ["amount", "currency"]
            }
        }"#;

        let tool = load_mcp_tool(json).unwrap();
        let metadata = tool.to_tool_metadata();

        assert_eq!(metadata.provider, "stripe");
        assert_eq!(metadata.name, "charge");
        assert_eq!(metadata.description, "Charge a card");
        assert_eq!(metadata.params.len(), 2);
    }

    #[test]
    fn test_example_notion_tools() {
        let tools = example_notion_tools();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "notion:search");
        assert_eq!(tools[1].name, "notion:get_page");
        assert_eq!(tools[2].name, "notion:create_page");
    }

    #[test]
    fn test_example_stripe_tools() {
        let tools = example_stripe_tools();
        assert_eq!(tools.len(), 2);

        // Check create_charge has proper constraints
        let charge = &tools[0];
        assert_eq!(charge.name, "stripe:create_charge");

        let cap = charge.to_tool_call_cap();

        // Valid charge
        assert!(
            cap.check(&serde_json::json!({
                "amount": 5000,
                "currency": "usd",
                "customer": "cus_123"
            }))
            .is_ok()
        );

        // Invalid - amount too low (min 50)
        assert!(
            cap.check(&serde_json::json!({
                "amount": 10,
                "currency": "usd",
                "customer": "cus_123"
            }))
            .is_err()
        );
    }

    #[test]
    fn test_example_github_tools() {
        let tools = example_github_tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "github:create_issue");
        assert_eq!(tools[1].name, "github:list_repos");
    }

    #[test]
    fn test_tool_without_provider() {
        let json = r#"{
            "name": "simple_tool",
            "inputSchema": {"type": "object"}
        }"#;

        let tool = load_mcp_tool(json).unwrap();
        assert_eq!(tool.provider, "mcp");
        assert_eq!(tool.action, "simple_tool");
    }

    /// Test with real MCP filesystem server tool definitions.
    /// Source: <https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem>
    #[test]
    fn test_real_mcp_filesystem_tools() {
        let json = r#"[
            {
                "name": "filesystem:read_file",
                "description": "Read the complete contents of a file from the file system.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read"
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "filesystem:write_file",
                "description": "Create a new file or completely overwrite an existing file.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path where the file should be written"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "filesystem:list_directory",
                "description": "Get a detailed listing of all files and directories.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the directory to list"
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "filesystem:search_files",
                "description": "Recursively search for files matching a pattern.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Starting path for the search"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob-style search pattern (e.g., *.ts)"
                        },
                        "excludePatterns": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Patterns to exclude"
                        }
                    },
                    "required": ["path", "pattern"]
                }
            }
        ]"#;

        let tools = load_mcp_tools(json).unwrap();
        assert_eq!(tools.len(), 4);

        // Verify structure
        assert_eq!(tools[0].name, "filesystem:read_file");
        assert_eq!(tools[0].provider, "filesystem");
        assert_eq!(tools[0].action, "read_file");
        assert_eq!(tools[0].params.len(), 1);
        assert!(tools[0].params[0].required);

        // write_file has 2 required params
        assert_eq!(tools[1].params.len(), 2);
        assert!(tools[1].params.iter().all(|p| p.required));

        // search_files has 2 required + 1 optional
        assert_eq!(tools[3].params.len(), 3);
        let optional_count = tools[3].params.iter().filter(|p| !p.required).count();
        assert_eq!(optional_count, 1);
    }

    /// Test interface generation with real MCP tools.
    #[test]
    fn test_interface_generation_with_real_tools() {
        use crate::ToolStubGenerator;
        use amla_vfs::Vfs;

        let json = r#"[
            {
                "name": "github:create_pull_request",
                "description": "Create a new pull request in a GitHub repository",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "owner": {
                            "type": "string",
                            "description": "Repository owner"
                        },
                        "repo": {
                            "type": "string",
                            "description": "Repository name"
                        },
                        "title": {
                            "type": "string",
                            "description": "Pull request title"
                        },
                        "body": {
                            "type": "string",
                            "description": "Pull request body content"
                        },
                        "head": {
                            "type": "string",
                            "description": "Branch with your changes"
                        },
                        "base": {
                            "type": "string",
                            "description": "Branch to merge into"
                        },
                        "draft": {
                            "type": "boolean",
                            "description": "Create as draft PR"
                        }
                    },
                    "required": ["owner", "repo", "title", "head", "base"]
                }
            }
        ]"#;

        let tools = load_mcp_tools(json).unwrap();
        let mut vfs = Vfs::new();

        ToolStubGenerator::generate_from_mcp(&mut vfs, &tools);

        // Check files were created
        assert!(vfs.is_file("/tools/github/create_pull_request.js"));
        assert!(vfs.is_file("/tools/github/create_pull_request.d.ts"));
        assert!(vfs.is_file("/tools/github/create_pull_request.md"));

        // Verify JS content has proper documentation
        let js = vfs
            .read_file_string("/tools/github/create_pull_request.js")
            .unwrap();
        assert!(js.contains("Create a new pull request"));
        assert!(js.contains("owner: string"));
        assert!(js.contains("head: string"));
        assert!(js.contains("draft: boolean"));
        assert!(js.contains("__amla__.toolCall"));
        assert!(js.contains("github:create_pull_request"));

        // Verify TypeScript has proper interface
        let dts = vfs
            .read_file_string("/tools/github/create_pull_request.d.ts")
            .unwrap();
        assert!(dts.contains("export interface CreatePullRequestParams"));
        assert!(dts.contains("owner: string;"));
        assert!(dts.contains("draft?: boolean;")); // Optional

        // Verify README has markdown table
        let readme = vfs
            .read_file_string("/tools/github/create_pull_request.md")
            .unwrap();
        assert!(readme.contains("| Name | Type | Required | Description |"));
        assert!(readme.contains("| owner | string | Yes |"));
        assert!(readme.contains("| draft | boolean | No |"));
    }
}
