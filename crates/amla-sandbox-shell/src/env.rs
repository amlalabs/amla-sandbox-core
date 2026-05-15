//! Environment variable handling.

use std::collections::HashMap;

/// Environment variables.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// Variable storage.
    vars: HashMap<String, String>,
}

impl Environment {
    /// Create a new empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an environment with some standard variables.
    pub fn with_defaults() -> Self {
        let mut env = Self::new();
        env.set("PATH", "/bin:/usr/bin");
        env.set("HOME", "/home");
        env.set("SHELL", "/bin/sh");
        env.set("PWD", "/");
        env
    }

    /// Get a variable.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(std::string::String::as_str)
    }

    /// Set a variable.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(name.into(), value.into());
    }

    /// Unset a variable.
    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
    }

    /// Check if a variable is set.
    pub fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// Iterate over all variables.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Get number of variables.
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Expand variables in a string.
    ///
    /// Supports:
    /// - `$VAR` - simple variable
    /// - `${VAR}` - braced variable
    /// - `$?` - last exit code (needs to be passed in)
    /// - `$$` - shell PID (always 1 in sandbox)
    /// - `$0` - shell name
    pub fn expand(&self, s: &str, last_exit: i32) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '$' {
                match chars.peek() {
                    Some('{') => {
                        chars.next(); // consume '{'
                        let mut name = String::new();
                        while let Some(&c) = chars.peek() {
                            if c == '}' {
                                chars.next();
                                break;
                            }
                            name.push(c);
                            chars.next();
                        }
                        if let Some(value) = self.get(&name) {
                            result.push_str(value);
                        }
                    }
                    Some('?') => {
                        chars.next();
                        result.push_str(&last_exit.to_string());
                    }
                    Some('$') => {
                        chars.next();
                        result.push('1'); // PID is always 1 in sandbox
                    }
                    Some('!') => {
                        chars.next();
                        // $! is the PID of the last background job
                        // In sandbox, check if set via env, otherwise empty
                        if let Some(val) = self.get("!") {
                            result.push_str(val);
                        }
                        // If not set, expands to empty string (no background jobs)
                    }
                    Some('0') => {
                        chars.next();
                        // $0 is the shell/script name - check env first, default to "sh"
                        if let Some(val) = self.get("0") {
                            result.push_str(val);
                        } else {
                            result.push_str("sh");
                        }
                    }
                    Some('#') => {
                        chars.next();
                        // $# is the number of positional parameters
                        if let Some(val) = self.get("#") {
                            result.push_str(val);
                        } else {
                            result.push('0');
                        }
                    }
                    Some('@') | Some('*') => {
                        chars.next();
                        // $@ and $* are all positional parameters
                        // (In simple cases they're equivalent; difference is in quoted contexts)
                        if let Some(val) = self.get("@") {
                            result.push_str(val);
                        }
                    }
                    Some(c) if c.is_ascii_digit() && *c != '0' => {
                        // $1, $2, ... $9 - positional parameters
                        // Note: $0 is handled above
                        let digit = *c;
                        chars.next();
                        if let Some(val) = self.get(&digit.to_string()) {
                            result.push_str(val);
                        }
                        // If not set, expands to empty string
                    }
                    Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
                        let mut name = String::new();
                        while let Some(&c) = chars.peek() {
                            if c.is_ascii_alphanumeric() || c == '_' {
                                name.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        if let Some(value) = self.get(&name) {
                            result.push_str(value);
                        }
                    }
                    _ => {
                        result.push('$');
                    }
                }
            } else {
                result.push(c);
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Basic operations
    // =========================================================================

    #[test]
    fn basic_operations() {
        let mut env = Environment::new();
        assert!(env.get("FOO").is_none());

        env.set("FOO", "bar");
        assert_eq!(env.get("FOO"), Some("bar"));

        env.unset("FOO");
        assert!(env.get("FOO").is_none());
    }

    #[test]
    fn contains() {
        let mut env = Environment::new();
        assert!(!env.contains("FOO"));
        env.set("FOO", "bar");
        assert!(env.contains("FOO"));
    }

    #[test]
    fn len_and_is_empty() {
        let mut env = Environment::new();
        assert!(env.is_empty());
        assert_eq!(env.len(), 0);

        env.set("A", "1");
        assert!(!env.is_empty());
        assert_eq!(env.len(), 1);

        env.set("B", "2");
        assert_eq!(env.len(), 2);

        env.unset("A");
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn iter() {
        let mut env = Environment::new();
        env.set("A", "1");
        env.set("B", "2");

        let pairs: Vec<_> = env.iter().collect();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("A", "1")));
        assert!(pairs.contains(&("B", "2")));
    }

    #[test]
    fn overwrite_variable() {
        let mut env = Environment::new();
        env.set("FOO", "first");
        assert_eq!(env.get("FOO"), Some("first"));

        env.set("FOO", "second");
        assert_eq!(env.get("FOO"), Some("second"));
    }

    #[test]
    fn unset_nonexistent() {
        let mut env = Environment::new();
        env.unset("NONEXISTENT"); // Should not panic
    }

    #[test]
    fn defaults() {
        let env = Environment::with_defaults();
        assert!(env.get("PATH").is_some());
        assert!(env.get("HOME").is_some());
        assert!(env.get("SHELL").is_some());
        assert!(env.get("PWD").is_some());
    }

    // =========================================================================
    // Simple expansion
    // =========================================================================

    #[test]
    fn expand_simple() {
        let mut env = Environment::new();
        env.set("NAME", "world");

        assert_eq!(env.expand("hello $NAME", 0), "hello world");
        assert_eq!(env.expand("hello ${NAME}", 0), "hello world");
    }

    #[test]
    fn expand_no_vars() {
        let env = Environment::new();
        assert_eq!(env.expand("hello world", 0), "hello world");
    }

    #[test]
    fn expand_missing() {
        let env = Environment::new();

        // Missing variables expand to empty
        assert_eq!(env.expand("$MISSING", 0), "");
        assert_eq!(env.expand("a${MISSING}b", 0), "ab");
    }

    #[test]
    fn expand_empty_variable() {
        let mut env = Environment::new();
        env.set("EMPTY", "");
        // $EMPTYb is parsed as variable name "EMPTYb", not "EMPTY" + "b"
        // Since EMPTYb isn't set, it expands to empty
        assert_eq!(env.expand("a$EMPTYb", 0), "a"); // EMPTYb not set -> ""
        assert_eq!(env.expand("a${EMPTY}b", 0), "ab"); // EMPTY is set to ""
    }

    // =========================================================================
    // Special variables
    // =========================================================================

    #[test]
    fn expand_special() {
        let env = Environment::new();

        assert_eq!(env.expand("exit=$?", 42), "exit=42");
        assert_eq!(env.expand("pid=$$", 0), "pid=1");
        assert_eq!(env.expand("shell=$0", 0), "shell=sh");
    }

    #[test]
    fn expand_background_pid() {
        let mut env = Environment::new();

        // By default, $! is empty (no background jobs)
        assert_eq!(env.expand("$!", 0), "");
        assert_eq!(env.expand("bg=$!", 0), "bg=");

        // When set (e.g., by background job handling), it expands
        env.set("!", "12345");
        assert_eq!(env.expand("$!", 0), "12345");
        assert_eq!(env.expand("bg=$!", 0), "bg=12345");
    }

    #[test]
    fn expand_exit_code_zero() {
        let env = Environment::new();
        assert_eq!(env.expand("$?", 0), "0");
    }

    #[test]
    fn expand_exit_code_negative() {
        let env = Environment::new();
        // Negative exit codes are possible in some systems
        assert_eq!(env.expand("$?", -1), "-1");
    }

    #[test]
    fn expand_exit_code_large() {
        let env = Environment::new();
        assert_eq!(env.expand("$?", 255), "255");
    }

    // =========================================================================
    // Variable name parsing
    // =========================================================================

    #[test]
    fn expand_underscore_in_name() {
        let mut env = Environment::new();
        env.set("MY_VAR", "value");
        assert_eq!(env.expand("$MY_VAR", 0), "value");
        assert_eq!(env.expand("${MY_VAR}", 0), "value");
    }

    #[test]
    fn expand_numbers_in_name() {
        let mut env = Environment::new();
        env.set("VAR1", "one");
        env.set("VAR2", "two");
        assert_eq!(env.expand("$VAR1$VAR2", 0), "onetwo");
    }

    #[test]
    fn expand_name_ends_at_non_alnum() {
        let mut env = Environment::new();
        env.set("VAR", "value");

        // Variable name ends at slash
        assert_eq!(env.expand("$VAR/path", 0), "value/path");
        // Variable name ends at dot
        assert_eq!(env.expand("$VAR.txt", 0), "value.txt");
        // Variable name ends at colon
        assert_eq!(env.expand("$VAR:next", 0), "value:next");
        // Variable name ends at dash (- is not alphanumeric)
        assert_eq!(env.expand("$VAR-suffix", 0), "value-suffix");
    }

    #[test]
    fn expand_adjacent_variables() {
        let mut env = Environment::new();
        env.set("A", "1");
        env.set("B", "2");

        assert_eq!(env.expand("$A$B", 0), "12");
        assert_eq!(env.expand("${A}${B}", 0), "12");
        assert_eq!(env.expand("$A${B}", 0), "12");
    }

    #[test]
    fn expand_variable_at_end() {
        let mut env = Environment::new();
        env.set("VAR", "value");

        assert_eq!(env.expand("prefix$VAR", 0), "prefixvalue");
        assert_eq!(env.expand("prefix${VAR}", 0), "prefixvalue");
    }

    // =========================================================================
    // Braced variables
    // =========================================================================

    #[test]
    fn expand_braced_with_suffix() {
        let mut env = Environment::new();
        env.set("BASE", "file");

        // Without braces, BASEtxt would be the variable name
        assert_eq!(env.expand("$BASEtxt", 0), ""); // BASEtxt not set

        // With braces, we get file.txt
        assert_eq!(env.expand("${BASE}.txt", 0), "file.txt");
    }

    #[test]
    fn expand_unclosed_brace() {
        let mut env = Environment::new();
        env.set("VAR", "value");

        // Unclosed brace - reads to end of string
        assert_eq!(env.expand("${VAR", 0), "value");
    }

    #[test]
    fn expand_empty_braces() {
        let env = Environment::new();
        // ${} - empty variable name
        assert_eq!(env.expand("${}", 0), "");
    }

    // =========================================================================
    // Dollar sign edge cases
    // =========================================================================

    #[test]
    fn expand_lone_dollar() {
        let env = Environment::new();
        // Lone $ at end of string
        assert_eq!(env.expand("price$", 0), "price$");
    }

    #[test]
    fn expand_dollar_space() {
        let env = Environment::new();
        // $ followed by space
        assert_eq!(env.expand("$ 100", 0), "$ 100");
    }

    #[test]
    fn expand_positional_params() {
        let mut env = Environment::new();
        // $1, $2, etc. expand to positional parameters
        assert_eq!(env.expand("$1", 0), ""); // Not set, expands to empty

        env.set("1", "first");
        env.set("2", "second");
        assert_eq!(env.expand("$1", 0), "first");
        assert_eq!(env.expand("$2", 0), "second");
        assert_eq!(env.expand("$1 and $2", 0), "first and second");
    }

    #[test]
    fn expand_special_params() {
        let mut env = Environment::new();
        // $# = number of positional params
        assert_eq!(env.expand("$#", 0), "0"); // Default

        env.set("#", "3");
        assert_eq!(env.expand("$#", 0), "3");

        // $@ and $* = all positional params
        env.set("@", "a b c");
        assert_eq!(env.expand("$@", 0), "a b c");
        assert_eq!(env.expand("$*", 0), "a b c");
    }

    #[test]
    fn expand_dollar_zero_custom() {
        let mut env = Environment::new();
        // Default $0 is "sh"
        assert_eq!(env.expand("$0", 0), "sh");

        // Can be overridden
        env.set("0", "myscript");
        assert_eq!(env.expand("$0", 0), "myscript");
    }

    #[test]
    fn expand_multiple_dollars() {
        let env = Environment::new();
        // Multiple $$ = multiple PIDs
        assert_eq!(env.expand("$$$$", 0), "11");
    }

    #[test]
    fn expand_dollar_in_value() {
        let mut env = Environment::new();
        // Value contains $
        env.set("VAR", "cost$100");
        assert_eq!(env.expand("$VAR", 0), "cost$100");
    }

    // =========================================================================
    // Complex expansion scenarios
    // =========================================================================

    #[test]
    fn expand_path_like() {
        let mut env = Environment::new();
        env.set("HOME", "/home/user");
        env.set("PROJECT", "myproject");

        assert_eq!(
            env.expand("$HOME/$PROJECT/src", 0),
            "/home/user/myproject/src"
        );
    }

    #[test]
    fn expand_command_like() {
        let mut env = Environment::new();
        env.set("CC", "gcc");
        env.set("FLAGS", "-O2 -Wall");

        assert_eq!(env.expand("$CC $FLAGS main.c", 0), "gcc -O2 -Wall main.c");
    }

    #[test]
    fn expand_mixed_special_and_regular() {
        let mut env = Environment::new();
        env.set("CMD", "mycommand");

        assert_eq!(
            env.expand("$CMD exited with $? (pid=$$)", 42),
            "mycommand exited with 42 (pid=1)"
        );
    }

    #[test]
    fn expand_no_expansion_needed() {
        let env = Environment::new();
        // String with no $ at all
        assert_eq!(env.expand("hello world 123", 0), "hello world 123");
    }

    #[test]
    fn expand_only_special_chars() {
        let env = Environment::new();
        // Only special variables
        assert_eq!(env.expand("$?$$$0", 5), "51sh");
    }

    // =========================================================================
    // Unicode handling
    // =========================================================================

    #[test]
    fn expand_unicode_value() {
        let mut env = Environment::new();
        env.set("GREETING", "こんにちは");
        assert_eq!(env.expand("$GREETING", 0), "こんにちは");
    }

    #[test]
    fn expand_unicode_around_variable() {
        let mut env = Environment::new();
        env.set("NAME", "world");
        assert_eq!(env.expand("👋 $NAME 🌍", 0), "👋 world 🌍");
    }

    // =========================================================================
    // Empty and whitespace strings
    // =========================================================================

    #[test]
    fn expand_empty_string() {
        let env = Environment::new();
        assert_eq!(env.expand("", 0), "");
    }

    #[test]
    fn expand_whitespace_only() {
        let env = Environment::new();
        assert_eq!(env.expand("   ", 0), "   ");
    }

    #[test]
    fn expand_newlines() {
        let mut env = Environment::new();
        env.set("VAR", "value");
        assert_eq!(env.expand("line1\n$VAR\nline3", 0), "line1\nvalue\nline3");
    }
}
