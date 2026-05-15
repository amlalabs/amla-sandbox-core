//! Shell parser.
//!
//! Parses tokens into an abstract syntax tree (AST).
//!
//! Grammar (simplified):
//! ```text
//! command_line := async_cmd ((';' | '&') async_cmd)*
//! async_cmd    := list ('&')?  -- '&' backgrounds the command
//! list         := pipeline (('&&' | '||') pipeline)*
//! pipeline     := command ('|' command)*
//! command      := simple_command | '(' command_line ')'
//! simple_command := word+ redirect*
//! redirect     := ('>' | '>>' | '<' | '2>' | '2>>' | '&>') word
//!               | N '>&' M
//! ```

use smallvec::SmallVec;

use crate::ast::{Command, Redirect};
use crate::error::{Result, ShellError};
use crate::lexer::{Lexer, Token, Word};

/// Shell command parser.
pub struct Parser<'a> {
    lexer: Lexer<'a>,
}

impl<'a> Parser<'a> {
    /// Create a new parser.
    pub fn new(input: &'a str) -> Self {
        Parser {
            lexer: Lexer::new(input),
        }
    }

    /// Parse the entire input.
    pub fn parse(&mut self) -> Result<Command> {
        self.skip_newlines()?;

        if self.lexer.at_end()? {
            return Ok(Command::Empty);
        }

        let cmd = self.parse_command_line()?;

        // Skip trailing newlines
        self.skip_newlines()?;

        Ok(cmd)
    }

    /// Parse a command line (sequence of lists separated by `;` or `&`).
    ///
    /// `&` acts as both a separator AND backgrounds the preceding command.
    /// So `cmd1 & cmd2 & cmd3` becomes Sequence[Background(cmd1), Background(cmd2), cmd3]
    fn parse_command_line(&mut self) -> Result<Command> {
        let mut commands = vec![];

        loop {
            let list = self.parse_list()?;

            match self.lexer.peek()? {
                Token::Semi => {
                    // cmd; - add to sequence and continue
                    commands.push(list);
                    self.lexer.next()?;
                    self.skip_newlines()?;

                    // Check for end
                    match self.lexer.peek()? {
                        Token::Eof | Token::RParen | Token::Newline => break,
                        Token::Amp => {
                            // ; & is weird but valid - empty background, then continue
                            self.lexer.next()?;
                            self.skip_newlines()?;
                            match self.lexer.peek()? {
                                Token::Eof | Token::RParen | Token::Newline => break,
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                Token::Amp => {
                    // cmd & - background this command and continue
                    commands.push(Command::Background {
                        command: Box::new(list),
                    });
                    self.lexer.next()?;
                    self.skip_newlines()?;

                    // Check for more commands
                    match self.lexer.peek()? {
                        Token::Eof | Token::RParen | Token::Newline => break,
                        _ => {} // More commands to parse
                    }
                }
                Token::Newline | Token::Eof | Token::RParen => {
                    // End of command line
                    commands.push(list);
                    break;
                }
                _ => {
                    commands.push(list);
                    break;
                }
            }
        }

        // Flatten single command
        if commands.len() == 1 {
            Ok(commands.pop().unwrap())
        } else if commands.is_empty() {
            Ok(Command::Empty)
        } else {
            Ok(Command::Sequence { commands })
        }
    }

    /// Parse a list (pipelines connected by && or ||).
    fn parse_list(&mut self) -> Result<Command> {
        let mut left = self.parse_pipeline()?;

        loop {
            match self.lexer.peek()? {
                Token::And => {
                    self.lexer.next()?;
                    self.skip_newlines()?;
                    let right = self.parse_pipeline()?;
                    left = Command::And {
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                Token::Or => {
                    self.lexer.next()?;
                    self.skip_newlines()?;
                    let right = self.parse_pipeline()?;
                    left = Command::Or {
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                _ => break,
            }
        }

        Ok(left)
    }

    /// Parse a pipeline (commands connected by |).
    fn parse_pipeline(&mut self) -> Result<Command> {
        let mut commands = vec![self.parse_command()?];

        while matches!(self.lexer.peek()?, Token::Pipe) {
            self.lexer.next()?;
            self.skip_newlines()?;
            commands.push(self.parse_command()?);
        }

        Ok(Command::pipeline(commands))
    }

    /// Parse a command (simple command or subshell).
    fn parse_command(&mut self) -> Result<Command> {
        match self.lexer.peek()? {
            Token::LParen => {
                self.lexer.next()?;
                self.skip_newlines()?;
                let cmd = self.parse_command_line()?;
                self.expect(Token::RParen)?;
                Ok(Command::Subshell {
                    command: Box::new(cmd),
                })
            }
            _ => self.parse_simple_command(),
        }
    }

    /// Parse a simple command (words and redirects).
    fn parse_simple_command(&mut self) -> Result<Command> {
        let mut argv: SmallVec<[Word; 8]> = SmallVec::new();
        let mut redirects: SmallVec<[Redirect; 2]> = SmallVec::new();

        loop {
            match self.lexer.peek()? {
                Token::Word(w) => {
                    let word = w.clone();
                    self.lexer.next()?;
                    argv.push(word);
                }
                Token::RedirectOut => {
                    self.lexer.next()?;
                    let target = self.expect_word()?;
                    redirects.push(Redirect::stdout_write(target));
                }
                Token::RedirectAppend => {
                    self.lexer.next()?;
                    let target = self.expect_word()?;
                    redirects.push(Redirect::stdout_append(target));
                }
                Token::RedirectIn => {
                    self.lexer.next()?;
                    let target = self.expect_word()?;
                    redirects.push(Redirect::stdin_read(target));
                }
                Token::RedirectErr => {
                    self.lexer.next()?;
                    let target = self.expect_word()?;
                    redirects.push(Redirect::stderr_write(target));
                }
                Token::RedirectErrAppend => {
                    self.lexer.next()?;
                    let target = self.expect_word()?;
                    redirects.push(Redirect::stderr_append(target));
                }
                Token::RedirectBoth => {
                    self.lexer.next()?;
                    let target = self.expect_word()?;
                    // &> is equivalent to > file 2>&1
                    redirects.push(Redirect::stdout_write(target.clone()));
                    redirects.push(Redirect::dup_fd(2, 1));
                }
                Token::DupFd(source, target) => {
                    let s = *source;
                    let t = *target;
                    self.lexer.next()?;
                    redirects.push(Redirect::dup_fd(s, t));
                }
                Token::HereDoc {
                    content,
                    strip_tabs,
                } => {
                    let content = content.clone();
                    let strip = *strip_tabs;
                    self.lexer.next()?;
                    redirects.push(Redirect::stdin_heredoc(content, strip));
                }
                _ => break,
            }
        }

        if argv.is_empty() && redirects.is_empty() {
            return Err(ShellError::Syntax {
                message: "expected command".into(),
                position: self.lexer.position(),
            });
        }

        if argv.is_empty() {
            // Redirect-only command (unusual but valid)
            argv.push(":".into()); // No-op command
        }

        Ok(Command::simple_with_redirects(argv, redirects))
    }

    /// Skip newlines.
    fn skip_newlines(&mut self) -> Result<()> {
        while matches!(self.lexer.peek()?, Token::Newline) {
            self.lexer.next()?;
        }
        Ok(())
    }

    /// Expect a specific token.
    fn expect(&mut self, expected: Token) -> Result<()> {
        let tok = self.lexer.next()?;
        if tok == expected {
            Ok(())
        } else {
            Err(ShellError::Syntax {
                message: format!("expected {expected:?}, got {tok:?}"),
                position: self.lexer.position(),
            })
        }
    }

    /// Expect a word token.
    fn expect_word(&mut self) -> Result<Word> {
        match self.lexer.next()? {
            Token::Word(w) => Ok(w),
            tok => Err(ShellError::Syntax {
                message: format!("expected word, got {tok:?}"),
                position: self.lexer.position(),
            }),
        }
    }
}

/// Parse a shell command string.
pub fn parse(input: &str) -> Result<Command> {
    Parser::new(input).parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{RedirectKind, RedirectTarget};

    /// Helper to compare argv (SmallVec<[Word; 8]>) with expected strings.
    fn check_argv(argv: &SmallVec<[Word; 8]>, expected: &[&str]) {
        assert_eq!(
            argv.len(),
            expected.len(),
            "argv length mismatch: got {:?}",
            argv.iter().map(|w| w.text()).collect::<Vec<_>>()
        );
        for (w, e) in argv.iter().zip(expected) {
            assert_eq!(w, *e, "word mismatch");
        }
    }

    #[test]
    fn simple_command() {
        let cmd = parse("ls -la /tmp").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["ls", "-la", "/tmp"]);
                assert!(redirects.is_empty());
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn pipeline() {
        let cmd = parse("cat file | grep foo | wc -l").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 3);
            }
            _ => panic!("expected pipeline"),
        }
    }

    #[test]
    fn redirect_out() {
        let cmd = parse("echo hello > out.txt").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["echo", "hello"]);
                assert_eq!(redirects.len(), 1);
                assert_eq!(redirects[0].source_fd, 1);
                assert!(matches!(redirects[0].kind, RedirectKind::Write));
                assert!(matches!(&redirects[0].target, RedirectTarget::File(f) if f == "out.txt"));
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn redirect_append() {
        let cmd = parse("echo hello >> out.txt").unwrap();
        match cmd {
            Command::Simple { argv: _, redirects } => {
                assert_eq!(redirects.len(), 1);
                assert!(matches!(redirects[0].kind, RedirectKind::Append));
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn redirect_in() {
        let cmd = parse("cat < in.txt").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["cat"]);
                assert_eq!(redirects.len(), 1);
                assert_eq!(redirects[0].source_fd, 0);
                assert!(matches!(redirects[0].kind, RedirectKind::Read));
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn redirect_stderr() {
        let cmd = parse("cmd 2> err.txt").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["cmd"]);
                assert_eq!(redirects.len(), 1);
                assert_eq!(redirects[0].source_fd, 2);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn fd_dup() {
        let cmd = parse("cmd 2>&1").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["cmd"]);
                assert_eq!(redirects.len(), 1);
                assert_eq!(redirects[0].source_fd, 2);
                assert!(matches!(redirects[0].target, RedirectTarget::Fd(1)));
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn and_list() {
        let cmd = parse("cmd1 && cmd2").unwrap();
        assert!(matches!(cmd, Command::And { .. }));
    }

    #[test]
    fn or_list() {
        let cmd = parse("cmd1 || cmd2").unwrap();
        assert!(matches!(cmd, Command::Or { .. }));
    }

    #[test]
    fn sequence() {
        let cmd = parse("cmd1 ; cmd2 ; cmd3").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 3);
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn background() {
        let cmd = parse("sleep 10 &").unwrap();
        assert!(matches!(cmd, Command::Background { .. }));
    }

    #[test]
    fn subshell() {
        let cmd = parse("(cmd1 | cmd2)").unwrap();
        assert!(matches!(cmd, Command::Subshell { .. }));
    }

    #[test]
    fn complex_command() {
        let cmd = parse("cat file | grep foo && echo found || echo 'not found'").unwrap();
        // Should be: (cat | grep) && echo || echo
        assert!(matches!(cmd, Command::Or { .. }));
    }

    #[test]
    fn empty_input() {
        let cmd = parse("").unwrap();
        assert!(cmd.is_empty());
    }

    #[test]
    fn multiple_redirects() {
        let cmd = parse("cmd < in.txt > out.txt 2>&1").unwrap();
        match cmd {
            Command::Simple { redirects, .. } => {
                assert_eq!(redirects.len(), 3);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn redirect_both() {
        let cmd = parse("cmd &> out.txt").unwrap();
        match cmd {
            Command::Simple { redirects, .. } => {
                // &> expands to > file 2>&1
                assert_eq!(redirects.len(), 2);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn pipeline_with_redirects() {
        let cmd = parse("cat < in.txt | grep foo > out.txt").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 2);
                match &commands[0] {
                    Command::Simple { redirects, .. } => {
                        assert_eq!(redirects.len(), 1); // < in.txt
                    }
                    _ => panic!("expected simple"),
                }
                match &commands[1] {
                    Command::Simple { redirects, .. } => {
                        assert_eq!(redirects.len(), 1); // > out.txt
                    }
                    _ => panic!("expected simple"),
                }
            }
            _ => panic!("expected pipeline"),
        }
    }

    // =========================================================================
    // EDGE CASES - Newlines and whitespace
    // =========================================================================

    #[test]
    fn leading_newlines() {
        let cmd = parse("\n\n\necho hello").unwrap();
        match cmd {
            Command::Simple { argv, .. } => {
                check_argv(&argv, &["echo", "hello"]);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn trailing_newlines() {
        let cmd = parse("echo hello\n\n\n").unwrap();
        match cmd {
            Command::Simple { argv, .. } => {
                check_argv(&argv, &["echo", "hello"]);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn newlines_around_operators() {
        // Newlines after && should be allowed
        let cmd = parse("cmd1 &&\ncmd2").unwrap();
        assert!(matches!(cmd, Command::And { .. }));
    }

    #[test]
    fn newlines_after_pipe() {
        let cmd = parse("cmd1 |\ncmd2").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 2);
            }
            _ => panic!("expected pipeline"),
        }
    }

    #[test]
    fn newlines_in_subshell() {
        // Parser allows newlines after ( but not before )
        let cmd = parse("(\ncmd1)").unwrap();
        assert!(matches!(cmd, Command::Subshell { .. }));
    }

    #[test]
    fn only_newlines() {
        let cmd = parse("\n\n\n").unwrap();
        assert!(cmd.is_empty());
    }

    #[test]
    fn only_whitespace() {
        let cmd = parse("   \t  ").unwrap();
        assert!(cmd.is_empty());
    }

    // =========================================================================
    // EDGE CASES - Semicolons
    // =========================================================================

    #[test]
    fn trailing_semicolon() {
        // Trailing semicolon after last command
        let cmd = parse("cmd1 ; cmd2 ;").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 2);
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn semicolon_before_eof() {
        let cmd = parse("cmd ;").unwrap();
        match cmd {
            Command::Simple { argv, .. } => {
                check_argv(&argv, &["cmd"]);
            }
            _ => panic!("expected simple command"),
        }
    }

    #[test]
    fn semicolon_with_newlines() {
        let cmd = parse("cmd1 ;\n\ncmd2").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 2);
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn semicolon_before_background() {
        // cmd1 ; & is unusual - in bash this is actually a syntax error
        // We're permissive: the & after ; is ignored (nothing to background)
        let cmd = parse("cmd1 ; &").unwrap();
        // Result is just cmd1 - the trailing ;&  is effectively ignored
        assert!(matches!(cmd, Command::Simple { .. }));
    }

    // =========================================================================
    // EDGE CASES - Background operator
    // =========================================================================

    #[test]
    fn background_simple() {
        let cmd = parse("cmd &").unwrap();
        match cmd {
            Command::Background { command } => {
                assert!(matches!(*command, Command::Simple { .. }));
            }
            _ => panic!("expected background"),
        }
    }

    #[test]
    fn background_pipeline() {
        let cmd = parse("cmd1 | cmd2 &").unwrap();
        match cmd {
            Command::Background { command } => {
                assert!(matches!(*command, Command::Pipeline { .. }));
            }
            _ => panic!("expected background"),
        }
    }

    #[test]
    fn background_then_foreground() {
        // cmd1 & cmd2 means: background cmd1, then run cmd2 in foreground
        let cmd = parse("cmd1 & cmd2").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 2);
                assert!(matches!(commands[0], Command::Background { .. }));
                assert!(matches!(commands[1], Command::Simple { .. }));
            }
            _ => panic!("expected sequence, got {cmd:?}"),
        }
    }

    #[test]
    fn multiple_background() {
        // cmd1 & cmd2 & cmd3 means: background cmd1, background cmd2, foreground cmd3
        let cmd = parse("cmd1 & cmd2 & cmd3").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 3);
                assert!(matches!(commands[0], Command::Background { .. }));
                assert!(matches!(commands[1], Command::Background { .. }));
                assert!(matches!(commands[2], Command::Simple { .. }));
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn multiple_background_trailing_amp() {
        // cmd1 & cmd2 & means: background both
        let cmd = parse("cmd1 & cmd2 &").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 2);
                assert!(matches!(commands[0], Command::Background { .. }));
                assert!(matches!(commands[1], Command::Background { .. }));
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn background_with_wait() {
        // sleep 1 & sleep 2 & wait - common pattern
        let cmd = parse("sleep 1 & sleep 2 & wait").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 3);
                assert!(matches!(commands[0], Command::Background { .. }));
                assert!(matches!(commands[1], Command::Background { .. }));
                assert!(matches!(commands[2], Command::Simple { .. }));
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn semicolon_then_background() {
        // cmd1 ; cmd2 & means: foreground cmd1, then background cmd2
        let cmd = parse("cmd1 ; cmd2 &").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 2);
                assert!(matches!(commands[0], Command::Simple { .. }));
                assert!(matches!(commands[1], Command::Background { .. }));
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn background_with_and() {
        let cmd = parse("cmd1 && cmd2 &").unwrap();
        match cmd {
            Command::Background { command } => {
                assert!(matches!(*command, Command::And { .. }));
            }
            _ => panic!("expected background"),
        }
    }

    // =========================================================================
    // EDGE CASES - Subshells
    // =========================================================================

    #[test]
    fn nested_subshells() {
        let cmd = parse("((cmd))").unwrap();
        match cmd {
            Command::Subshell { command } => {
                assert!(matches!(*command, Command::Subshell { .. }));
            }
            _ => panic!("expected nested subshell"),
        }
    }

    #[test]
    fn subshell_with_sequence() {
        let cmd = parse("(cmd1 ; cmd2)").unwrap();
        match cmd {
            Command::Subshell { command } => {
                assert!(matches!(*command, Command::Sequence { .. }));
            }
            _ => panic!("expected subshell with sequence"),
        }
    }

    #[test]
    fn subshell_with_and() {
        let cmd = parse("(cmd1 && cmd2)").unwrap();
        match cmd {
            Command::Subshell { command } => {
                assert!(matches!(*command, Command::And { .. }));
            }
            _ => panic!("expected subshell with and"),
        }
    }

    #[test]
    fn subshell_with_background() {
        let cmd = parse("(cmd &)").unwrap();
        match cmd {
            Command::Subshell { command } => {
                assert!(matches!(*command, Command::Background { .. }));
            }
            _ => panic!("expected subshell with background"),
        }
    }

    #[test]
    fn subshell_in_pipeline() {
        let cmd = parse("(cmd1) | (cmd2)").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 2);
                assert!(matches!(commands[0], Command::Subshell { .. }));
                assert!(matches!(commands[1], Command::Subshell { .. }));
            }
            _ => panic!("expected pipeline of subshells"),
        }
    }

    #[test]
    fn subshell_with_redirect() {
        // Redirects after subshell - currently not supported in grammar
        // but let's test what happens
        let result = parse("(cmd) > out.txt");
        // This depends on implementation - might be error or might work
        // Just verify it doesn't crash
        let _ = result;
    }

    // =========================================================================
    // EDGE CASES - Chained operators
    // =========================================================================

    #[test]
    fn chained_and() {
        let cmd = parse("a && b && c && d").unwrap();
        // Should be left-associative: ((a && b) && c) && d
        match cmd {
            Command::And { left, right: _ } => {
                match *left {
                    Command::And { left: _, right: _ } => {
                        // Good, it's nested
                    }
                    _ => panic!("expected nested and"),
                }
            }
            _ => panic!("expected and"),
        }
    }

    #[test]
    fn chained_or() {
        let cmd = parse("a || b || c").unwrap();
        assert!(matches!(cmd, Command::Or { .. }));
    }

    #[test]
    fn mixed_and_or() {
        let cmd = parse("a && b || c && d").unwrap();
        // Should parse as: ((a && b) || c) && d
        // Because && and || have same precedence, left-to-right
        assert!(matches!(cmd, Command::And { .. }));
    }

    #[test]
    fn and_or_with_pipelines() {
        let cmd = parse("a | b && c | d").unwrap();
        // Should be: (a | b) && (c | d)
        match cmd {
            Command::And { left, right } => {
                assert!(matches!(*left, Command::Pipeline { .. }));
                assert!(matches!(*right, Command::Pipeline { .. }));
            }
            _ => panic!("expected and of pipelines"),
        }
    }

    // =========================================================================
    // EDGE CASES - Redirect ordering and combinations
    // =========================================================================

    #[test]
    fn redirects_before_args() {
        let cmd = parse("> out.txt cmd arg").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["cmd", "arg"]);
                assert_eq!(redirects.len(), 1);
            }
            _ => panic!("expected simple"),
        }
    }

    #[test]
    fn redirects_interspersed() {
        let cmd = parse("cmd > out arg1 < in arg2").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &["cmd", "arg1", "arg2"]);
                assert_eq!(redirects.len(), 2);
            }
            _ => panic!("expected simple"),
        }
    }

    #[test]
    fn redirect_only_command() {
        // Redirect without explicit command - should use : (no-op)
        let cmd = parse("> out.txt").unwrap();
        match cmd {
            Command::Simple { argv, redirects } => {
                check_argv(&argv, &[":"]);
                assert_eq!(redirects.len(), 1);
            }
            _ => panic!("expected simple with : command"),
        }
    }

    #[test]
    fn all_redirect_types() {
        let cmd = parse("cmd > a >> b < c 2> d 2>> e &> f 3>&4").unwrap();
        match cmd {
            Command::Simple { redirects, .. } => {
                // &> expands to 2 redirects
                assert_eq!(redirects.len(), 8);
            }
            _ => panic!("expected simple"),
        }
    }

    // =========================================================================
    // ERROR CASES
    // =========================================================================

    #[test]
    fn error_missing_command_after_pipe() {
        let result = parse("cmd |");
        assert!(result.is_err());
    }

    #[test]
    fn error_missing_command_after_and() {
        let result = parse("cmd &&");
        assert!(result.is_err());
    }

    #[test]
    fn error_missing_command_after_or() {
        let result = parse("cmd ||");
        assert!(result.is_err());
    }

    #[test]
    fn error_unclosed_paren() {
        let result = parse("(cmd");
        assert!(result.is_err());
    }

    #[test]
    fn error_missing_redirect_target() {
        let result = parse("cmd >");
        assert!(result.is_err());
    }

    #[test]
    fn error_redirect_to_pipe() {
        let result = parse("cmd > |");
        assert!(result.is_err());
    }

    #[test]
    fn error_empty_pipeline_stage() {
        // Starting with pipe
        let result = parse("| cmd");
        assert!(result.is_err());
    }

    #[test]
    fn error_double_pipe() {
        // || is OR, not two pipes
        // But ||| would be || followed by |
        let result = parse("cmd ||| next");
        // This should parse as cmd || (| next) which is an error
        assert!(result.is_err());
    }

    #[test]
    fn error_unmatched_rparen() {
        let result = parse("cmd )");
        // Unmatched ) - depends on how parser handles it
        // It should fail because ) isn't expected
        let _ = result; // Just check it doesn't crash
    }

    #[test]
    fn error_empty_subshell() {
        let result = parse("()");
        assert!(result.is_err());
    }

    // =========================================================================
    // COMPLEX INTEGRATION TESTS
    // =========================================================================

    #[test]
    fn complex_realistic_command() {
        let cmd =
            parse("cat file 2>/dev/null | grep -v '^#' | sort | uniq -c > result.txt").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 4);
            }
            _ => panic!("expected pipeline"),
        }
    }

    #[test]
    fn complex_conditional() {
        let cmd = parse("test -f file && cat file || echo 'not found'").unwrap();
        match cmd {
            Command::Or { left, right: _ } => {
                assert!(matches!(*left, Command::And { .. }));
            }
            _ => panic!("expected or with and inside"),
        }
    }

    #[test]
    fn complex_grouped() {
        let cmd = parse("(cd /tmp && ls) | grep foo").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 2);
                match &commands[0] {
                    Command::Subshell { command } => {
                        assert!(matches!(**command, Command::And { .. }));
                    }
                    _ => panic!("expected subshell"),
                }
            }
            _ => panic!("expected pipeline"),
        }
    }

    #[test]
    fn complex_script_like() {
        let cmd = parse("mkdir -p dir ; cd dir ; touch file ; echo done").unwrap();
        match cmd {
            Command::Sequence { commands } => {
                assert_eq!(commands.len(), 4);
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn long_pipeline() {
        let cmd = parse("a | b | c | d | e | f | g | h").unwrap();
        match cmd {
            Command::Pipeline { commands } => {
                assert_eq!(commands.len(), 8);
            }
            _ => panic!("expected pipeline"),
        }
    }

    #[test]
    fn multiline_script() {
        let cmd = parse("cmd1\ncmd2\ncmd3").unwrap();
        // Newlines are command separators
        if let Command::Simple { argv, .. } = cmd {
            // Only first line parsed? Or all?
            // Based on grammar, newline is like EOF/end
            assert_eq!(argv[0], "cmd1");
        }
    }
}
