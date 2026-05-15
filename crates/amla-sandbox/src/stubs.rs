//! Tool stub generation for the VFS.
//!
//! Generates `/tools/<provider>/<tool>.js`, `.d.ts`, and `README.md` files
//! from capabilities, allowing LLMs to discover tools via shell commands.
//!
//! ## Example with MCP Tools
//!
//! ```rust
//! use amla_sandbox::mcp::example_notion_tools;
//! use amla_sandbox::ToolStubGenerator;
//! use amla_vfs::Vfs;
//!
//! let mut vfs = Vfs::new();
//!
//! // Generate stubs from MCP tool definitions
//! let tools = example_notion_tools();
//! ToolStubGenerator::generate_from_mcp(&mut vfs, &tools);
//!
//! // Now the VFS has /tools/notion/search.js, etc.
//! assert!(vfs.is_file("/tools/notion/search.js"));
//! ```

use amla_capabilities::ToolCallCap;
use amla_constraints::Constraint;
use amla_vfs::{Permission, Vfs};
use serde::{Deserialize, Serialize};

use crate::mcp::McpTool;

/// Tool metadata for stub generation.
///
/// Contains all the information needed to generate JavaScript stubs,
/// TypeScript definitions, and documentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Provider namespace (e.g., "stripe", "notion", "github")
    pub provider: String,
    /// Tool action name (e.g., "charge", "search", "`create_issue`")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Parameter metadata
    pub params: Vec<ParamMetadata>,
    /// Constraints derived from schema or capability
    pub constraints: Vec<Constraint>,
}

/// Parameter metadata for tool input schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamMetadata {
    /// Parameter name
    pub name: String,
    /// Type (string, integer, number, boolean, array, object)
    pub param_type: String,
    /// Human-readable description
    pub description: String,
    /// Whether this parameter is required
    pub required: bool,
}

/// Generates tool stubs in the VFS from capabilities.
///
/// Supports two input formats:
/// - [`ToolCallCap`] - minimal capability with constraints
/// - [`McpTool`] - rich MCP tool definition with full parameter metadata
pub struct ToolStubGenerator;

impl ToolStubGenerator {
    /// Generate tool stubs from a list of tool capabilities.
    ///
    /// This is the basic method that works with `ToolCallCap` only.
    /// For richer output with parameter documentation, use `generate_from_mcp`.
    pub fn generate(vfs: &mut Vfs, capabilities: &[ToolCallCap]) {
        for cap in capabilities {
            let metadata = Self::cap_to_metadata(cap);
            Self::generate_tool_files(vfs, &metadata);
        }
    }

    /// Generate tool stubs from MCP tool definitions.
    ///
    /// This produces richer output with:
    /// - Full parameter documentation from JSON Schema
    /// - Type information for each parameter
    /// - Required/optional parameter markers
    /// - Constraints derived from min/max/enum
    /// - A prelude script that defines tools as globals
    pub fn generate_from_mcp(vfs: &mut Vfs, tools: &[McpTool]) {
        let mut tool_entries = Vec::new();

        for tool in tools {
            let metadata = tool.to_tool_metadata();
            Self::generate_tool_files(vfs, &metadata);

            // Collect tool info for prelude generation
            let tool_id = format!("{}:{}", metadata.provider, metadata.name);
            let fn_name = Self::js_identifier(&metadata.name);
            tool_entries.push((fn_name, tool_id, metadata.description.clone()));
        }

        // Always generate prelude with fs/shell, plus tool stubs if any
        let prelude = Self::generate_prelude(&tool_entries);
        let _ = vfs.insert_file(
            "/tools/prelude.js",
            prelude.as_bytes(),
            Permission::ReadOnly,
        );

        // Generate environment documentation
        Self::generate_environment_docs(vfs);
    }

    /// Generate a prelude with just fs and shell globals (no tools).
    ///
    /// This is useful when the sandbox has no tools registered but still
    /// needs the fs and shell wrappers.
    pub fn generate_fs_shell_prelude(vfs: &mut Vfs) {
        let prelude = Self::generate_prelude(&[]);
        let _ = vfs.insert_file(
            "/tools/prelude.js",
            prelude.as_bytes(),
            Permission::ReadOnly,
        );

        // Generate environment documentation
        Self::generate_environment_docs(vfs);
    }

    /// Generate environment documentation file explaining available APIs.
    fn generate_environment_docs(vfs: &mut Vfs) {
        let docs = r#"# Sandbox Environment

This is a **QuickJS sandbox**, NOT Node.js. Code runs in an isolated WASM environment.

## Available APIs

### Console
```javascript
console.log("message");
console.error("error");
console.warn("warning");
console.info("info");
console.debug("debug");
```

### File System (async)
```javascript
// All fs operations are async and require await
const content = await fs.readFile("/path/to/file");
await fs.writeFile("/path/to/file", "content");
const exists = await fs.exists("/path/to/file");
const stats = await fs.stat("/path/to/file");
const files = await fs.readdir("/path/to/dir");
await fs.mkdir("/path/to/dir", { recursive: true });
await fs.unlink("/path/to/file");
```

### Shell Commands (async)
```javascript
// Run sandboxed shell commands - returns {stdout, stderr, exitCode}
const result = await shell("grep pattern file.txt");
console.log(result.stdout);

// Convenience: shell.run() returns stdout only (throws on error)
const output = await shell.run("cat /tmp/data.json | jq '.items[]'");
```

Available shell commands: `cat`, `grep`, `head`, `tail`, `cut`, `sort`, `uniq`,
`wc`, `tr`, `sed`, `awk`, `jq`, `echo`, `printf`, `test`, `expr`, and more.

### HTTP Fetch (async, capability-controlled)
```javascript
const response = await fetch("https://api.example.com/data");
const json = await response.json();
const text = await response.text();
```

### Timers
```javascript
const id = setTimeout(() => console.log("delayed"), 1000);
clearTimeout(id);

const intervalId = setInterval(() => console.log("tick"), 1000);
clearInterval(intervalId);
```

### Tool Calls (async)
```javascript
// Tools are available as global functions (check /tools/ directory)
const result = await toolName({ param1: "value" });

// Introspection
const tools = listTools();           // Get all tool names
const info = getToolInfo("stripe_charge");  // Get tool metadata
```

## NOT Available (Node.js APIs)

These will throw helpful errors if used:

- `require()` / `import` - No module system
- `process` - No process object (env, cwd, argv, etc.)
- `Buffer` - Use strings or btoa()/atob() for base64
- `path` - Use template literals or shell commands
- `__dirname`, `__filename` - Not available
- `module`, `exports` - No CommonJS modules
- `http`, `https`, `net` - Use fetch() instead
- `child_process` - Use shell() instead
- `crypto` - Not available
- `os` - Not available

## Tips for Writing Code

1. **Use shell for data processing**: Instead of complex JS, use shell pipelines
   ```javascript
   // Good: Let shell tools do the work
   const disputed = await shell.run("cat /tmp/transactions.json | jq '.[] | select(.disputed)'");

   // Avoid: Complex JS parsing
   ```

2. **All I/O is async**: Always use `await` with fs, shell, fetch, and tool calls

3. **No imports needed**: fs, shell, fetch, console are all globals

4. **Write data to files**: Use the virtual filesystem as a scratchpad
   ```javascript
   await fs.writeFile("/tmp/data.json", JSON.stringify(largeData));
   const filtered = await shell.run("cat /tmp/data.json | jq '.items | length'");
   ```

5. **Check available tools**: Run `cat /tools/*/README.md` or use `listTools()`
"#;

        let _ = vfs.insert_file("/ENVIRONMENT.md", docs.as_bytes(), Permission::ReadOnly);
    }

    /// Generate a prelude script that defines all tools as global functions.
    ///
    /// This allows agent code to call tools directly without imports:
    /// ```js
    /// const result = await math_add({a: 1, b: 2});
    /// ```
    fn generate_prelude(tools: &[(String, String, String)]) -> String {
        let mut prelude =
            String::from("// Auto-generated tool prelude - defines tools as globals\n\n");

        // Node.js compatibility: helpful errors for common mistakes
        // Full docs at /ENVIRONMENT.md
        // Prelude is idempotent - safe to load multiple times
        prelude.push_str(
            r#"// QuickJS sandbox - NOT Node.js. See /ENVIRONMENT.md for available APIs.
if(!globalThis.__preludeLoaded){globalThis.__preludeLoaded=true;
const __notNode=(m)=>{throw new Error(m+' (QuickJS sandbox, not Node.js)')};
globalThis.require=(m)=>__notNode(`require('${m}') unavailable. Use fs/shell/fetch globals`);
globalThis.process=new Proxy({},{get:(_,p)=>__notNode(`process.${p} unavailable`)});
globalThis.Buffer=new Proxy(function(){},{construct:()=>__notNode('Buffer unavailable. Use btoa/atob'),get:(_,p)=>__notNode(`Buffer.${p} unavailable`),apply:()=>__notNode('Buffer unavailable')});
globalThis.path=new Proxy({},{get:(_,p)=>__notNode(`path.${p} unavailable. Use template literals or shell`)});
Object.defineProperty(globalThis,'__dirname',{get:()=>__notNode('__dirname unavailable. Use shell("pwd")')});
Object.defineProperty(globalThis,'__filename',{get:()=>__notNode('__filename unavailable')});
globalThis.module=new Proxy({},{get:(_,p)=>__notNode(`module.${p} unavailable`),set:()=>__notNode('module unavailable')});
globalThis.exports=new Proxy({},{get:()=>__notNode('exports unavailable'),set:()=>__notNode('exports unavailable')});

// File System (async - all operations require await)
globalThis.fs = {
    readFile: async (path) => await __amla__.fs.readFile(path),
    writeFile: async (path, data) => {
        const content = typeof data === 'string' ? data : JSON.stringify(data);
        return await __amla__.fs.writeFile(path, content);
    },
    exists: async (path) => await __amla__.fs.exists(path),
    mkdir: async (path, opts) => await __amla__.fs.mkdir(path, opts || {}),
    readdir: async (path) => await __amla__.fs.readDir(path),
    unlink: async (path) => await __amla__.fs.unlink(path),
    stat: async (path) => await __amla__.fs.stat(path),
    // Node.js "Sync" methods don't exist - throw helpful errors
    readFileSync: () => { throw new Error('Use: await fs.readFile(path)'); },
    writeFileSync: () => { throw new Error('Use: await fs.writeFile(path, data)'); },
    existsSync: () => { throw new Error('Use: await fs.exists(path)'); },
    mkdirSync: () => { throw new Error('Use: await fs.mkdir(path)'); },
    readdirSync: () => { throw new Error('Use: await fs.readdir(path)'); },
    unlinkSync: () => { throw new Error('Use: await fs.unlink(path)'); },
    statSync: () => { throw new Error('Use: await fs.stat(path)'); },
};

// =============================================================================
// Shell (async - returns {stdout, stderr, exitCode})
// =============================================================================
globalThis.shell = async (command) => await __amla__.shell(command);

// Convenience: shell.exec() returns full result, shell.run() returns stdout only
shell.exec = async (command) => await __amla__.shell(command);
shell.run = async (command) => {
    const result = await __amla__.shell(command);
    if (result.exitCode !== 0) {
        throw new Error(`Command failed (exit ${result.exitCode}): ${result.stderr || result.stdout}`);
    }
    return result.stdout;
};

"#,
        );

        // Tool registry for introspection
        prelude.push_str(
            "// =============================================================================\n",
        );
        prelude.push_str("// Tool Registry (for introspection)\n");
        prelude.push_str(
            "// =============================================================================\n",
        );
        prelude.push_str("globalThis.__tools__ = {\n");

        for (fn_name, tool_id, description) in tools {
            let escaped_desc = description.replace('\\', "\\\\").replace('"', "\\\"");
            prelude.push_str(&format!(
                "    {fn_name}: {{ id: \"{tool_id}\", description: \"{escaped_desc}\" }},\n",
            ));
        }
        prelude.push_str("};\n\n");

        // Introspection helpers
        prelude.push_str(
            r"/** List all available tool function names */
globalThis.listTools = () => Object.keys(__tools__);

/** Get metadata for a tool */
globalThis.getToolInfo = (name) => __tools__[name] || null;

// =============================================================================
// Tool Functions
// =============================================================================
",
        );

        // Generate tool functions
        for (fn_name, tool_id, description) in tools {
            let escaped_desc = description.replace("*/", "* /");
            prelude.push_str(&format!(
                r#"/** {escaped_desc} */
globalThis.{fn_name} = async (params) => await __amla__.toolCall("{tool_id}", params || {{}});

"#,
            ));
        }

        // Close the idempotency guard
        prelude.push_str("} // end __preludeLoaded guard\n");

        prelude
    }

    /// Generate tool stubs from a single `ToolMetadata`.
    pub fn generate_from_metadata(vfs: &mut Vfs, metadata: &ToolMetadata) {
        Self::generate_tool_files(vfs, metadata);
    }

    /// Convert a `ToolCallCap` to `ToolMetadata`
    fn cap_to_metadata(cap: &ToolCallCap) -> ToolMetadata {
        // Parse tool name: "provider:action" or just "action"
        let (provider, name) = if let Some(idx) = cap.tool.find(':') {
            (cap.tool[..idx].to_string(), cap.tool[idx + 1..].to_string())
        } else {
            ("default".to_string(), cap.tool.clone())
        };

        ToolMetadata {
            provider,
            name,
            description: format!("Tool: {}", cap.tool),
            params: vec![], // Would be populated from schema
            constraints: cap.constraints.constraints().to_vec(),
        }
    }

    /// Generate all files for a tool
    ///
    /// Uses `insert_file` to bypass permission checks - this is privileged
    /// runtime code populating read-only `/tools/` directory.
    fn generate_tool_files(vfs: &mut Vfs, metadata: &ToolMetadata) {
        let dir_path = format!("/tools/{}", metadata.provider);
        let _ = vfs.insert_dir_all(&dir_path, Permission::ReadOnly);

        // Generate .js stub
        let js_content = Self::generate_js_stub(metadata);
        let js_path = format!("{}/{}.js", dir_path, metadata.name);
        let _ = vfs.insert_file(&js_path, js_content.as_bytes(), Permission::ReadOnly);

        // Generate .d.ts type definitions
        let dts_content = Self::generate_dts(metadata);
        let dts_path = format!("{}/{}.d.ts", dir_path, metadata.name);
        let _ = vfs.insert_file(&dts_path, dts_content.as_bytes(), Permission::ReadOnly);

        // Generate README.md
        let readme_content = Self::generate_readme(metadata);
        let readme_path = format!("{}/{}.md", dir_path, metadata.name);
        let _ = vfs.insert_file(
            &readme_path,
            readme_content.as_bytes(),
            Permission::ReadOnly,
        );
    }

    /// Generate JavaScript stub that calls sidecar validation
    fn generate_js_stub(metadata: &ToolMetadata) -> String {
        let tool_id = format!("{}:{}", metadata.provider, metadata.name);
        let constraints_doc = Self::constraints_to_jsdoc(&metadata.constraints);
        let params_doc = Self::params_to_jsdoc(&metadata.params);
        // Sanitize name for valid JS identifier (e.g., "math.add" -> "math_add")
        let fn_name = Self::js_identifier(&metadata.name);

        format!(
            r#"/**
 * {description}
 *
 * @module {provider}/{name}
 *
 * Parameters:
{params_doc}
 *
 * Constraints:
{constraints_doc}
 */

/**
 * Call the {name} tool with the given parameters.
 * Parameters are validated against capability constraints before execution.
 *
 * @param {{Object}} params - Tool parameters
 * @returns {{Promise<any>}} Tool result
 * @throws {{Error}} If constraints are violated or tool execution fails
 */
export async function {fn_name}(params) {{
    return await __amla__.toolCall("{tool_id}", params);
}}

export default {fn_name};
"#,
            description = metadata.description,
            provider = metadata.provider,
            name = metadata.name,
            fn_name = fn_name,
            params_doc = params_doc,
            constraints_doc = constraints_doc,
            tool_id = tool_id,
        )
    }

    /// Generate TypeScript type definitions
    fn generate_dts(metadata: &ToolMetadata) -> String {
        let constraints_doc = Self::constraints_to_doc(&metadata.constraints);
        let params_interface = Self::params_to_typescript(&metadata.params);
        // Sanitize name for valid JS/TS identifier
        let fn_name = Self::js_identifier(&metadata.name);
        let pascal_name = Self::pascal_case(&fn_name);

        format!(
            r"/**
 * {description}
 *
 * Constraints:
{constraints_doc}
 */

export interface {pascal_name}Params {{
{params_interface}
}}

export interface {pascal_name}Result {{
    [key: string]: unknown;
}}

/**
 * Call the {name} tool.
 * @param params - Tool parameters (validated against constraints)
 * @returns Tool execution result
 */
export function {fn_name}(params: {pascal_name}Params): Promise<{pascal_name}Result>;

export default {fn_name};
",
            description = metadata.description,
            pascal_name = pascal_name,
            fn_name = fn_name,
            name = metadata.name,
            constraints_doc = constraints_doc,
            params_interface = params_interface,
        )
    }

    /// Generate README documentation
    fn generate_readme(metadata: &ToolMetadata) -> String {
        let constraints_doc = Self::constraints_to_markdown(&metadata.constraints);
        let params_doc = Self::params_to_markdown(&metadata.params);

        format!(
            r"# {provider}:{name}

{description}

## Usage

```javascript
import {{ {name} }} from '/tools/{provider}/{name}.js';

const result = await {name}({{
    // Add your parameters here
}});
```

## Parameters

{params_doc}

## Constraints

The following constraints are enforced on this tool:

{constraints_doc}
",
            provider = metadata.provider,
            name = metadata.name,
            description = metadata.description,
            params_doc = params_doc,
            constraints_doc = constraints_doc,
        )
    }

    /// Convert constraints to `JSDoc` format
    fn constraints_to_jsdoc(constraints: &[Constraint]) -> String {
        if constraints.is_empty() {
            return " *   (no constraints)".to_string();
        }

        constraints
            .iter()
            .map(|c| format!(" *   - {}", Self::constraint_to_string(c)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert constraints to plain doc format
    fn constraints_to_doc(constraints: &[Constraint]) -> String {
        if constraints.is_empty() {
            return " *   (no constraints)".to_string();
        }

        constraints
            .iter()
            .map(|c| format!(" * - {}", Self::constraint_to_string(c)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert constraints to Markdown format
    fn constraints_to_markdown(constraints: &[Constraint]) -> String {
        if constraints.is_empty() {
            return "No constraints defined.".to_string();
        }

        constraints
            .iter()
            .map(|c| format!("- {}", Self::constraint_to_string(c)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert a single constraint to human-readable string
    fn constraint_to_string(constraint: &Constraint) -> String {
        match constraint {
            Constraint::Lt { param, value } => format!("`{param}` < {value:?}"),
            Constraint::Le { param, value } => format!("`{param}` <= {value:?}"),
            Constraint::Gt { param, value } => format!("`{param}` > {value:?}"),
            Constraint::Ge { param, value } => format!("`{param}` >= {value:?}"),
            Constraint::Eq { param, value } => format!("`{param}` == {value:?}"),
            Constraint::Ne { param, value } => format!("`{param}` != {value:?}"),
            Constraint::In { param, values } => format!("`{param}` in {values:?}"),
            Constraint::NotIn { param, values } => format!("`{param}` not in {values:?}"),
            Constraint::StartsWith { param, prefix } => {
                format!("`{param}` starts with \"{prefix}\"")
            }
            Constraint::EndsWith { param, suffix } => format!("`{param}` ends with \"{suffix}\""),
            Constraint::Contains { param, substring } => {
                format!("`{param}` contains \"{substring}\"")
            }
            Constraint::Exists { param } => format!("`{param}` must exist"),
            Constraint::NotExists { param } => format!("`{param}` must not exist"),
            Constraint::And { constraints } => {
                let inner: Vec<_> = constraints.iter().map(Self::constraint_to_string).collect();
                format!("({})", inner.join(" AND "))
            }
            Constraint::Or { constraints } => {
                let inner: Vec<_> = constraints.iter().map(Self::constraint_to_string).collect();
                format!("({})", inner.join(" OR "))
            }
        }
    }

    /// Convert parameters to `JSDoc` format
    fn params_to_jsdoc(params: &[ParamMetadata]) -> String {
        if params.is_empty() {
            return " *   (no parameters)".to_string();
        }

        params
            .iter()
            .map(|p| {
                let required = if p.required { " (required)" } else { "" };
                format!(
                    " *   - {}: {} - {}{}",
                    p.name, p.param_type, p.description, required
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert parameters to TypeScript interface fields
    fn params_to_typescript(params: &[ParamMetadata]) -> String {
        if params.is_empty() {
            return "    [key: string]: unknown;".to_string();
        }

        params
            .iter()
            .map(|p| {
                let ts_type = Self::json_type_to_typescript(&p.param_type);
                let optional = if p.required { "" } else { "?" };
                format!(
                    "    /** {} */\n    {}{}: {};",
                    p.description, p.name, optional, ts_type
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert parameters to Markdown table
    fn params_to_markdown(params: &[ParamMetadata]) -> String {
        if params.is_empty() {
            return "No parameters defined.".to_string();
        }

        let mut lines = vec![
            "| Name | Type | Required | Description |".to_string(),
            "|------|------|----------|-------------|".to_string(),
        ];

        for p in params {
            let required = if p.required { "Yes" } else { "No" };
            lines.push(format!(
                "| {} | {} | {} | {} |",
                p.name, p.param_type, required, p.description
            ));
        }

        lines.join("\n")
    }

    /// Convert JSON Schema type to TypeScript type
    fn json_type_to_typescript(json_type: &str) -> &'static str {
        amla_tools::ParamType::from(json_type).to_typescript()
    }

    /// Convert `snake_case` to `PascalCase`
    fn pascal_case(s: &str) -> String {
        s.split('_')
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(c) => c.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            })
            .collect()
    }

    /// Convert a tool name to a valid JavaScript identifier.
    ///
    /// - Replaces dots and dashes with underscores
    /// - Prefixes with underscore if starts with digit
    /// - Replaces other invalid characters with underscores
    ///
    /// Examples:
    /// - "math.add" -> `math_add`
    /// - "my-tool" -> `my_tool`
    /// - "123tool" -> "_123tool"
    /// - "tool@v2" -> `tool_v2`
    fn js_identifier(s: &str) -> String {
        let mut result = String::with_capacity(s.len());

        for (i, c) in s.chars().enumerate() {
            match c {
                // Valid identifier characters
                'a'..='z' | 'A'..='Z' | '_' | '$' => result.push(c),
                '0'..='9' => {
                    // Digits valid except at start
                    if i == 0 {
                        result.push('_');
                    }
                    result.push(c);
                }
                // Replace invalid chars (and any other) with underscore
                _ => result.push('_'),
            }
        }

        // Handle empty result
        if result.is_empty() {
            return "_tool".to_string();
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_constraints::ConstraintSet;
    use serde_json::json;

    #[test]
    fn test_generate_tool_stubs() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::with_constraints(
            "stripe:charge",
            ConstraintSet::new(vec![
                Constraint::Ge {
                    param: "amount".to_string(),
                    value: json!(100),
                },
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(10000),
                },
            ]),
        );

        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Check files were created
        assert!(vfs.is_file("/tools/stripe/charge.js"));
        assert!(vfs.is_file("/tools/stripe/charge.d.ts"));
        assert!(vfs.is_file("/tools/stripe/charge.md"));

        // Check content
        let js = vfs.read_file_string("/tools/stripe/charge.js").unwrap();
        assert!(js.contains("__amla__.toolCall"));
        assert!(js.contains("stripe:charge"));
    }

    #[test]
    fn test_generate_tool_without_provider() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("echo");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        assert!(vfs.is_file("/tools/default/echo.js"));
    }

    #[test]
    fn test_pascal_case() {
        assert_eq!(ToolStubGenerator::pascal_case("hello_world"), "HelloWorld");
        assert_eq!(ToolStubGenerator::pascal_case("simple"), "Simple");
        assert_eq!(
            ToolStubGenerator::pascal_case("one_two_three"),
            "OneTwoThree"
        );
    }

    #[test]
    fn test_constraint_to_string() {
        let c = Constraint::Le {
            param: "amount".to_string(),
            value: json!(100),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`amount` <= Number(100)"
        );

        let c = Constraint::In {
            param: "currency".to_string(),
            values: vec![json!("USD"), json!("EUR")],
        };
        let s = ToolStubGenerator::constraint_to_string(&c);
        assert!(s.contains("currency"));
        assert!(s.contains("in"));
    }

    #[test]
    fn test_generate_from_mcp() {
        use crate::mcp::example_stripe_tools;

        let mut vfs = Vfs::new();
        let tools = example_stripe_tools();

        ToolStubGenerator::generate_from_mcp(&mut vfs, &tools);

        // Check files were created
        assert!(vfs.is_file("/tools/stripe/create_charge.js"));
        assert!(vfs.is_file("/tools/stripe/create_charge.d.ts"));
        assert!(vfs.is_file("/tools/stripe/create_charge.md"));
        assert!(vfs.is_file("/tools/stripe/create_refund.js"));

        // Check JS content has params
        let js = vfs
            .read_file_string("/tools/stripe/create_charge.js")
            .unwrap();
        assert!(js.contains("amount"));
        assert!(js.contains("currency"));
        assert!(js.contains("customer"));

        // Check TypeScript has proper types
        let dts = vfs
            .read_file_string("/tools/stripe/create_charge.d.ts")
            .unwrap();
        assert!(dts.contains("amount: number"));
        assert!(dts.contains("currency: string"));

        // Check README has params table
        let readme = vfs
            .read_file_string("/tools/stripe/create_charge.md")
            .unwrap();
        assert!(readme.contains("| Name | Type |"));
        assert!(readme.contains("| amount |"));
    }

    #[test]
    fn test_params_to_typescript() {
        let params = vec![
            ParamMetadata {
                name: "count".to_string(),
                param_type: "integer".to_string(),
                description: "Number of items".to_string(),
                required: true,
            },
            ParamMetadata {
                name: "filter".to_string(),
                param_type: "string".to_string(),
                description: "Optional filter".to_string(),
                required: false,
            },
        ];

        let ts = ToolStubGenerator::params_to_typescript(&params);
        assert!(ts.contains("count: number;"));
        assert!(ts.contains("filter?: string;"));
    }

    #[test]
    fn test_json_type_to_typescript() {
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("string"),
            "string"
        );
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("integer"),
            "number"
        );
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("number"),
            "number"
        );
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("boolean"),
            "boolean"
        );
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("array"),
            "unknown[]"
        );
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("object"),
            "Record<string, unknown>"
        );
        // Unknown types default to string (consistent with ParamType::from behavior)
        assert_eq!(
            ToolStubGenerator::json_type_to_typescript("custom"),
            "string"
        );
    }

    #[test]
    fn test_generate_from_metadata() {
        let mut vfs = Vfs::new();

        let metadata = ToolMetadata {
            provider: "custom".to_string(),
            name: "my_tool".to_string(),
            description: "A custom tool for testing".to_string(),
            params: vec![ParamMetadata {
                name: "input".to_string(),
                param_type: "string".to_string(),
                description: "The input value".to_string(),
                required: true,
            }],
            constraints: vec![Constraint::Ge {
                param: "limit".to_string(),
                value: json!(1),
            }],
        };

        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        // Check files were created
        assert!(vfs.is_file("/tools/custom/my_tool.js"));
        assert!(vfs.is_file("/tools/custom/my_tool.d.ts"));
        assert!(vfs.is_file("/tools/custom/my_tool.md"));

        // Check content
        let js = vfs.read_file_string("/tools/custom/my_tool.js").unwrap();
        assert!(js.contains("custom:my_tool"));
        assert!(js.contains("A custom tool for testing"));
    }

    #[test]
    fn test_constraint_to_string_comparison() {
        // Lt
        let c = Constraint::Lt {
            param: "x".to_string(),
            value: json!(10),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`x` < Number(10)"
        );

        // Gt
        let c = Constraint::Gt {
            param: "y".to_string(),
            value: json!(0),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`y` > Number(0)"
        );

        // Eq
        let c = Constraint::Eq {
            param: "status".to_string(),
            value: json!("active"),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`status` == String(\"active\")"
        );

        // Ne
        let c = Constraint::Ne {
            param: "role".to_string(),
            value: json!("guest"),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`role` != String(\"guest\")"
        );
    }

    #[test]
    fn test_constraint_to_string_membership() {
        // NotIn
        let c = Constraint::NotIn {
            param: "method".to_string(),
            values: vec![json!("DELETE"), json!("DROP")],
        };
        let s = ToolStubGenerator::constraint_to_string(&c);
        assert!(s.contains("`method` not in"));
        assert!(s.contains("DELETE"));
        assert!(s.contains("DROP"));
    }

    #[test]
    fn test_constraint_to_string_string_ops() {
        // StartsWith
        let c = Constraint::StartsWith {
            param: "path".to_string(),
            prefix: "/api/".to_string(),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`path` starts with \"/api/\""
        );

        // EndsWith
        let c = Constraint::EndsWith {
            param: "file".to_string(),
            suffix: ".json".to_string(),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`file` ends with \".json\""
        );

        // Contains
        let c = Constraint::Contains {
            param: "query".to_string(),
            substring: "SELECT".to_string(),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`query` contains \"SELECT\""
        );
    }

    #[test]
    fn test_constraint_to_string_existence() {
        // Exists
        let c = Constraint::Exists {
            param: "user_id".to_string(),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`user_id` must exist"
        );

        // NotExists
        let c = Constraint::NotExists {
            param: "deprecated".to_string(),
        };
        assert_eq!(
            ToolStubGenerator::constraint_to_string(&c),
            "`deprecated` must not exist"
        );
    }

    #[test]
    fn test_constraint_to_string_composite() {
        // And
        let c = Constraint::And {
            constraints: vec![
                Constraint::Ge {
                    param: "min".to_string(),
                    value: json!(0),
                },
                Constraint::Le {
                    param: "max".to_string(),
                    value: json!(100),
                },
            ],
        };
        let s = ToolStubGenerator::constraint_to_string(&c);
        assert!(s.contains("AND"));
        assert!(s.contains("`min` >= Number(0)"));
        assert!(s.contains("`max` <= Number(100)"));

        // Or
        let c = Constraint::Or {
            constraints: vec![
                Constraint::Eq {
                    param: "type".to_string(),
                    value: json!("credit"),
                },
                Constraint::Eq {
                    param: "type".to_string(),
                    value: json!("debit"),
                },
            ],
        };
        let s = ToolStubGenerator::constraint_to_string(&c);
        assert!(s.contains("OR"));
        assert!(s.contains("credit"));
        assert!(s.contains("debit"));
    }

    #[test]
    fn test_pascal_case_edge_cases() {
        // Empty string
        assert_eq!(ToolStubGenerator::pascal_case(""), "");

        // Single character
        assert_eq!(ToolStubGenerator::pascal_case("a"), "A");

        // Trailing underscore (creates empty part)
        assert_eq!(ToolStubGenerator::pascal_case("hello_"), "Hello");

        // Leading underscore (creates empty part)
        assert_eq!(ToolStubGenerator::pascal_case("_world"), "World");

        // Double underscore (creates empty part in middle)
        assert_eq!(ToolStubGenerator::pascal_case("hello__world"), "HelloWorld");

        // Multiple underscores
        assert_eq!(ToolStubGenerator::pascal_case("___"), "");

        // Already pascal case (no underscores) - keeps original casing after first char
        assert_eq!(
            ToolStubGenerator::pascal_case("AlreadyPascal"),
            "AlreadyPascal"
        );
    }

    #[test]
    fn test_params_to_jsdoc_empty() {
        let params: Vec<ParamMetadata> = vec![];
        let result = ToolStubGenerator::params_to_jsdoc(&params);
        assert_eq!(result, " *   (no parameters)");
    }

    #[test]
    fn test_params_to_jsdoc_with_optional() {
        let params = vec![
            ParamMetadata {
                name: "required_param".to_string(),
                param_type: "string".to_string(),
                description: "Required field".to_string(),
                required: true,
            },
            ParamMetadata {
                name: "optional_param".to_string(),
                param_type: "integer".to_string(),
                description: "Optional field".to_string(),
                required: false,
            },
        ];
        let result = ToolStubGenerator::params_to_jsdoc(&params);
        assert!(result.contains("required_param"));
        assert!(result.contains("(required)"));
        assert!(result.contains("optional_param"));
        // Optional params don't have (required) marker
        let lines: Vec<&str> = result.lines().collect();
        assert!(lines[0].contains("(required)"));
        assert!(!lines[1].contains("(required)"));
    }

    #[test]
    fn test_params_to_typescript_empty() {
        let params: Vec<ParamMetadata> = vec![];
        let result = ToolStubGenerator::params_to_typescript(&params);
        assert_eq!(result, "    [key: string]: unknown;");
    }

    #[test]
    fn test_params_to_markdown_empty() {
        let params: Vec<ParamMetadata> = vec![];
        let result = ToolStubGenerator::params_to_markdown(&params);
        assert_eq!(result, "No parameters defined.");
    }

    #[test]
    fn test_params_to_markdown_table() {
        let params = vec![
            ParamMetadata {
                name: "id".to_string(),
                param_type: "string".to_string(),
                description: "Unique identifier".to_string(),
                required: true,
            },
            ParamMetadata {
                name: "page".to_string(),
                param_type: "integer".to_string(),
                description: "Page number".to_string(),
                required: false,
            },
        ];
        let result = ToolStubGenerator::params_to_markdown(&params);

        // Check table header
        assert!(result.contains("| Name | Type | Required | Description |"));
        assert!(result.contains("|------|------|----------|-------------|"));

        // Check rows
        assert!(result.contains("| id | string | Yes | Unique identifier |"));
        assert!(result.contains("| page | integer | No | Page number |"));
    }

    #[test]
    fn test_constraints_to_jsdoc_empty() {
        let constraints: Vec<Constraint> = vec![];
        let result = ToolStubGenerator::constraints_to_jsdoc(&constraints);
        assert_eq!(result, " *   (no constraints)");
    }

    #[test]
    fn test_constraints_to_doc_empty() {
        let constraints: Vec<Constraint> = vec![];
        let result = ToolStubGenerator::constraints_to_doc(&constraints);
        assert_eq!(result, " *   (no constraints)");
    }

    #[test]
    fn test_constraints_to_markdown_empty() {
        let constraints: Vec<Constraint> = vec![];
        let result = ToolStubGenerator::constraints_to_markdown(&constraints);
        assert_eq!(result, "No constraints defined.");
    }

    #[test]
    fn test_full_readme_generation() {
        let metadata = ToolMetadata {
            provider: "test".to_string(),
            name: "full_test".to_string(),
            description: "A comprehensive test tool".to_string(),
            params: vec![ParamMetadata {
                name: "query".to_string(),
                param_type: "string".to_string(),
                description: "Search query".to_string(),
                required: true,
            }],
            constraints: vec![Constraint::Le {
                param: "limit".to_string(),
                value: json!(100),
            }],
        };

        let mut vfs = Vfs::new();
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        let readme = vfs.read_file_string("/tools/test/full_test.md").unwrap();

        // Check structure
        assert!(readme.contains("# test:full_test"));
        assert!(readme.contains("A comprehensive test tool"));
        assert!(readme.contains("## Usage"));
        assert!(readme.contains("## Parameters"));
        assert!(readme.contains("## Constraints"));

        // Check usage code block
        assert!(readme.contains("import { full_test }"));
        assert!(readme.contains("from '/tools/test/full_test.js'"));
    }

    #[test]
    fn test_full_dts_generation() {
        let metadata = ToolMetadata {
            provider: "api".to_string(),
            name: "create_user".to_string(),
            description: "Create a new user".to_string(),
            params: vec![
                ParamMetadata {
                    name: "email".to_string(),
                    param_type: "string".to_string(),
                    description: "User email".to_string(),
                    required: true,
                },
                ParamMetadata {
                    name: "admin".to_string(),
                    param_type: "boolean".to_string(),
                    description: "Is admin".to_string(),
                    required: false,
                },
            ],
            constraints: vec![Constraint::Exists {
                param: "email".to_string(),
            }],
        };

        let mut vfs = Vfs::new();
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        let dts = vfs.read_file_string("/tools/api/create_user.d.ts").unwrap();

        // Check interface name is PascalCase
        assert!(dts.contains("export interface CreateUserParams"));
        assert!(dts.contains("export interface CreateUserResult"));

        // Check function signature
        assert!(dts.contains(
            "export function create_user(params: CreateUserParams): Promise<CreateUserResult>"
        ));

        // Check param types
        assert!(dts.contains("email: string"));
        assert!(dts.contains("admin?: boolean")); // Optional with ?
    }

    #[test]
    fn test_multiple_constraints_in_docs() {
        let constraints = vec![
            Constraint::Ge {
                param: "amount".to_string(),
                value: json!(100),
            },
            Constraint::Le {
                param: "amount".to_string(),
                value: json!(10000),
            },
            Constraint::In {
                param: "currency".to_string(),
                values: vec![json!("USD"), json!("EUR")],
            },
        ];

        // Test JSDoc format
        let jsdoc = ToolStubGenerator::constraints_to_jsdoc(&constraints);
        assert!(jsdoc.contains("`amount` >= Number(100)"));
        assert!(jsdoc.contains("`amount` <= Number(10000)"));
        assert!(jsdoc.contains("`currency` in"));

        // Test Markdown format
        let markdown = ToolStubGenerator::constraints_to_markdown(&constraints);
        assert!(markdown.starts_with("- ")); // List items
        assert!(markdown.contains("\n- ")); // Multiple items
    }

    // ========== PATH MAPPING TESTS ==========

    #[test]
    fn test_path_mapping_standard_format() {
        let mut vfs = Vfs::new();

        // Standard "provider:action" format
        let cap = ToolCallCap::new("github:create_issue");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Should create /tools/github/create_issue.* files
        assert!(vfs.is_file("/tools/github/create_issue.js"));
        assert!(vfs.is_file("/tools/github/create_issue.d.ts"));
        assert!(vfs.is_file("/tools/github/create_issue.md"));

        // Verify directory structure
        assert!(vfs.is_dir("/tools"));
        assert!(vfs.is_dir("/tools/github"));
    }

    #[test]
    fn test_path_mapping_no_provider() {
        let mut vfs = Vfs::new();

        // No colon = uses "default" provider
        let cap = ToolCallCap::new("echo");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        assert!(vfs.is_file("/tools/default/echo.js"));
        assert!(vfs.is_file("/tools/default/echo.d.ts"));
        assert!(vfs.is_file("/tools/default/echo.md"));
    }

    #[test]
    fn test_path_mapping_multiple_colons() {
        let mut vfs = Vfs::new();

        // Multiple colons - first colon separates provider from action
        let cap = ToolCallCap::new("aws:s3:upload");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Provider is "aws", action is "s3:upload"
        assert!(vfs.is_file("/tools/aws/s3:upload.js"));
        assert!(vfs.is_file("/tools/aws/s3:upload.d.ts"));
        assert!(vfs.is_file("/tools/aws/s3:upload.md"));
    }

    #[test]
    fn test_path_mapping_colon_at_start() {
        let mut vfs = Vfs::new();

        // Colon at start = empty provider, action is the rest
        let cap = ToolCallCap::new(":action_only");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Empty provider is used as-is (edge case)
        assert!(vfs.is_file("/tools//action_only.js"));
    }

    #[test]
    fn test_path_mapping_colon_at_end() {
        let mut vfs = Vfs::new();

        // Colon at end = provider with empty action
        let cap = ToolCallCap::new("provider:");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Empty action is used as-is (edge case)
        assert!(vfs.is_file("/tools/provider/.js"));
    }

    #[test]
    fn test_path_mapping_multiple_providers() {
        let mut vfs = Vfs::new();

        let caps = vec![
            ToolCallCap::new("stripe:charge"),
            ToolCallCap::new("stripe:refund"),
            ToolCallCap::new("github:create_pr"),
            ToolCallCap::new("notion:search"),
        ];
        ToolStubGenerator::generate(&mut vfs, &caps);

        // Each provider gets its own directory
        assert!(vfs.is_dir("/tools/stripe"));
        assert!(vfs.is_dir("/tools/github"));
        assert!(vfs.is_dir("/tools/notion"));

        // All files created
        assert!(vfs.is_file("/tools/stripe/charge.js"));
        assert!(vfs.is_file("/tools/stripe/refund.js"));
        assert!(vfs.is_file("/tools/github/create_pr.js"));
        assert!(vfs.is_file("/tools/notion/search.js"));
    }

    #[test]
    fn test_path_mapping_underscore_names() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("my_provider:my_action_name");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        assert!(vfs.is_file("/tools/my_provider/my_action_name.js"));
        assert!(vfs.is_file("/tools/my_provider/my_action_name.d.ts"));
    }

    #[test]
    fn test_path_mapping_numeric_names() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("api2:create_v3");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        assert!(vfs.is_file("/tools/api2/create_v3.js"));
    }

    #[test]
    fn test_generated_js_contains_correct_tool_id() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("stripe:charge");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        let js = vfs.read_file_string("/tools/stripe/charge.js").unwrap();

        // Tool ID in the call should match original
        assert!(js.contains("__amla__.toolCall(\"stripe:charge\""));
    }

    #[test]
    fn test_generated_dts_has_correct_interface_name() {
        let mut vfs = Vfs::new();

        let metadata = ToolMetadata {
            provider: "github".to_string(),
            name: "create_pull_request".to_string(),
            description: "Create a PR".to_string(),
            params: vec![],
            constraints: vec![],
        };
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        let dts = vfs
            .read_file_string("/tools/github/create_pull_request.d.ts")
            .unwrap();

        // Interface name should be PascalCase
        assert!(dts.contains("export interface CreatePullRequestParams"));
        assert!(dts.contains("export interface CreatePullRequestResult"));
    }

    #[test]
    fn test_generated_readme_has_correct_import_path() {
        let mut vfs = Vfs::new();

        let metadata = ToolMetadata {
            provider: "notion".to_string(),
            name: "search".to_string(),
            description: "Search Notion".to_string(),
            params: vec![],
            constraints: vec![],
        };
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        let readme = vfs.read_file_string("/tools/notion/search.md").unwrap();

        // Import path should match file location
        assert!(readme.contains("from '/tools/notion/search.js'"));
    }

    #[test]
    fn test_files_are_readonly() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("test:tool");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Files should be read-only
        let stat = vfs.stat("/tools/test/tool.js").unwrap();
        assert_eq!(stat.permission(), Permission::ReadOnly);
    }

    #[test]
    fn test_directory_is_readonly() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("test:tool");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // Directories should be read-only
        let stat = vfs.stat("/tools").unwrap();
        assert_eq!(stat.permission(), Permission::ReadOnly);

        let stat = vfs.stat("/tools/test").unwrap();
        assert_eq!(stat.permission(), Permission::ReadOnly);
    }

    #[test]
    fn test_cap_to_metadata_parsing() {
        // Standard format
        let cap = ToolCallCap::new("stripe:charge");
        let metadata = ToolStubGenerator::cap_to_metadata(&cap);
        assert_eq!(metadata.provider, "stripe");
        assert_eq!(metadata.name, "charge");

        // No provider
        let cap = ToolCallCap::new("echo");
        let metadata = ToolStubGenerator::cap_to_metadata(&cap);
        assert_eq!(metadata.provider, "default");
        assert_eq!(metadata.name, "echo");

        // Multiple colons
        let cap = ToolCallCap::new("aws:s3:upload");
        let metadata = ToolStubGenerator::cap_to_metadata(&cap);
        assert_eq!(metadata.provider, "aws");
        assert_eq!(metadata.name, "s3:upload");
    }

    #[test]
    fn test_metadata_constraints_preserved() {
        let constraints = vec![
            Constraint::Le {
                param: "amount".to_string(),
                value: json!(100),
            },
            Constraint::In {
                param: "currency".to_string(),
                values: vec![json!("USD")],
            },
        ];

        let cap =
            ToolCallCap::with_constraints("test:tool", ConstraintSet::new(constraints.clone()));
        let metadata = ToolStubGenerator::cap_to_metadata(&cap);

        assert_eq!(metadata.constraints.len(), 2);
    }

    // ========== TOOL ID CONSISTENCY TESTS ==========

    #[test]
    fn test_tool_id_in_js_matches_reconstructed() {
        let mut vfs = Vfs::new();

        // Test cases: (original_name, expected_tool_id)
        // Note: Tool ID is always "{provider}:{action}" in the generated code
        let test_cases = vec![
            ("simple", "default:simple"),           // No colon → default provider
            ("provider:action", "provider:action"), // Standard format
            ("aws:s3:upload", "aws:s3:upload"),     // Multiple colons preserved
            (
                "with_underscores:action_name",
                "with_underscores:action_name",
            ),
        ];

        for (tool_name, expected_tool_id) in test_cases {
            let cap = ToolCallCap::new(tool_name);
            ToolStubGenerator::generate(&mut vfs, &[cap]);

            // Extract provider and action to find file
            let (provider, action) = if let Some(idx) = tool_name.find(':') {
                (&tool_name[..idx], &tool_name[idx + 1..])
            } else {
                ("default", tool_name)
            };

            let js_path = format!("/tools/{provider}/{action}.js");
            let js = vfs.read_file_string(&js_path).unwrap();

            // Verify tool ID in generated code matches expected
            let expected = format!("__amla__.toolCall(\"{expected_tool_id}\"");
            assert!(
                js.contains(&expected),
                "Tool ID mismatch for {tool_name}: expected '{expected}' in JS"
            );
        }
    }

    #[test]
    fn test_all_three_files_generated() {
        let mut vfs = Vfs::new();

        let cap = ToolCallCap::new("test:tool");
        ToolStubGenerator::generate(&mut vfs, &[cap]);

        // All three file types must be present
        assert!(vfs.is_file("/tools/test/tool.js"), "JS file missing");
        assert!(vfs.is_file("/tools/test/tool.d.ts"), "DTS file missing");
        assert!(vfs.is_file("/tools/test/tool.md"), "MD file missing");
    }

    #[test]
    fn test_js_is_valid_module() {
        let mut vfs = Vfs::new();

        let metadata = ToolMetadata {
            provider: "test".to_string(),
            name: "validate".to_string(),
            description: "Test tool".to_string(),
            params: vec![ParamMetadata {
                name: "input".to_string(),
                param_type: "string".to_string(),
                description: "Input".to_string(),
                required: true,
            }],
            constraints: vec![],
        };
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        let js = vfs.read_file_string("/tools/test/validate.js").unwrap();

        // Should have module exports
        assert!(js.contains("export async function validate"));
        assert!(js.contains("export default validate"));

        // Should have JSDoc
        assert!(js.contains("/**"));
        assert!(js.contains("*/"));
    }

    #[test]
    fn test_dts_is_valid_typescript() {
        let mut vfs = Vfs::new();

        let metadata = ToolMetadata {
            provider: "api".to_string(),
            name: "get_user".to_string(),
            description: "Get a user".to_string(),
            params: vec![
                ParamMetadata {
                    name: "id".to_string(),
                    param_type: "string".to_string(),
                    description: "User ID".to_string(),
                    required: true,
                },
                ParamMetadata {
                    name: "include_metadata".to_string(),
                    param_type: "boolean".to_string(),
                    description: "Include metadata".to_string(),
                    required: false,
                },
            ],
            constraints: vec![],
        };
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata);

        let dts = vfs.read_file_string("/tools/api/get_user.d.ts").unwrap();

        // Valid TypeScript structure
        assert!(dts.contains("export interface GetUserParams"));
        assert!(dts.contains("export interface GetUserResult"));
        assert!(
            dts.contains("export function get_user(params: GetUserParams): Promise<GetUserResult>")
        );
        assert!(dts.contains("export default get_user"));

        // Required vs optional params
        assert!(dts.contains("id: string;")); // Required
        assert!(dts.contains("include_metadata?: boolean;")); // Optional
    }

    // ========== OVERWRITE BEHAVIOR TESTS ==========

    #[test]
    fn test_regenerate_overwrites_existing() {
        let mut vfs = Vfs::new();

        // Generate first version
        let metadata1 = ToolMetadata {
            provider: "test".to_string(),
            name: "tool".to_string(),
            description: "First version".to_string(),
            params: vec![],
            constraints: vec![],
        };
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata1);

        let js1 = vfs.read_file_string("/tools/test/tool.js").unwrap();
        assert!(js1.contains("First version"));

        // Generate second version (should overwrite)
        let metadata2 = ToolMetadata {
            provider: "test".to_string(),
            name: "tool".to_string(),
            description: "Second version".to_string(),
            params: vec![],
            constraints: vec![],
        };
        ToolStubGenerator::generate_from_metadata(&mut vfs, &metadata2);

        let js2 = vfs.read_file_string("/tools/test/tool.js").unwrap();
        assert!(js2.contains("Second version"));
        assert!(!js2.contains("First version"));
    }

    #[test]
    fn test_js_identifier() {
        // Basic conversions
        assert_eq!(ToolStubGenerator::js_identifier("math.add"), "math_add");
        assert_eq!(ToolStubGenerator::js_identifier("my-tool"), "my_tool");
        assert_eq!(ToolStubGenerator::js_identifier("simple"), "simple");

        // Edge cases
        assert_eq!(ToolStubGenerator::js_identifier("123tool"), "_123tool");
        assert_eq!(ToolStubGenerator::js_identifier("tool@v2"), "tool_v2");
        assert_eq!(ToolStubGenerator::js_identifier("a/b/c"), "a_b_c");
        assert_eq!(ToolStubGenerator::js_identifier("foo:bar"), "foo_bar");
        assert_eq!(ToolStubGenerator::js_identifier("with space"), "with_space");

        // $ is valid in JS identifiers
        assert_eq!(ToolStubGenerator::js_identifier("$tool"), "$tool");

        // Empty string
        assert_eq!(ToolStubGenerator::js_identifier(""), "_tool");
    }
}
