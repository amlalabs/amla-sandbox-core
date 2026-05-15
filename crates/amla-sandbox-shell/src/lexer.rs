//! Shell lexer (tokenizer).
//!
//! Converts input text into a stream of tokens for the parser.

use smallvec::{SmallVec, smallvec};

use crate::error::Result;

/// A segment of a word with expansion flags.
///
/// Each segment tracks whether variable and glob expansion should occur.
/// This allows proper handling of mixed quoting like: `$VAR'literal'*.txt`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSegment {
    /// The text content (quotes already removed).
    pub text: String,
    /// Whether to expand variables ($VAR, ${VAR}).
    /// False for single-quoted content.
    pub expand_vars: bool,
    /// Whether to expand globs (* ? [...]).
    /// False for both single-quoted and double-quoted content.
    pub expand_globs: bool,
    /// If Some, this segment is a command substitution.
    /// The string contains the command to execute (e.g., "date" for $(date)).
    pub command_substitution: Option<String>,
}

impl WordSegment {
    /// Create an unquoted segment (full expansion).
    pub fn unquoted(text: impl Into<String>) -> Self {
        WordSegment {
            text: text.into(),
            expand_vars: true,
            expand_globs: true,
            command_substitution: None,
        }
    }

    /// Create a single-quoted segment (no expansion).
    pub fn single_quoted(text: impl Into<String>) -> Self {
        WordSegment {
            text: text.into(),
            expand_vars: false,
            expand_globs: false,
            command_substitution: None,
        }
    }

    /// Create a double-quoted segment (variable expansion only).
    pub fn double_quoted(text: impl Into<String>) -> Self {
        WordSegment {
            text: text.into(),
            expand_vars: true,
            expand_globs: false,
            command_substitution: None,
        }
    }

    /// Create a command substitution segment.
    pub fn command_substitution(command: impl Into<String>) -> Self {
        WordSegment {
            text: String::new(),
            expand_vars: false,
            expand_globs: false,
            command_substitution: Some(command.into()),
        }
    }
}

/// A word token with expansion metadata.
///
/// A word is composed of segments, each with its own expansion flags.
/// This allows proper handling of shell quoting rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// Segments of the word (typically 1-2, inline storage avoids heap).
    pub segments: SmallVec<[WordSegment; 2]>,
}

impl Word {
    /// Create a new empty word.
    pub fn new() -> Self {
        Word {
            segments: SmallVec::new(),
        }
    }

    /// Create a word from a single unquoted segment.
    pub fn unquoted(text: impl Into<String>) -> Self {
        Word {
            segments: smallvec![WordSegment::unquoted(text)],
        }
    }

    /// Create a word from a single single-quoted segment.
    pub fn single_quoted(text: impl Into<String>) -> Self {
        Word {
            segments: smallvec![WordSegment::single_quoted(text)],
        }
    }

    /// Create a word from a single double-quoted segment.
    pub fn double_quoted(text: impl Into<String>) -> Self {
        Word {
            segments: smallvec![WordSegment::double_quoted(text)],
        }
    }

    /// Add a segment to the word.
    pub fn push(&mut self, segment: WordSegment) {
        self.segments.push(segment);
    }

    /// Get the concatenated text (for display/simple use).
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }

    /// Check if any segment should expand globs.
    pub fn has_glob_expansion(&self) -> bool {
        self.segments.iter().any(|s| s.expand_globs)
    }
}

impl Default for Word {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&str> for Word {
    fn from(s: &str) -> Self {
        Word::unquoted(s)
    }
}

impl From<String> for Word {
    fn from(s: String) -> Self {
        Word::unquoted(s)
    }
}

impl PartialEq<&str> for Word {
    fn eq(&self, other: &&str) -> bool {
        self.text() == *other
    }
}

impl PartialEq<str> for Word {
    fn eq(&self, other: &str) -> bool {
        self.text() == other
    }
}

/// A token from the lexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A word (command, argument, filename).
    Word(Word),
    /// Pipe: `|`
    Pipe,
    /// And: `&&`
    And,
    /// Or: `||`
    Or,
    /// Semicolon: `;`
    Semi,
    /// Ampersand (background): `&`
    Amp,
    /// Output redirect: `>`
    RedirectOut,
    /// Append redirect: `>>`
    RedirectAppend,
    /// Input redirect: `<`
    RedirectIn,
    /// Here document: `<<DELIM` or `<<-DELIM`
    ///
    /// Contains the heredoc content and whether to strip leading tabs.
    HereDoc {
        /// The content between the delimiter lines.
        content: String,
        /// Whether to strip leading tabs (`<<-` syntax).
        strip_tabs: bool,
    },
    /// Stderr redirect: `2>`
    RedirectErr,
    /// Stderr append: `2>>`
    RedirectErrAppend,
    /// Both stdout and stderr: `&>`
    RedirectBoth,
    /// File descriptor duplication: `N>&M` (stored as source, target)
    DupFd(i32, i32),
    /// Left parenthesis: `(`
    LParen,
    /// Right parenthesis: `)`
    RParen,
    /// Newline (command separator).
    Newline,
    /// End of input.
    Eof,
}

/// Lexer for shell input.
pub struct Lexer<'a> {
    /// Input text.
    input: &'a str,
    /// Current position in input.
    pos: usize,
    /// Peeked token (if any).
    peeked: Option<Token>,
}

impl<'a> Lexer<'a> {
    /// Create a new lexer.
    pub fn new(input: &'a str) -> Self {
        Lexer {
            input,
            pos: 0,
            peeked: None,
        }
    }

    /// Peek at the next token without consuming it.
    pub fn peek(&mut self) -> Result<&Token> {
        if self.peeked.is_none() {
            self.peeked = Some(self.next_token()?);
        }
        Ok(self.peeked.as_ref().unwrap())
    }

    /// Get the next token.
    pub fn next(&mut self) -> Result<Token> {
        if let Some(tok) = self.peeked.take() {
            return Ok(tok);
        }
        self.next_token()
    }

    /// Check if at end of input.
    pub fn at_end(&mut self) -> Result<bool> {
        Ok(matches!(self.peek()?, Token::Eof))
    }

    /// Current position in input.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Get the next token from input.
    fn next_token(&mut self) -> Result<Token> {
        self.skip_whitespace();

        if self.pos >= self.input.len() {
            return Ok(Token::Eof);
        }

        let remaining = &self.input[self.pos..];
        let c = remaining.chars().next().unwrap();

        // Check for operators (multi-char first)
        if remaining.starts_with("&&") {
            self.pos += 2;
            return Ok(Token::And);
        }
        if remaining.starts_with("||") {
            self.pos += 2;
            return Ok(Token::Or);
        }
        if remaining.starts_with(">>") {
            self.pos += 2;
            return Ok(Token::RedirectAppend);
        }
        if remaining.starts_with("&>") {
            self.pos += 2;
            return Ok(Token::RedirectBoth);
        }
        // Check for N>&M pattern (fd duplication) - must be before 2> to catch 2>&1
        if let Some(dup) = self.try_lex_fd_dup(remaining) {
            return Ok(dup);
        }

        if remaining.starts_with("2>>") {
            self.pos += 3;
            return Ok(Token::RedirectErrAppend);
        }
        if remaining.starts_with("2>") {
            self.pos += 2;
            return Ok(Token::RedirectErr);
        }

        // Check for heredoc: << or <<-
        if remaining.starts_with("<<") {
            return self.lex_heredoc();
        }

        // Single-char operators
        match c {
            '|' => {
                self.pos += 1;
                Ok(Token::Pipe)
            }
            ';' => {
                self.pos += 1;
                Ok(Token::Semi)
            }
            '&' => {
                self.pos += 1;
                Ok(Token::Amp)
            }
            '>' => {
                self.pos += 1;
                Ok(Token::RedirectOut)
            }
            '<' => {
                self.pos += 1;
                Ok(Token::RedirectIn)
            }
            '(' => {
                self.pos += 1;
                Ok(Token::LParen)
            }
            ')' => {
                self.pos += 1;
                Ok(Token::RParen)
            }
            '\n' => {
                self.pos += 1;
                Ok(Token::Newline)
            }
            '#' => {
                // Comment - skip to end of line
                while self.pos < self.input.len() {
                    if self.input[self.pos..].starts_with('\n') {
                        break;
                    }
                    self.pos += 1;
                }
                self.next_token()
            }
            // Quotes start a word (lex_word handles quote concatenation)
            '\'' | '"' => self.lex_word(),
            _ => self.lex_word(),
        }
    }

    /// Skip whitespace (but not newlines).
    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c == ' ' || c == '\t' || c == '\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Try to lex a file descriptor duplication pattern (N>&M).
    fn try_lex_fd_dup(&mut self, remaining: &str) -> Option<Token> {
        // Pattern: digit(s) >& digit(s)
        let mut chars = remaining.chars().peekable();

        // Get source fd
        let mut source_str = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                source_str.push(c);
                chars.next();
            } else {
                break;
            }
        }

        if source_str.is_empty() {
            return None;
        }

        // Check for >&
        if chars.next() != Some('>') || chars.next() != Some('&') {
            return None;
        }

        // Get target fd
        let mut target_str = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                target_str.push(c);
                chars.next();
            } else {
                break;
            }
        }

        if target_str.is_empty() {
            return None;
        }

        let source: i32 = source_str.parse().ok()?;
        let target: i32 = target_str.parse().ok()?;

        // Calculate consumed length
        let consumed = source_str.len() + 2 + target_str.len();
        self.pos += consumed;

        Some(Token::DupFd(source, target))
    }

    /// Lex a heredoc: `<<DELIM` or `<<-DELIM`
    ///
    /// After the delimiter is read, scans forward to find the content
    /// between the newline and the closing delimiter.
    ///
    /// Note: This implementation handles complete input (not interactive).
    /// For `cat <<EOF\nline1\nEOF`, the token contains "line1" and position
    /// ends after "EOF" delimiter, ready for the next command.
    fn lex_heredoc(&mut self) -> Result<Token> {
        // Skip "<<"
        self.pos += 2;

        // Check for strip_tabs flag (<<-)
        let strip_tabs = if self.input[self.pos..].starts_with('-') {
            self.pos += 1;
            true
        } else {
            false
        };

        // Skip whitespace before delimiter (but not newline)
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c == ' ' || c == '\t' {
                self.pos += 1;
            } else {
                break;
            }
        }

        // Read the delimiter (can be quoted or unquoted)
        let (delimiter, _quoted) = self.read_heredoc_delimiter()?;

        if delimiter.is_empty() {
            return Err(crate::error::ShellError::Syntax {
                message: "heredoc requires a delimiter".into(),
                position: self.pos,
            });
        }

        // Find the newline that starts the heredoc content
        let newline_pos = self.input[self.pos..].find('\n');
        let content_start = match newline_pos {
            Some(n) => self.pos + n + 1,
            None => {
                // No newline - heredoc is empty, delimiter at end of input
                return Ok(Token::HereDoc {
                    content: String::new(),
                    strip_tabs,
                });
            }
        };

        // Find the closing delimiter line
        let mut content_end = content_start;
        let mut end_pos = content_start;
        let mut found_delimiter = false;
        let remaining = &self.input[content_start..];

        for (line_start, line) in remaining.split('\n').scan(0usize, |offset, line| {
            let start = *offset;
            *offset += line.len() + 1; // +1 for the newline
            Some((start, line))
        }) {
            // Check if this line is the delimiter
            let trimmed = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };

            if trimmed == delimiter || trimmed.trim_end() == delimiter {
                content_end = content_start + line_start;
                // Calculate end position (after delimiter line)
                end_pos = content_start + line_start + line.len();
                if end_pos < self.input.len() && self.input[end_pos..].starts_with('\n') {
                    end_pos += 1;
                }
                found_delimiter = true;
                break;
            }
        }

        if !found_delimiter {
            return Err(crate::error::ShellError::Syntax {
                message: format!("heredoc delimiter '{delimiter}' not found"),
                position: self.pos,
            });
        }

        // Extract content
        let mut content = self.input[content_start..content_end].to_string();

        // Strip leading tabs if <<- was used
        if strip_tabs {
            content = content
                .lines()
                .map(|line| line.trim_start_matches('\t'))
                .collect::<Vec<_>>()
                .join("\n");
        }

        // Remove trailing newline if present
        if content.ends_with('\n') {
            content.pop();
        }

        // Position stays at after_delimiter_pos so rest of command line can be lexed
        // But we need to track that content extends to end_pos
        // For simplicity in non-interactive mode, we'll skip the content entirely
        // The heredoc content is already captured in the token
        self.pos = end_pos;

        Ok(Token::HereDoc {
            content,
            strip_tabs,
        })
    }

    /// Read a heredoc delimiter (handles quoting).
    ///
    /// Returns (delimiter, is_quoted).
    /// Quoted delimiters prevent variable expansion in content.
    fn read_heredoc_delimiter(&mut self) -> Result<(String, bool)> {
        if self.pos >= self.input.len() {
            return Ok((String::new(), false));
        }

        let c = self.input[self.pos..].chars().next().unwrap();

        // Check for quoted delimiter
        if c == '\'' || c == '"' {
            let quote = c;
            self.pos += 1;
            let mut delimiter = String::new();

            while self.pos < self.input.len() {
                let c = self.input[self.pos..].chars().next().unwrap();
                if c == quote {
                    self.pos += 1;
                    return Ok((delimiter, true));
                }
                delimiter.push(c);
                self.pos += c.len_utf8();
            }

            // Unclosed quote
            return Err(crate::error::ShellError::Syntax {
                message: "unclosed quote in heredoc delimiter".into(),
                position: self.pos,
            });
        }

        // Unquoted delimiter - read until whitespace or newline
        let mut delimiter = String::new();
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c.is_whitespace() || c == '\n' {
                break;
            }
            delimiter.push(c);
            self.pos += c.len_utf8();
        }

        Ok((delimiter, false))
    }

    /// Lex a command substitution content after seeing "$(".
    ///
    /// Tracks parenthesis depth to handle nested substitutions.
    /// Returns the command string (without the $( and )).
    fn lex_command_substitution(&mut self) -> String {
        let mut content = String::new();
        let mut depth = 1; // We've already consumed $(

        while self.pos < self.input.len() && depth > 0 {
            let c = self.input[self.pos..].chars().next().unwrap();

            match c {
                '(' => {
                    depth += 1;
                    content.push(c);
                    self.pos += 1;
                }
                ')' => {
                    depth -= 1;
                    if depth > 0 {
                        content.push(c);
                    }
                    self.pos += 1;
                }
                '$' if self.input[self.pos..].starts_with("$(") => {
                    // Nested command substitution
                    content.push_str("$(");
                    self.pos += 2;
                    depth += 1;
                }
                '\'' => {
                    // Single quotes - read until closing quote
                    content.push(c);
                    self.pos += 1;
                    while self.pos < self.input.len() {
                        let qc = self.input[self.pos..].chars().next().unwrap();
                        content.push(qc);
                        self.pos += qc.len_utf8();
                        if qc == '\'' {
                            break;
                        }
                    }
                }
                '"' => {
                    // Double quotes - read until closing quote (handle escapes)
                    content.push(c);
                    self.pos += 1;
                    while self.pos < self.input.len() {
                        let qc = self.input[self.pos..].chars().next().unwrap();
                        content.push(qc);
                        self.pos += qc.len_utf8();
                        if qc == '"' {
                            break;
                        }
                        if qc == '\\' && self.pos < self.input.len() {
                            let esc = self.input[self.pos..].chars().next().unwrap();
                            content.push(esc);
                            self.pos += esc.len_utf8();
                        }
                    }
                }
                '\\' => {
                    // Escape
                    content.push(c);
                    self.pos += 1;
                    if self.pos < self.input.len() {
                        let esc = self.input[self.pos..].chars().next().unwrap();
                        content.push(esc);
                        self.pos += esc.len_utf8();
                    }
                }
                _ => {
                    content.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }

        content
    }

    /// Lex a word (handles unquoted, single-quoted, and double-quoted content).
    ///
    /// Returns a Word with segments that track expansion semantics:
    /// - Unquoted: `expand_vars=true`, `expand_globs=true`
    /// - Single-quoted: `expand_vars=false`, `expand_globs=false`
    /// - Double-quoted: `expand_vars=true`, `expand_globs=false`
    fn lex_word(&mut self) -> Result<Token> {
        let mut word = Word::new();
        let mut current_text = String::new();
        let mut current_expand_vars = true;
        let mut current_expand_globs = true;

        // Helper to flush current segment if non-empty
        let flush_segment =
            |word: &mut Word, text: &mut String, expand_vars: bool, expand_globs: bool| {
                if !text.is_empty() {
                    word.push(WordSegment {
                        text: std::mem::take(text),
                        expand_vars,
                        expand_globs,
                        command_substitution: None,
                    });
                }
            };

        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();

            match c {
                // Word terminators
                ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | '>' | '<' | '(' | ')' => break,

                // Escape - escaped char is literal but still in current context
                '\\' => {
                    self.pos += 1;
                    if self.pos < self.input.len() {
                        let escaped = self.input[self.pos..].chars().next().unwrap();
                        current_text.push(escaped);
                        self.pos += escaped.len_utf8();
                    }
                }

                // Single-quoted section (no expansion at all)
                '\'' => {
                    // Flush current unquoted segment
                    flush_segment(
                        &mut word,
                        &mut current_text,
                        current_expand_vars,
                        current_expand_globs,
                    );

                    self.pos += 1;
                    let mut quoted_text = String::new();
                    while self.pos < self.input.len() {
                        let qc = self.input[self.pos..].chars().next().unwrap();
                        if qc == '\'' {
                            self.pos += 1;
                            break;
                        }
                        quoted_text.push(qc);
                        self.pos += qc.len_utf8();
                    }
                    // Always push segment, even if empty (for '' to produce empty arg)
                    word.push(WordSegment::single_quoted(quoted_text));

                    // Reset to unquoted mode
                    current_expand_vars = true;
                    current_expand_globs = true;
                }

                // Double-quoted section (variable expansion, no glob)
                '"' => {
                    // Flush current unquoted segment
                    flush_segment(
                        &mut word,
                        &mut current_text,
                        current_expand_vars,
                        current_expand_globs,
                    );

                    self.pos += 1;
                    let mut quoted_text = String::new();
                    while self.pos < self.input.len() {
                        let qc = self.input[self.pos..].chars().next().unwrap();
                        match qc {
                            '"' => {
                                self.pos += 1;
                                break;
                            }
                            '$' if self.input[self.pos..].starts_with("$(") => {
                                // Command substitution inside double quotes
                                // Flush current quoted text first
                                if !quoted_text.is_empty() {
                                    word.push(WordSegment::double_quoted(std::mem::take(
                                        &mut quoted_text,
                                    )));
                                }
                                // Skip "$("
                                self.pos += 2;
                                let cmd = self.lex_command_substitution();
                                word.push(WordSegment::command_substitution(cmd));
                            }
                            '\\' => {
                                self.pos += 1;
                                if self.pos < self.input.len() {
                                    let esc = self.input[self.pos..].chars().next().unwrap();
                                    // In double quotes, only $, `, ", \, and newline are special
                                    // after backslash. For other chars, preserve the backslash.
                                    match esc {
                                        '$' | '`' | '"' | '\\' | '\n' => {
                                            quoted_text.push(esc);
                                        }
                                        _ => {
                                            quoted_text.push('\\');
                                            quoted_text.push(esc);
                                        }
                                    }
                                    self.pos += esc.len_utf8();
                                }
                            }
                            _ => {
                                quoted_text.push(qc);
                                self.pos += qc.len_utf8();
                            }
                        }
                    }
                    // Always push segment, even if empty (for "" to produce empty arg)
                    word.push(WordSegment::double_quoted(quoted_text));

                    // Reset to unquoted mode
                    current_expand_vars = true;
                    current_expand_globs = true;
                }

                // Command substitution $(...)
                '$' if self.input[self.pos..].starts_with("$(") => {
                    // Flush current segment first
                    flush_segment(
                        &mut word,
                        &mut current_text,
                        current_expand_vars,
                        current_expand_globs,
                    );

                    // Skip "$("
                    self.pos += 2;

                    // Extract command with balanced parentheses
                    let cmd = self.lex_command_substitution();
                    word.push(WordSegment::command_substitution(cmd));

                    // Continue in unquoted mode
                    current_expand_vars = true;
                    current_expand_globs = true;
                }

                // Regular character in unquoted context
                _ => {
                    current_text.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }

        // Flush any remaining unquoted content
        flush_segment(
            &mut word,
            &mut current_text,
            current_expand_vars,
            current_expand_globs,
        );

        Ok(Token::Word(word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper function to tokenize input and return all tokens (excluding Eof).
    fn tokens(input: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(input);
        let mut toks = Vec::new();
        loop {
            let tok = lexer.next().unwrap();
            if tok == Token::Eof {
                break;
            }
            toks.push(tok);
        }
        toks
    }

    // =========================================================================
    // 1. BASIC TOKENIZATION (words, operators, quotes)
    // =========================================================================

    mod basic_tokenization {
        use super::*;

        #[test]
        fn simple_command() {
            assert_eq!(
                tokens("ls -la /tmp"),
                vec![
                    Token::Word("ls".into()),
                    Token::Word("-la".into()),
                    Token::Word("/tmp".into()),
                ]
            );
        }

        #[test]
        fn single_word() {
            assert_eq!(tokens("echo"), vec![Token::Word("echo".into())]);
        }

        #[test]
        fn words_with_various_separators() {
            // Multiple spaces
            assert_eq!(
                tokens("cmd1   cmd2"),
                vec![Token::Word("cmd1".into()), Token::Word("cmd2".into())]
            );
            // Tabs
            assert_eq!(
                tokens("cmd1\tcmd2"),
                vec![Token::Word("cmd1".into()), Token::Word("cmd2".into())]
            );
            // Mixed whitespace
            assert_eq!(
                tokens("cmd1 \t cmd2"),
                vec![Token::Word("cmd1".into()), Token::Word("cmd2".into())]
            );
        }

        #[test]
        fn words_with_numbers() {
            assert_eq!(
                tokens("test123 456test 7890"),
                vec![
                    Token::Word("test123".into()),
                    Token::Word("456test".into()),
                    Token::Word("7890".into()),
                ]
            );
        }

        #[test]
        fn words_with_underscores_and_dashes() {
            assert_eq!(
                tokens("my_var my-var --flag _underscore"),
                vec![
                    Token::Word("my_var".into()),
                    Token::Word("my-var".into()),
                    Token::Word("--flag".into()),
                    Token::Word("_underscore".into()),
                ]
            );
        }

        #[test]
        fn words_with_dots() {
            assert_eq!(
                tokens("file.txt path/to/file.rs"),
                vec![
                    Token::Word("file.txt".into()),
                    Token::Word("path/to/file.rs".into()),
                ]
            );
        }

        #[test]
        fn words_with_special_path_chars() {
            assert_eq!(
                tokens("/usr/bin/cmd ./relative ../parent ~"),
                vec![
                    Token::Word("/usr/bin/cmd".into()),
                    Token::Word("./relative".into()),
                    Token::Word("../parent".into()),
                    Token::Word("~".into()),
                ]
            );
        }

        #[test]
        fn words_with_equals() {
            assert_eq!(
                tokens("VAR=value key=val"),
                vec![
                    Token::Word("VAR=value".into()),
                    Token::Word("key=val".into()),
                ]
            );
        }

        #[test]
        fn words_with_colons() {
            assert_eq!(
                tokens("PATH:/usr/bin:$HOME/bin"),
                vec![Token::Word("PATH:/usr/bin:$HOME/bin".into())]
            );
        }
    }

    // =========================================================================
    // 2. REDIRECT PARSING (>, >>, <, 2>, 2>&1, &>)
    // =========================================================================

    mod redirects {
        use super::*;

        #[test]
        fn stdout_redirect() {
            assert_eq!(
                tokens("echo hello > out.txt"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn stdout_redirect_no_space() {
            assert_eq!(
                tokens("echo hello >out.txt"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn append_redirect() {
            assert_eq!(
                tokens("echo hello >> out.txt"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello".into()),
                    Token::RedirectAppend,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn append_redirect_no_space() {
            assert_eq!(
                tokens("echo hello >>out.txt"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello".into()),
                    Token::RedirectAppend,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn input_redirect() {
            assert_eq!(
                tokens("cat < input.txt"),
                vec![
                    Token::Word("cat".into()),
                    Token::RedirectIn,
                    Token::Word("input.txt".into()),
                ]
            );
        }

        #[test]
        fn input_redirect_no_space() {
            assert_eq!(
                tokens("cat <input.txt"),
                vec![
                    Token::Word("cat".into()),
                    Token::RedirectIn,
                    Token::Word("input.txt".into()),
                ]
            );
        }

        #[test]
        fn stderr_redirect() {
            assert_eq!(
                tokens("cmd 2> err.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectErr,
                    Token::Word("err.txt".into()),
                ]
            );
        }

        #[test]
        fn stderr_redirect_no_space() {
            assert_eq!(
                tokens("cmd 2>err.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectErr,
                    Token::Word("err.txt".into()),
                ]
            );
        }

        #[test]
        fn stderr_append() {
            assert_eq!(
                tokens("cmd 2>> err.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectErrAppend,
                    Token::Word("err.txt".into()),
                ]
            );
        }

        #[test]
        fn stderr_append_no_space() {
            assert_eq!(
                tokens("cmd 2>>err.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectErrAppend,
                    Token::Word("err.txt".into()),
                ]
            );
        }

        #[test]
        fn redirect_both_stdout_and_stderr() {
            assert_eq!(
                tokens("cmd &> out.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectBoth,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn redirect_both_no_space() {
            assert_eq!(
                tokens("cmd &>out.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectBoth,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn fd_duplication_2_to_1() {
            assert_eq!(
                tokens("cmd 2>&1"),
                vec![Token::Word("cmd".into()), Token::DupFd(2, 1)]
            );
        }

        #[test]
        fn fd_duplication_1_to_2() {
            assert_eq!(
                tokens("cmd 1>&2"),
                vec![Token::Word("cmd".into()), Token::DupFd(1, 2)]
            );
        }

        #[test]
        fn fd_duplication_arbitrary_fds() {
            assert_eq!(
                tokens("cmd 3>&4"),
                vec![Token::Word("cmd".into()), Token::DupFd(3, 4)]
            );
        }

        #[test]
        fn fd_duplication_larger_numbers() {
            assert_eq!(
                tokens("cmd 10>&20"),
                vec![Token::Word("cmd".into()), Token::DupFd(10, 20)]
            );
        }

        #[test]
        fn combined_redirects() {
            assert_eq!(
                tokens("cat < in.txt > out.txt"),
                vec![
                    Token::Word("cat".into()),
                    Token::RedirectIn,
                    Token::Word("in.txt".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn complex_redirect_chain() {
            assert_eq!(
                tokens("cmd < in.txt > out.txt 2>&1"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectIn,
                    Token::Word("in.txt".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                    Token::DupFd(2, 1),
                ]
            );
        }

        #[test]
        fn multiple_output_redirects() {
            assert_eq!(
                tokens("cmd > out.txt 2> err.txt"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                    Token::RedirectErr,
                    Token::Word("err.txt".into()),
                ]
            );
        }

        #[test]
        fn redirect_to_dev_null() {
            assert_eq!(
                tokens("cmd > /dev/null 2>&1"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectOut,
                    Token::Word("/dev/null".into()),
                    Token::DupFd(2, 1),
                ]
            );
        }
    }

    // =========================================================================
    // 3. PIPELINE PARSING (|)
    // =========================================================================

    mod pipelines {
        use super::*;

        #[test]
        fn simple_pipeline() {
            assert_eq!(
                tokens("cat file | grep foo"),
                vec![
                    Token::Word("cat".into()),
                    Token::Word("file".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Word("foo".into()),
                ]
            );
        }

        #[test]
        fn three_stage_pipeline() {
            assert_eq!(
                tokens("cat file | grep foo | wc -l"),
                vec![
                    Token::Word("cat".into()),
                    Token::Word("file".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Word("foo".into()),
                    Token::Pipe,
                    Token::Word("wc".into()),
                    Token::Word("-l".into()),
                ]
            );
        }

        #[test]
        fn pipeline_no_spaces() {
            assert_eq!(
                tokens("cat|grep|wc"),
                vec![
                    Token::Word("cat".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Pipe,
                    Token::Word("wc".into()),
                ]
            );
        }

        #[test]
        fn pipeline_mixed_spacing() {
            assert_eq!(
                tokens("cat |grep| wc"),
                vec![
                    Token::Word("cat".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Pipe,
                    Token::Word("wc".into()),
                ]
            );
        }

        #[test]
        fn pipeline_with_args() {
            assert_eq!(
                tokens("ps aux | grep -v grep | awk '{print $2}'"),
                vec![
                    Token::Word("ps".into()),
                    Token::Word("aux".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Word("-v".into()),
                    Token::Word("grep".into()),
                    Token::Pipe,
                    Token::Word("awk".into()),
                    Token::Word(Word::single_quoted("{print $2}")),
                ]
            );
        }

        #[test]
        fn long_pipeline() {
            assert_eq!(
                tokens("a | b | c | d | e | f"),
                vec![
                    Token::Word("a".into()),
                    Token::Pipe,
                    Token::Word("b".into()),
                    Token::Pipe,
                    Token::Word("c".into()),
                    Token::Pipe,
                    Token::Word("d".into()),
                    Token::Pipe,
                    Token::Word("e".into()),
                    Token::Pipe,
                    Token::Word("f".into()),
                ]
            );
        }

        #[test]
        fn pipeline_with_redirect() {
            assert_eq!(
                tokens("cat file | grep foo > out.txt"),
                vec![
                    Token::Word("cat".into()),
                    Token::Word("file".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Word("foo".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                ]
            );
        }
    }

    // =========================================================================
    // 4. QUOTE HANDLING (single quotes, double quotes, escaping)
    // =========================================================================

    mod quotes {
        use super::*;

        #[test]
        fn single_quoted_simple() {
            assert_eq!(
                tokens("echo 'hello world'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("hello world")),
                ]
            );
        }

        #[test]
        fn single_quoted_with_special_chars() {
            // Single quotes preserve all characters literally
            assert_eq!(
                tokens("echo 'hello | world && test'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("hello | world && test")),
                ]
            );
        }

        #[test]
        fn single_quoted_empty() {
            assert_eq!(
                tokens("echo ''"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted(""))
                ]
            );
        }

        #[test]
        fn double_quoted_simple() {
            assert_eq!(
                tokens(r#"echo "hello world""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted("hello world")),
                ]
            );
        }

        #[test]
        fn double_quoted_with_special_chars() {
            // Double quotes preserve special characters
            assert_eq!(
                tokens(r#"echo "hello | world && test""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted("hello | world && test")),
                ]
            );
        }

        #[test]
        fn double_quoted_empty() {
            assert_eq!(
                tokens(r#"echo """#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted(""))
                ]
            );
        }

        #[test]
        fn double_quoted_with_escape() {
            assert_eq!(
                tokens(r#"echo "hello\"world""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted(r#"hello"world"#)),
                ]
            );
        }

        #[test]
        fn double_quoted_with_backslash() {
            assert_eq!(
                tokens(r#"echo "hello\\world""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted(r"hello\world")),
                ]
            );
        }

        #[test]
        fn double_quoted_preserves_backslash_n() {
            // In bash, \n in double quotes is preserved as literal \n (not a newline)
            // Only $, `, ", \, and newline are special after backslash in double quotes
            assert_eq!(
                tokens(r#"echo "hello\nworld""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted(r"hello\nworld")),
                ]
            );
        }

        #[test]
        fn double_quoted_preserves_backslash_t() {
            // \t in double quotes should be preserved as literal \t
            assert_eq!(
                tokens(r#"echo "hello\tworld""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted(r"hello\tworld")),
                ]
            );
        }

        #[test]
        fn unquoted_escape_space() {
            assert_eq!(
                tokens(r"echo hello\ world"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello world".into()),
                ]
            );
        }

        #[test]
        fn unquoted_escape_special() {
            assert_eq!(
                tokens(r"echo hello\|world"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello|world".into()),
                ]
            );
        }

        #[test]
        fn unquoted_escape_backslash() {
            assert_eq!(
                tokens(r"echo hello\\world"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(r"hello\world".into()),
                ]
            );
        }

        #[test]
        fn mixed_quotes_concatenated() {
            // 'hello' is single-quoted, "world" is double-quoted
            // Result is a word with two segments
            let expected_word = {
                let mut w = Word::new();
                w.push(WordSegment::single_quoted("hello"));
                w.push(WordSegment::double_quoted("world"));
                w
            };
            assert_eq!(
                tokens(r#"echo 'hello'"world""#),
                vec![Token::Word("echo".into()), Token::Word(expected_word),]
            );
        }

        #[test]
        fn mixed_quotes_with_unquoted() {
            // pre is unquoted, 'middle' is single-quoted, "post" is double-quoted
            let expected_word = {
                let mut w = Word::new();
                w.push(WordSegment::unquoted("pre"));
                w.push(WordSegment::single_quoted("middle"));
                w.push(WordSegment::double_quoted("post"));
                w
            };
            assert_eq!(
                tokens(r#"echo pre'middle'"post""#),
                vec![Token::Word("echo".into()), Token::Word(expected_word),]
            );
        }

        #[test]
        fn single_quotes_preserve_double() {
            assert_eq!(
                tokens(r#"echo 'hello "world"'"#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted(r#"hello "world""#)),
                ]
            );
        }

        #[test]
        fn double_quotes_preserve_single() {
            assert_eq!(
                tokens(r#"echo "hello 'world'""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted("hello 'world'")),
                ]
            );
        }

        #[test]
        fn single_quotes_preserve_backslash() {
            // In single quotes, backslash is literal
            assert_eq!(
                tokens(r"echo 'hello\nworld'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted(r"hello\nworld")),
                ]
            );
        }

        #[test]
        fn multiple_quoted_args() {
            assert_eq!(
                tokens(r#"cmd 'arg1' "arg2" arg3"#),
                vec![
                    Token::Word("cmd".into()),
                    Token::Word(Word::single_quoted("arg1")),
                    Token::Word(Word::double_quoted("arg2")),
                    Token::Word("arg3".into()),
                ]
            );
        }

        #[test]
        fn quotes_with_newlines_inside() {
            assert_eq!(
                tokens("echo 'hello\nworld'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("hello\nworld")),
                ]
            );
        }

        #[test]
        fn quotes_with_tabs_inside() {
            assert_eq!(
                tokens("echo 'hello\tworld'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("hello\tworld")),
                ]
            );
        }
    }

    // =========================================================================
    // 5. VARIABLE EXPANSION SYNTAX ($VAR, ${VAR})
    // =========================================================================

    mod variable_syntax {
        use super::*;

        // Note: The lexer tokenizes the variable syntax as words.
        // Variable expansion happens at a later parsing/execution stage.

        #[test]
        fn simple_variable() {
            assert_eq!(
                tokens("echo $VAR"),
                vec![Token::Word("echo".into()), Token::Word("$VAR".into())]
            );
        }

        #[test]
        fn braced_variable() {
            assert_eq!(
                tokens("echo ${VAR}"),
                vec![Token::Word("echo".into()), Token::Word("${VAR}".into())]
            );
        }

        #[test]
        fn variable_in_word() {
            assert_eq!(
                tokens("echo prefix$VAR"),
                vec![Token::Word("echo".into()), Token::Word("prefix$VAR".into()),]
            );
        }

        #[test]
        fn variable_with_suffix() {
            assert_eq!(
                tokens("echo $VAR/suffix"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("$VAR/suffix".into()),
                ]
            );
        }

        #[test]
        fn braced_variable_with_suffix() {
            assert_eq!(
                tokens("echo ${VAR}suffix"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("${VAR}suffix".into()),
                ]
            );
        }

        #[test]
        fn multiple_variables() {
            assert_eq!(
                tokens("echo $VAR1 $VAR2 ${VAR3}"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("$VAR1".into()),
                    Token::Word("$VAR2".into()),
                    Token::Word("${VAR3}".into()),
                ]
            );
        }

        #[test]
        fn variable_in_double_quotes() {
            assert_eq!(
                tokens(r#"echo "$VAR""#),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted("$VAR"))
                ]
            );
        }

        #[test]
        fn variable_in_single_quotes_preserved() {
            // Single quotes preserve $ literally
            assert_eq!(
                tokens("echo '$VAR'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("$VAR"))
                ]
            );
        }

        #[test]
        fn special_variables() {
            assert_eq!(
                tokens("echo $? $$ $! $0 $1 $@"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("$?".into()),
                    Token::Word("$$".into()),
                    Token::Word("$!".into()),
                    Token::Word("$0".into()),
                    Token::Word("$1".into()),
                    Token::Word("$@".into()),
                ]
            );
        }

        #[test]
        fn variable_with_default() {
            assert_eq!(
                tokens("echo ${VAR:-default}"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("${VAR:-default}".into()),
                ]
            );
        }

        #[test]
        fn variable_concatenation() {
            assert_eq!(
                tokens("echo $VAR1$VAR2"),
                vec![Token::Word("echo".into()), Token::Word("$VAR1$VAR2".into()),]
            );
        }
    }

    // =========================================================================
    // 6. SUBSHELL SYNTAX (command) - parentheses
    // =========================================================================

    mod subshell {
        use super::*;

        #[test]
        fn simple_subshell() {
            assert_eq!(
                tokens("(cmd)"),
                vec![Token::LParen, Token::Word("cmd".into()), Token::RParen,]
            );
        }

        #[test]
        fn subshell_with_args() {
            assert_eq!(
                tokens("(echo hello)"),
                vec![
                    Token::LParen,
                    Token::Word("echo".into()),
                    Token::Word("hello".into()),
                    Token::RParen,
                ]
            );
        }

        #[test]
        fn subshell_with_pipeline() {
            assert_eq!(
                tokens("(cmd1 | cmd2)"),
                vec![
                    Token::LParen,
                    Token::Word("cmd1".into()),
                    Token::Pipe,
                    Token::Word("cmd2".into()),
                    Token::RParen,
                ]
            );
        }

        #[test]
        fn nested_subshells() {
            assert_eq!(
                tokens("((cmd))"),
                vec![
                    Token::LParen,
                    Token::LParen,
                    Token::Word("cmd".into()),
                    Token::RParen,
                    Token::RParen,
                ]
            );
        }

        #[test]
        fn subshell_in_pipeline() {
            assert_eq!(
                tokens("(cmd1) | (cmd2)"),
                vec![
                    Token::LParen,
                    Token::Word("cmd1".into()),
                    Token::RParen,
                    Token::Pipe,
                    Token::LParen,
                    Token::Word("cmd2".into()),
                    Token::RParen,
                ]
            );
        }

        #[test]
        fn subshell_with_redirects() {
            assert_eq!(
                tokens("(cmd) > out.txt"),
                vec![
                    Token::LParen,
                    Token::Word("cmd".into()),
                    Token::RParen,
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                ]
            );
        }

        #[test]
        fn subshell_with_and_or() {
            assert_eq!(
                tokens("(cmd1 && cmd2)"),
                vec![
                    Token::LParen,
                    Token::Word("cmd1".into()),
                    Token::And,
                    Token::Word("cmd2".into()),
                    Token::RParen,
                ]
            );
        }

        #[test]
        fn subshell_no_spaces() {
            assert_eq!(
                tokens("(cmd1|cmd2)"),
                vec![
                    Token::LParen,
                    Token::Word("cmd1".into()),
                    Token::Pipe,
                    Token::Word("cmd2".into()),
                    Token::RParen,
                ]
            );
        }

        #[test]
        fn complex_subshell() {
            assert_eq!(
                tokens("(cd /tmp && ls -la | grep foo)"),
                vec![
                    Token::LParen,
                    Token::Word("cd".into()),
                    Token::Word("/tmp".into()),
                    Token::And,
                    Token::Word("ls".into()),
                    Token::Word("-la".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Word("foo".into()),
                    Token::RParen,
                ]
            );
        }
    }

    // =========================================================================
    // 7. AND/OR OPERATORS (&& and ||)
    // =========================================================================

    mod and_or_operators {
        use super::*;

        #[test]
        fn simple_and() {
            assert_eq!(
                tokens("cmd1 && cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::And,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn simple_or() {
            assert_eq!(
                tokens("cmd1 || cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Or,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn chained_and() {
            assert_eq!(
                tokens("cmd1 && cmd2 && cmd3"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::And,
                    Token::Word("cmd2".into()),
                    Token::And,
                    Token::Word("cmd3".into()),
                ]
            );
        }

        #[test]
        fn chained_or() {
            assert_eq!(
                tokens("cmd1 || cmd2 || cmd3"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Or,
                    Token::Word("cmd2".into()),
                    Token::Or,
                    Token::Word("cmd3".into()),
                ]
            );
        }

        #[test]
        fn mixed_and_or() {
            assert_eq!(
                tokens("cmd1 && cmd2 || cmd3"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::And,
                    Token::Word("cmd2".into()),
                    Token::Or,
                    Token::Word("cmd3".into()),
                ]
            );
        }

        #[test]
        fn and_no_spaces() {
            assert_eq!(
                tokens("cmd1&&cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::And,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn or_no_spaces() {
            assert_eq!(
                tokens("cmd1||cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Or,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn and_or_with_args() {
            assert_eq!(
                tokens("mkdir -p dir && cd dir || echo failed"),
                vec![
                    Token::Word("mkdir".into()),
                    Token::Word("-p".into()),
                    Token::Word("dir".into()),
                    Token::And,
                    Token::Word("cd".into()),
                    Token::Word("dir".into()),
                    Token::Or,
                    Token::Word("echo".into()),
                    Token::Word("failed".into()),
                ]
            );
        }

        #[test]
        fn and_with_redirects() {
            assert_eq!(
                tokens("cmd1 > out && cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::RedirectOut,
                    Token::Word("out".into()),
                    Token::And,
                    Token::Word("cmd2".into()),
                ]
            );
        }
    }

    // =========================================================================
    // 8. BACKGROUND OPERATOR (&)
    // =========================================================================

    mod background_operator {
        use super::*;

        #[test]
        fn simple_background() {
            assert_eq!(
                tokens("sleep 10 &"),
                vec![
                    Token::Word("sleep".into()),
                    Token::Word("10".into()),
                    Token::Amp,
                ]
            );
        }

        #[test]
        fn background_no_space() {
            assert_eq!(tokens("cmd&"), vec![Token::Word("cmd".into()), Token::Amp]);
        }

        #[test]
        fn multiple_background() {
            assert_eq!(
                tokens("cmd1 & cmd2 &"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Amp,
                    Token::Word("cmd2".into()),
                    Token::Amp,
                ]
            );
        }

        #[test]
        fn background_with_redirects() {
            assert_eq!(
                tokens("cmd > out.txt &"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectOut,
                    Token::Word("out.txt".into()),
                    Token::Amp,
                ]
            );
        }

        #[test]
        fn background_with_pipeline() {
            assert_eq!(
                tokens("cmd1 | cmd2 &"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Pipe,
                    Token::Word("cmd2".into()),
                    Token::Amp,
                ]
            );
        }

        #[test]
        fn background_and_then_command() {
            // &; starts another command after background
            assert_eq!(
                tokens("cmd1 & cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Amp,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn amp_vs_and() {
            // Distinguish & from &&
            assert_eq!(
                tokens("cmd1 & && cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Amp,
                    Token::And,
                    Token::Word("cmd2".into()),
                ]
            );
        }
    }

    // =========================================================================
    // 9. SEMICOLON COMMAND SEPARATOR
    // =========================================================================

    mod semicolon_separator {
        use super::*;

        #[test]
        fn simple_semicolon() {
            assert_eq!(
                tokens("cmd1 ; cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn semicolon_no_spaces() {
            assert_eq!(
                tokens("cmd1;cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn multiple_semicolons() {
            assert_eq!(
                tokens("cmd1 ; cmd2 ; cmd3"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                    Token::Semi,
                    Token::Word("cmd3".into()),
                ]
            );
        }

        #[test]
        fn semicolon_with_args() {
            assert_eq!(
                tokens("echo hello ; echo world"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello".into()),
                    Token::Semi,
                    Token::Word("echo".into()),
                    Token::Word("world".into()),
                ]
            );
        }

        #[test]
        fn semicolon_trailing() {
            assert_eq!(
                tokens("cmd1 ; cmd2 ;"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                    Token::Semi,
                ]
            );
        }

        #[test]
        fn semicolon_with_redirects() {
            assert_eq!(
                tokens("cmd1 > out ; cmd2 < in"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::RedirectOut,
                    Token::Word("out".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                    Token::RedirectIn,
                    Token::Word("in".into()),
                ]
            );
        }

        #[test]
        fn semicolon_and_background() {
            assert_eq!(
                tokens("cmd1 & ; cmd2"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Amp,
                    Token::Semi,
                    Token::Word("cmd2".into()),
                ]
            );
        }

        #[test]
        fn semicolon_vs_newline() {
            // Both are command separators
            assert_eq!(
                tokens("cmd1 ; cmd2\ncmd3"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                    Token::Newline,
                    Token::Word("cmd3".into()),
                ]
            );
        }
    }

    // =========================================================================
    // 10. EDGE CASES (empty input, whitespace, special characters)
    // =========================================================================

    mod edge_cases {
        use super::*;

        #[test]
        fn empty_input() {
            assert_eq!(tokens(""), Vec::<Token>::new());
        }

        #[test]
        fn only_whitespace() {
            assert_eq!(tokens("   "), Vec::<Token>::new());
            assert_eq!(tokens("\t\t"), Vec::<Token>::new());
            assert_eq!(tokens("  \t  "), Vec::<Token>::new());
        }

        #[test]
        fn only_newlines() {
            assert_eq!(tokens("\n\n"), vec![Token::Newline, Token::Newline]);
        }

        #[test]
        fn whitespace_and_newlines() {
            assert_eq!(tokens("  \n  \n  "), vec![Token::Newline, Token::Newline]);
        }

        #[test]
        fn comment_only() {
            assert_eq!(tokens("# this is a comment"), Vec::<Token>::new());
        }

        #[test]
        fn command_with_comment() {
            assert_eq!(
                tokens("cmd # this is a comment"),
                vec![Token::Word("cmd".into())]
            );
        }

        #[test]
        fn comment_preserves_newline() {
            assert_eq!(
                tokens("cmd # comment\nnext"),
                vec![
                    Token::Word("cmd".into()),
                    Token::Newline,
                    Token::Word("next".into()),
                ]
            );
        }

        #[test]
        fn unicode_in_words() {
            assert_eq!(
                tokens("echo hello_world"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word("hello_world".into()),
                ]
            );
        }

        #[test]
        fn unicode_characters() {
            assert_eq!(
                tokens("echo cafe"),
                vec![Token::Word("echo".into()), Token::Word("cafe".into())]
            );
        }

        #[test]
        fn unicode_in_quotes() {
            assert_eq!(
                tokens("echo 'hello world'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("hello world")),
                ]
            );
        }

        #[test]
        fn single_special_char() {
            assert_eq!(tokens("|"), vec![Token::Pipe]);
            assert_eq!(tokens(";"), vec![Token::Semi]);
            assert_eq!(tokens("&"), vec![Token::Amp]);
            assert_eq!(tokens(">"), vec![Token::RedirectOut]);
            assert_eq!(tokens("<"), vec![Token::RedirectIn]);
            assert_eq!(tokens("("), vec![Token::LParen]);
            assert_eq!(tokens(")"), vec![Token::RParen]);
        }

        #[test]
        fn consecutive_operators() {
            assert_eq!(tokens("||&&"), vec![Token::Or, Token::And]);
        }

        #[test]
        fn operators_and_words() {
            assert_eq!(
                tokens("|a|b|"),
                vec![
                    Token::Pipe,
                    Token::Word("a".into()),
                    Token::Pipe,
                    Token::Word("b".into()),
                    Token::Pipe,
                ]
            );
        }

        #[test]
        fn very_long_word() {
            let long_word = "a".repeat(1000);
            assert_eq!(tokens(&long_word), vec![Token::Word(long_word.into())]);
        }

        #[test]
        fn word_ending_with_number() {
            // Make sure 2 isn't treated as fd for redirect
            assert_eq!(
                tokens("file2 file3"),
                vec![Token::Word("file2".into()), Token::Word("file3".into()),]
            );
        }

        #[test]
        fn number_as_argument() {
            assert_eq!(
                tokens("chmod 755 file"),
                vec![
                    Token::Word("chmod".into()),
                    Token::Word("755".into()),
                    Token::Word("file".into()),
                ]
            );
        }

        #[test]
        fn escaped_newline() {
            // Backslash-n escapes the 'n' character, making it part of the word
            // (not a newline escape sequence like in C strings)
            assert_eq!(tokens(r"cmd\narg"), vec![Token::Word("cmdnarg".into())]);
        }

        #[test]
        fn hash_in_word() {
            // # is only a comment starter at the beginning of a token
            // When it appears mid-word, it's part of the word
            assert_eq!(tokens("file#tag"), vec![Token::Word("file#tag".into())]);
        }

        #[test]
        fn hash_in_quotes() {
            assert_eq!(
                tokens("echo '#not a comment'"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("#not a comment")),
                ]
            );
        }

        #[test]
        fn carriage_return() {
            // CR should be treated as whitespace
            assert_eq!(
                tokens("cmd1\rcmd2"),
                vec![Token::Word("cmd1".into()), Token::Word("cmd2".into()),]
            );
        }

        #[test]
        fn glob_patterns() {
            // Glob patterns are words
            assert_eq!(
                tokens("ls *.txt **/*.rs [a-z]?"),
                vec![
                    Token::Word("ls".into()),
                    Token::Word("*.txt".into()),
                    Token::Word("**/*.rs".into()),
                    Token::Word("[a-z]?".into()),
                ]
            );
        }

        #[test]
        fn brace_expansion_syntax() {
            // Brace patterns are words
            assert_eq!(
                tokens("echo {a,b,c}"),
                vec![Token::Word("echo".into()), Token::Word("{a,b,c}".into()),]
            );
        }

        #[test]
        fn tilde_expansion() {
            assert_eq!(
                tokens("cd ~ ~/bin ~user"),
                vec![
                    Token::Word("cd".into()),
                    Token::Word("~".into()),
                    Token::Word("~/bin".into()),
                    Token::Word("~user".into()),
                ]
            );
        }

        #[test]
        fn exclamation_point() {
            assert_eq!(
                tokens("echo !event"),
                vec![Token::Word("echo".into()), Token::Word("!event".into()),]
            );
        }

        #[test]
        fn at_sign() {
            assert_eq!(
                tokens("echo @file"),
                vec![Token::Word("echo".into()), Token::Word("@file".into()),]
            );
        }

        #[test]
        fn percent_sign() {
            assert_eq!(
                tokens("echo 50%"),
                vec![Token::Word("echo".into()), Token::Word("50%".into()),]
            );
        }

        #[test]
        fn caret() {
            assert_eq!(
                tokens("echo ^start"),
                vec![Token::Word("echo".into()), Token::Word("^start".into()),]
            );
        }

        #[test]
        fn backtick() {
            // Backticks are part of word (command substitution not handled at lexer level)
            assert_eq!(
                tokens("echo `date`"),
                vec![Token::Word("echo".into()), Token::Word("`date`".into()),]
            );
        }

        #[test]
        fn dollar_paren_substitution() {
            // $() is now recognized as command substitution
            let result = tokens("echo $(date)");
            assert_eq!(result.len(), 2);
            assert_eq!(result[0], Token::Word("echo".into()));

            // Second token should be a word with command substitution
            if let Token::Word(word) = &result[1] {
                assert_eq!(word.segments.len(), 1);
                assert_eq!(
                    word.segments[0].command_substitution,
                    Some("date".to_string())
                );
            } else {
                panic!("Expected Word token for $(date)");
            }
        }

        #[test]
        fn redirect_immediately_after_word() {
            // word> should work
            assert_eq!(
                tokens("cmd>file"),
                vec![
                    Token::Word("cmd".into()),
                    Token::RedirectOut,
                    Token::Word("file".into()),
                ]
            );
        }

        #[test]
        fn pipe_immediately_after_word() {
            assert_eq!(
                tokens("cmd|next"),
                vec![
                    Token::Word("cmd".into()),
                    Token::Pipe,
                    Token::Word("next".into()),
                ]
            );
        }

        #[test]
        fn multiple_consecutive_semicolons() {
            assert_eq!(tokens(";;;"), vec![Token::Semi, Token::Semi, Token::Semi]);
        }

        #[test]
        fn multiple_consecutive_pipes() {
            assert_eq!(
                tokens("|||"),
                vec![Token::Or, Token::Pipe] // || followed by |
            );
        }

        #[test]
        fn trailing_escape() {
            // Escape at end of input - produces empty word with no segments
            // (no character to escape, so nothing is added)
            let result = tokens("cmd \\");
            assert_eq!(
                result,
                vec![Token::Word("cmd".into()), Token::Word(Word::new())]
            );
        }

        #[test]
        fn unclosed_single_quote() {
            // Unclosed quote - lexer reads to end, still marks as single-quoted
            assert_eq!(
                tokens("echo 'hello"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("hello")),
                ]
            );
        }

        #[test]
        fn unclosed_double_quote() {
            // Unclosed quote - lexer reads to end, still marks as double-quoted
            assert_eq!(
                tokens("echo \"hello"),
                vec![
                    Token::Word("echo".into()),
                    Token::Word(Word::double_quoted("hello")),
                ]
            );
        }
    }

    // =========================================================================
    // LEXER API TESTS
    // =========================================================================

    mod lexer_api {
        use super::*;

        #[test]
        fn peek_does_not_consume() {
            let mut lexer = Lexer::new("cmd1 cmd2");

            let peeked = lexer.peek().unwrap().clone();
            assert_eq!(peeked, Token::Word("cmd1".into()));

            let next = lexer.next().unwrap();
            assert_eq!(next, Token::Word("cmd1".into()));

            let next2 = lexer.next().unwrap();
            assert_eq!(next2, Token::Word("cmd2".into()));
        }

        #[test]
        fn multiple_peeks_same_result() {
            let mut lexer = Lexer::new("cmd");

            let peek1 = lexer.peek().unwrap().clone();
            let peek2 = lexer.peek().unwrap().clone();
            let peek3 = lexer.peek().unwrap().clone();

            assert_eq!(peek1, peek2);
            assert_eq!(peek2, peek3);
        }

        #[test]
        fn at_end_detection() {
            let mut lexer = Lexer::new("cmd");

            assert!(!lexer.at_end().unwrap());
            let _ = lexer.next();
            assert!(lexer.at_end().unwrap());
        }

        #[test]
        fn at_end_empty_input() {
            let mut lexer = Lexer::new("");
            assert!(lexer.at_end().unwrap());
        }

        #[test]
        fn at_end_whitespace_only() {
            let mut lexer = Lexer::new("   ");
            assert!(lexer.at_end().unwrap());
        }

        #[test]
        fn position_tracking() {
            let mut lexer = Lexer::new("ab cd ef");

            assert_eq!(lexer.position(), 0);

            let _ = lexer.next(); // "ab"
            // Position after "ab" and skipping space
            assert!(lexer.position() > 0);
        }

        #[test]
        fn eof_returned_after_all_tokens() {
            let mut lexer = Lexer::new("cmd");

            assert_eq!(lexer.next().unwrap(), Token::Word("cmd".into()));
            assert_eq!(lexer.next().unwrap(), Token::Eof);
            assert_eq!(lexer.next().unwrap(), Token::Eof);
            assert_eq!(lexer.next().unwrap(), Token::Eof);
        }

        #[test]
        fn eof_peek_then_next() {
            let mut lexer = Lexer::new("");

            assert_eq!(lexer.peek().unwrap(), &Token::Eof);
            assert_eq!(lexer.next().unwrap(), Token::Eof);
        }
    }

    // =========================================================================
    // COMPLEX/INTEGRATION TESTS
    // =========================================================================

    mod integration {
        use super::*;

        #[test]
        fn realistic_shell_command() {
            assert_eq!(
                tokens("cat /var/log/syslog | grep -i error | head -n 20 > errors.txt 2>&1"),
                vec![
                    Token::Word("cat".into()),
                    Token::Word("/var/log/syslog".into()),
                    Token::Pipe,
                    Token::Word("grep".into()),
                    Token::Word("-i".into()),
                    Token::Word("error".into()),
                    Token::Pipe,
                    Token::Word("head".into()),
                    Token::Word("-n".into()),
                    Token::Word("20".into()),
                    Token::RedirectOut,
                    Token::Word("errors.txt".into()),
                    Token::DupFd(2, 1),
                ]
            );
        }

        #[test]
        fn git_workflow() {
            assert_eq!(
                tokens("git add . && git commit -m 'update' && git push origin main"),
                vec![
                    Token::Word("git".into()),
                    Token::Word("add".into()),
                    Token::Word(".".into()),
                    Token::And,
                    Token::Word("git".into()),
                    Token::Word("commit".into()),
                    Token::Word("-m".into()),
                    Token::Word(Word::single_quoted("update")),
                    Token::And,
                    Token::Word("git".into()),
                    Token::Word("push".into()),
                    Token::Word("origin".into()),
                    Token::Word("main".into()),
                ]
            );
        }

        #[test]
        fn docker_command() {
            assert_eq!(
                tokens(r#"docker run -d --name my-app -p 8080:80 -e "ENV=prod" nginx:latest"#),
                vec![
                    Token::Word("docker".into()),
                    Token::Word("run".into()),
                    Token::Word("-d".into()),
                    Token::Word("--name".into()),
                    Token::Word("my-app".into()),
                    Token::Word("-p".into()),
                    Token::Word("8080:80".into()),
                    Token::Word("-e".into()),
                    Token::Word(Word::double_quoted("ENV=prod")),
                    Token::Word("nginx:latest".into()),
                ]
            );
        }

        #[test]
        fn find_with_exec() {
            assert_eq!(
                tokens(r"find . -name '*.rs' -exec grep -l 'TODO' {} \;"),
                vec![
                    Token::Word("find".into()),
                    Token::Word(".".into()),
                    Token::Word("-name".into()),
                    Token::Word(Word::single_quoted("*.rs")),
                    Token::Word("-exec".into()),
                    Token::Word("grep".into()),
                    Token::Word("-l".into()),
                    Token::Word(Word::single_quoted("TODO")),
                    Token::Word("{}".into()),
                    Token::Word(";".into()), // \; becomes ;
                ]
            );
        }

        #[test]
        fn awk_script() {
            assert_eq!(
                tokens("awk '{print $1, $3}' file.txt | sort | uniq -c"),
                vec![
                    Token::Word("awk".into()),
                    Token::Word(Word::single_quoted("{print $1, $3}")),
                    Token::Word("file.txt".into()),
                    Token::Pipe,
                    Token::Word("sort".into()),
                    Token::Pipe,
                    Token::Word("uniq".into()),
                    Token::Word("-c".into()),
                ]
            );
        }

        #[test]
        fn curl_with_headers() {
            assert_eq!(
                tokens(
                    r#"curl -X POST -H "Content-Type: application/json" -d '{"key":"value"}' https://api.example.com"#
                ),
                vec![
                    Token::Word("curl".into()),
                    Token::Word("-X".into()),
                    Token::Word("POST".into()),
                    Token::Word("-H".into()),
                    Token::Word(Word::double_quoted("Content-Type: application/json")),
                    Token::Word("-d".into()),
                    Token::Word(Word::single_quoted(r#"{"key":"value"}"#)),
                    Token::Word("https://api.example.com".into()),
                ]
            );
        }

        #[test]
        fn xargs_parallel() {
            assert_eq!(
                tokens("find . -name '*.log' | xargs -P 4 -I {} gzip {}"),
                vec![
                    Token::Word("find".into()),
                    Token::Word(".".into()),
                    Token::Word("-name".into()),
                    Token::Word(Word::single_quoted("*.log")),
                    Token::Pipe,
                    Token::Word("xargs".into()),
                    Token::Word("-P".into()),
                    Token::Word("4".into()),
                    Token::Word("-I".into()),
                    Token::Word("{}".into()),
                    Token::Word("gzip".into()),
                    Token::Word("{}".into()),
                ]
            );
        }

        #[test]
        fn multi_line_command() {
            assert_eq!(
                tokens("cmd1\ncmd2\ncmd3"),
                vec![
                    Token::Word("cmd1".into()),
                    Token::Newline,
                    Token::Word("cmd2".into()),
                    Token::Newline,
                    Token::Word("cmd3".into()),
                ]
            );
        }

        #[test]
        fn script_with_comments() {
            assert_eq!(
                tokens("# Setup\ncd /tmp && ls # list files"),
                vec![
                    Token::Newline,
                    Token::Word("cd".into()),
                    Token::Word("/tmp".into()),
                    Token::And,
                    Token::Word("ls".into()),
                ]
            );
        }

        #[test]
        fn complex_subshell_redirect() {
            assert_eq!(
                tokens("(cmd1; cmd2) 2>&1 | tee log.txt"),
                vec![
                    Token::LParen,
                    Token::Word("cmd1".into()),
                    Token::Semi,
                    Token::Word("cmd2".into()),
                    Token::RParen,
                    Token::DupFd(2, 1),
                    Token::Pipe,
                    Token::Word("tee".into()),
                    Token::Word("log.txt".into()),
                ]
            );
        }

        #[test]
        fn environment_variable_assignment() {
            assert_eq!(
                tokens("VAR=value cmd --flag=$VAR"),
                vec![
                    Token::Word("VAR=value".into()),
                    Token::Word("cmd".into()),
                    Token::Word("--flag=$VAR".into()),
                ]
            );
        }

        #[test]
        fn conditional_execution() {
            assert_eq!(
                tokens("test -f file && cat file || echo 'not found'"),
                vec![
                    Token::Word("test".into()),
                    Token::Word("-f".into()),
                    Token::Word("file".into()),
                    Token::And,
                    Token::Word("cat".into()),
                    Token::Word("file".into()),
                    Token::Or,
                    Token::Word("echo".into()),
                    Token::Word(Word::single_quoted("not found")),
                ]
            );
        }

        // =====================================================================
        // Heredoc tests
        // =====================================================================

        #[test]
        fn heredoc_basic() {
            let input = "cat <<EOF\nline1\nline2\nEOF";
            let toks = tokens(input);
            assert_eq!(toks.len(), 2); // cat, HereDoc
            assert_eq!(toks[0], Token::Word("cat".into()));
            assert!(matches!(toks[1], Token::HereDoc { .. }));
            if let Token::HereDoc {
                content,
                strip_tabs,
            } = &toks[1]
            {
                assert_eq!(content, "line1\nline2");
                assert!(!strip_tabs);
            }
        }

        #[test]
        fn heredoc_strip_tabs() {
            let input = "cat <<-EOF\n\tline1\n\tline2\nEOF";
            let toks = tokens(input);
            assert_eq!(toks.len(), 2);
            if let Token::HereDoc {
                content,
                strip_tabs,
            } = &toks[1]
            {
                assert_eq!(content, "line1\nline2");
                assert!(strip_tabs);
            }
        }

        #[test]
        fn heredoc_empty() {
            let input = "cat <<EOF\nEOF";
            let toks = tokens(input);
            assert_eq!(toks.len(), 2);
            if let Token::HereDoc { content, .. } = &toks[1] {
                assert!(content.is_empty());
            }
        }

        #[test]
        fn heredoc_single_line() {
            let input = "cat <<EOF\nhello world\nEOF";
            let toks = tokens(input);
            if let Token::HereDoc { content, .. } = &toks[1] {
                assert_eq!(content, "hello world");
            }
        }

        #[test]
        fn heredoc_preserves_whitespace() {
            let input = "cat <<EOF\n  indented\n    more indented\nEOF";
            let toks = tokens(input);
            if let Token::HereDoc { content, .. } = &toks[1] {
                assert_eq!(content, "  indented\n    more indented");
            }
        }

        #[test]
        fn heredoc_quoted_delimiter() {
            let input = "cat <<'EOF'\n$VAR should not expand\nEOF";
            let toks = tokens(input);
            if let Token::HereDoc { content, .. } = &toks[1] {
                assert_eq!(content, "$VAR should not expand");
            }
        }
    }
}
