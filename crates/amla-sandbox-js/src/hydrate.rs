//! Tool stub generation for JavaScript runtime.
//!
//! This module generates JavaScript stubs from tool definitions that can be
//! injected into the JS runtime. The stubs provide typed wrappers that call
//! `__amla__.toolCall()` for each registered tool.
//!
//! # Example
//!
//! ```rust,ignore
//! use amla_js::hydrate::generate_tool_stubs;
//! use amla_tools::{ToolDef, ParamDef, ParamType};
//!
//! let tools = vec![
//!     ToolDef {
//!         name: "stripe:charge".to_string(),
//!         description: "Create a payment charge".to_string(),
//!         parameters: vec![
//!             ParamDef::new("amount", ParamType::Integer, "Amount in cents", true),
//!             ParamDef::new("currency", ParamType::String, "Currency code", true),
//!         ],
//!         category: Some("payments".to_string()),
//!         keywords: vec![],
//!         embedding: None,
//!     },
//! ];
//!
//! let js_code = generate_tool_stubs(&tools);
//! // Inject into runtime:
//! // runtime.execute(&js_code)?;
//! ```

use std::fmt::Write;

/// Escape a string for use in JavaScript string literals.
///
/// Handles backslashes, quotes, newlines, and other special characters.
fn escape_js_string(s: &str) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            c if c.is_control() => {
                // Use \uXXXX for other control characters
                for unit in c.encode_utf16(&mut [0; 2]) {
                    let _ = write!(result, "\\u{unit:04x}");
                }
            }
            c => result.push(c),
        }
    }
    result
}

/// Tool definition for stub generation.
///
/// This is a simplified version that doesn't require depending on amla-tools.
#[derive(Debug, Clone)]
pub struct ToolStub {
    /// Tool name (e.g., "stripe:charge").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Parameter definitions.
    pub params: Vec<ParamStub>,
}

/// Parameter definition for a tool stub.
#[derive(Debug, Clone)]
pub struct ParamStub {
    /// Parameter name.
    pub name: String,
    /// Whether the parameter is required.
    pub required: bool,
    /// Human-readable description.
    pub description: String,
}

impl ToolStub {
    /// Create a new tool stub.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            params: Vec::new(),
        }
    }

    /// Add a parameter to the tool stub.
    #[must_use]
    pub fn param(
        mut self,
        name: impl Into<String>,
        required: bool,
        description: impl Into<String>,
    ) -> Self {
        self.params.push(ParamStub {
            name: name.into(),
            required,
            description: description.into(),
        });
        self
    }
}

/// Generate JavaScript code that creates tool wrapper functions.
///
/// The generated code creates a `tools` global object with methods for each tool.
/// Each method calls `__amla__.toolCall()` with the appropriate tool name and parameters.
///
/// # Example Output
///
/// ```javascript
/// globalThis.tools = {
///     stripe: {
///         /**
///          * Create a payment charge
///          * @param {Object} params
///          * @param {number} params.amount - Amount in cents (required)
///          * @param {string} params.currency - Currency code (required)
///          * @returns {Promise}
///          */
///         charge: function(params) {
///             return __amla__.toolCall("stripe:charge", params);
///         }
///     }
/// };
/// ```
pub fn generate_tool_stubs(tools: &[ToolStub]) -> String {
    let mut js = String::from("// Auto-generated tool stubs\nglobalThis.tools = {};\n\n");

    // Group tools by namespace (before the colon)
    let mut namespaces: std::collections::HashMap<String, Vec<&ToolStub>> =
        std::collections::HashMap::new();

    for tool in tools {
        let (namespace, _method) = tool.name.split_once(':').unwrap_or(("_", &tool.name));
        namespaces
            .entry(namespace.to_string())
            .or_default()
            .push(tool);
    }

    // Generate code for each namespace
    for (namespace, ns_tools) in &namespaces {
        let ns_escaped = escape_js_string(namespace);
        let _ = writeln!(
            js,
            "globalThis.tools[\"{ns_escaped}\"] = globalThis.tools[\"{ns_escaped}\"] || {{}};"
        );

        for tool in ns_tools {
            let (_ns, method) = tool.name.split_once(':').unwrap_or(("_", &tool.name));
            let method_escaped = escape_js_string(method);
            let name_escaped = escape_js_string(&tool.name);

            // JSDoc comment
            let _ = writeln!(js, "/**");
            let _ = writeln!(js, " * {}", tool.description);
            if !tool.params.is_empty() {
                let _ = writeln!(js, " * @param {{Object}} params");
                for param in &tool.params {
                    let req = if param.required {
                        "required"
                    } else {
                        "optional"
                    };
                    let _ = writeln!(
                        js,
                        " * @param {{*}} params.{} - {} ({})",
                        param.name, param.description, req
                    );
                }
            }
            let _ = writeln!(js, " * @returns {{Promise}}");
            let _ = writeln!(js, " */");

            // Function (using bracket notation for safe property access)
            let _ = writeln!(
                js,
                "globalThis.tools[\"{ns_escaped}\"][\"{method_escaped}\"] = function(params) {{"
            );
            let _ = writeln!(
                js,
                "    return __amla__.toolCall(\"{name_escaped}\", params || {{}});"
            );
            let _ = writeln!(js, "}};");
            let _ = writeln!(js);
        }
    }

    js
}

/// Generate JavaScript code for a single tool (without namespace).
///
/// This is useful for injecting individual tools as needed.
pub fn generate_single_tool_stub(tool: &ToolStub) -> String {
    let mut js = String::new();

    let (namespace, method) = tool.name.split_once(':').unwrap_or(("_", &tool.name));
    let ns_escaped = escape_js_string(namespace);
    let method_escaped = escape_js_string(method);
    let name_escaped = escape_js_string(&tool.name);

    // Ensure namespace exists (using bracket notation for safety)
    let _ = writeln!(js, "globalThis.tools = globalThis.tools || {{}};");
    let _ = writeln!(
        js,
        "globalThis.tools[\"{ns_escaped}\"] = globalThis.tools[\"{ns_escaped}\"] || {{}};"
    );

    // JSDoc comment
    let _ = writeln!(js, "/**");
    let _ = writeln!(js, " * {}", tool.description);
    if !tool.params.is_empty() {
        let _ = writeln!(js, " * @param {{Object}} params");
        for param in &tool.params {
            let req = if param.required {
                "required"
            } else {
                "optional"
            };
            let _ = writeln!(
                js,
                " * @param {{*}} params.{} - {} ({})",
                param.name, param.description, req
            );
        }
    }
    let _ = writeln!(js, " * @returns {{Promise}}");
    let _ = writeln!(js, " */");

    // Function (using bracket notation for safe property access)
    let _ = writeln!(
        js,
        "globalThis.tools[\"{ns_escaped}\"][\"{method_escaped}\"] = function(params) {{"
    );
    let _ = writeln!(
        js,
        "    return __amla__.toolCall(\"{name_escaped}\", params || {{}});"
    );
    let _ = writeln!(js, "}};");

    js
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_single_tool() {
        let tool = ToolStub::new("stripe:charge", "Create a payment charge")
            .param("amount", true, "Amount in cents")
            .param("currency", true, "Currency code");

        let js = generate_single_tool_stub(&tool);

        assert!(js.contains(r#"globalThis.tools["stripe"]["charge"]"#));
        assert!(js.contains(r#"__amla__.toolCall("stripe:charge""#));
        assert!(js.contains("@param {*} params.amount"));
    }

    #[test]
    fn test_generate_multiple_tools() {
        let tools = vec![
            ToolStub::new("stripe:charge", "Create a payment charge"),
            ToolStub::new("stripe:refund", "Refund a payment"),
            ToolStub::new("notion:search", "Search Notion pages"),
        ];

        let js = generate_tool_stubs(&tools);

        assert!(js.contains(r#"globalThis.tools["stripe"]["charge"]"#));
        assert!(js.contains(r#"globalThis.tools["stripe"]["refund"]"#));
        assert!(js.contains(r#"globalThis.tools["notion"]["search"]"#));
    }

    #[test]
    fn test_tool_without_namespace() {
        let tool = ToolStub::new("echo", "Echo a message");
        let js = generate_single_tool_stub(&tool);

        assert!(js.contains(r#"globalThis.tools["_"]["echo"]"#));
        assert!(js.contains(r#"__amla__.toolCall("echo""#));
    }

    #[test]
    fn test_tool_with_special_characters() {
        // Tool names with hyphens, dots, etc. should work
        let tool = ToolStub::new("my-api.v2:do-thing", "Do something");
        let js = generate_single_tool_stub(&tool);

        // Should use bracket notation with escaped strings
        assert!(js.contains(r#"globalThis.tools["my-api.v2"]["do-thing"]"#));
        assert!(js.contains(r#"__amla__.toolCall("my-api.v2:do-thing""#));
    }

    #[test]
    fn test_tool_name_with_quotes() {
        // Quotes in names should be escaped
        let tool = ToolStub::new(r#"test:"quoted""#, "Has quotes");
        let js = generate_single_tool_stub(&tool);

        // Should escape the quotes
        assert!(js.contains(r#"globalThis.tools["test"]["\"quoted\""]"#));
    }

    #[test]
    fn test_escape_js_string() {
        assert_eq!(escape_js_string("hello"), "hello");
        assert_eq!(escape_js_string(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_js_string("back\\slash"), "back\\\\slash");
        assert_eq!(escape_js_string("line\nbreak"), "line\\nbreak");
        assert_eq!(escape_js_string("tab\there"), "tab\\there");
    }

    #[test]
    fn test_escape_js_string_carriage_return() {
        // Test \r escaping (line 45)
        assert_eq!(escape_js_string("carriage\rreturn"), "carriage\\rreturn");
        assert_eq!(escape_js_string("\r\n"), "\\r\\n");
    }

    #[test]
    fn test_escape_js_string_null() {
        // Test \0 escaping (line 47)
        assert_eq!(escape_js_string("null\0char"), "null\\0char");
        assert_eq!(escape_js_string("\0"), "\\0");
    }

    #[test]
    fn test_escape_js_string_control_characters() {
        // Test control character escaping (lines 50-51)
        // Control characters (other than \n, \r, \t, \0) should be escaped as \uXXXX
        assert_eq!(escape_js_string("\x01"), "\\u0001");
        assert_eq!(escape_js_string("\x02"), "\\u0002");
        assert_eq!(escape_js_string("\x1F"), "\\u001f"); // Last control char before space
        assert_eq!(escape_js_string("a\x03b"), "a\\u0003b");
    }

    #[test]
    fn test_escape_js_string_combined() {
        // Test multiple special characters together
        assert_eq!(
            escape_js_string("quote\"back\\new\ntab\tcr\rnull\0ctrl\x01"),
            "quote\\\"back\\\\new\\ntab\\tcr\\rnull\\0ctrl\\u0001"
        );
    }

    #[test]
    fn test_tool_stub_builder() {
        let tool = ToolStub::new("test:tool", "A test tool")
            .param("required_param", true, "A required parameter")
            .param("optional_param", false, "An optional parameter");

        assert_eq!(tool.name, "test:tool");
        assert_eq!(tool.description, "A test tool");
        assert_eq!(tool.params.len(), 2);
        assert!(tool.params[0].required);
        assert!(!tool.params[1].required);
    }

    #[test]
    fn test_empty_tools_list() {
        let tools: Vec<ToolStub> = vec![];
        let js = generate_tool_stubs(&tools);
        // Should still have the initialization
        assert!(js.contains("globalThis.tools"));
    }

    #[test]
    fn test_tool_with_optional_params() {
        let tool = ToolStub::new("api:call", "Make an API call")
            .param("required", true, "Required param")
            .param("optional", false, "Optional param");

        let js = generate_single_tool_stub(&tool);

        assert!(js.contains("(required)"));
        assert!(js.contains("(optional)"));
    }

    #[test]
    fn test_tool_with_no_params() {
        let tool = ToolStub::new("simple:action", "A simple action with no params");
        let js = generate_single_tool_stub(&tool);

        assert!(js.contains("A simple action with no params"));
        assert!(!js.contains("@param {Object} params"));
    }

    #[test]
    fn test_generate_tool_stubs_groups_by_namespace() {
        let tools = vec![
            ToolStub::new("ns1:a", "First in ns1"),
            ToolStub::new("ns1:b", "Second in ns1"),
            ToolStub::new("ns2:x", "First in ns2"),
        ];

        let js = generate_tool_stubs(&tools);

        // Should define both namespaces
        assert!(js.contains(r#"globalThis.tools["ns1"]"#));
        assert!(js.contains(r#"globalThis.tools["ns2"]"#));
        // Should have all methods
        assert!(js.contains(r#"["ns1"]["a"]"#));
        assert!(js.contains(r#"["ns1"]["b"]"#));
        assert!(js.contains(r#"["ns2"]["x"]"#));
    }
}
