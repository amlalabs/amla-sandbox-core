//! Glob pattern matching and expansion.
//!
//! Supports:
//! - `*` - matches any sequence of characters (including empty)
//! - `?` - matches exactly one character
//! - `[abc]` - matches any character in the set
//! - `[a-z]` - matches any character in the range
//! - `[!abc]` or `[^abc]` - matches any character NOT in the set

use std::cell::RefCell;
use std::rc::Rc;

use amla_vfs::Vfs;

/// Check if a string contains glob metacharacters.
pub fn is_glob_pattern(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Skip escaped character
                chars.next();
            }
            '*' | '?' => return true,
            // Check for valid bracket expression
            '[' if chars.peek().is_some() => return true,
            _ => {}
        }
    }
    false
}

/// Match a pattern against a string.
///
/// Returns true if the pattern matches the entire string.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_impl(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_impl(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0; // pattern index
    let mut ti = 0; // text index
    let mut star_pi = None; // position in pattern after last *
    let mut star_ti = None; // position in text when we matched *

    while ti < text.len() {
        if pi < pattern.len() {
            match pattern[pi] {
                // Escaped character - match literally
                b'\\' if pi + 1 < pattern.len() && text[ti] == pattern[pi + 1] => {
                    pi += 2;
                    ti += 1;
                    continue;
                }
                b'*' => {
                    // Star: save position and try matching zero characters
                    star_pi = Some(pi + 1);
                    star_ti = Some(ti);
                    pi += 1;
                    continue;
                }
                b'?' => {
                    // Question mark: match any single character
                    pi += 1;
                    ti += 1;
                    continue;
                }
                b'[' => {
                    // Bracket expression
                    if let Some((matched, end_pi)) = match_bracket(&pattern[pi..], text[ti])
                        && matched
                    {
                        pi += end_pi;
                        ti += 1;
                        continue;
                    }
                }
                c if c == text[ti] => {
                    // Literal match
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }

        // No match at current position - backtrack to last star if possible
        if let (Some(spi), Some(sti)) = (star_pi, star_ti) {
            pi = spi;
            star_ti = Some(sti + 1);
            ti = sti + 1;
        } else {
            return false;
        }
    }

    // Check remaining pattern is all stars
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

/// Match a bracket expression against a character.
/// Returns (matched, `end_position`) where `end_position` is offset from start of pattern.
fn match_bracket(pattern: &[u8], ch: u8) -> Option<(bool, usize)> {
    if pattern.is_empty() || pattern[0] != b'[' {
        return None;
    }

    let mut i = 1;
    let negate = if i < pattern.len() && (pattern[i] == b'!' || pattern[i] == b'^') {
        i += 1;
        true
    } else {
        false
    };

    let mut matched = false;

    // First character after [ (or [! / [^) can be ]
    if i < pattern.len() && pattern[i] == b']' {
        if ch == b']' {
            matched = true;
        }
        i += 1;
    }

    while i < pattern.len() && pattern[i] != b']' {
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            // Range: a-z
            let start = pattern[i];
            let end = pattern[i + 2];
            if ch >= start && ch <= end {
                matched = true;
            }
            i += 3;
        } else {
            // Single character
            if pattern[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }

    if i < pattern.len() && pattern[i] == b']' {
        Some((if negate { !matched } else { matched }, i + 1))
    } else {
        None // Unclosed bracket
    }
}

/// Expand a glob pattern into matching paths.
///
/// If the pattern doesn't match anything, returns the original pattern.
/// This is POSIX behavior (nullglob is off by default).
///
/// Supports globs in directory components (e.g., `/foo/*/bar`).
pub fn expand_glob(vfs: &Rc<RefCell<Vfs>>, cwd: &str, pattern: &str) -> Vec<String> {
    // If not a glob pattern, return as-is
    if !is_glob_pattern(pattern) {
        return vec![pattern.to_string()];
    }

    // Determine if pattern is absolute
    let is_absolute = pattern.starts_with('/');
    let base = if is_absolute { "/" } else { cwd };

    // Split pattern into path segments
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();

    if segments.is_empty() {
        return vec![pattern.to_string()];
    }

    // Recursively expand - results will be absolute paths
    let mut matches = expand_glob_recursive(vfs, base, &segments);

    // If pattern was relative, convert results to relative paths
    if !is_absolute {
        let cwd_prefix = if cwd.ends_with('/') {
            cwd
        } else {
            &format!("{cwd}/")
        };
        matches = matches
            .into_iter()
            .map(|path| {
                if let Some(rel) = path.strip_prefix(cwd_prefix) {
                    rel.to_string()
                } else if path == cwd {
                    ".".to_string()
                } else {
                    path
                }
            })
            .collect();
    }

    // Sort matches for consistent ordering
    matches.sort();

    // If no matches, return original pattern (POSIX behavior)
    if matches.is_empty() {
        vec![pattern.to_string()]
    } else {
        matches
    }
}

/// Recursively expand glob segments.
///
/// `base` is the current directory being searched (absolute path).
/// `segments` are the remaining path components to match.
/// Returns absolute paths.
fn expand_glob_recursive(vfs: &Rc<RefCell<Vfs>>, base: &str, segments: &[&str]) -> Vec<String> {
    if segments.is_empty() {
        // Base case: we've matched all segments
        return vec![base.to_string()];
    }

    let (current_segment, remaining) = (segments[0], &segments[1..]);

    if !is_glob_pattern(current_segment) {
        // Literal segment - just append and recurse
        let next_path = if base == "/" {
            format!("/{current_segment}")
        } else {
            format!("{base}/{current_segment}")
        };

        // Check if path exists before recursing
        let vfs_ref = vfs.borrow();
        if vfs_ref.exists(&next_path) {
            drop(vfs_ref);
            return expand_glob_recursive(vfs, &next_path, remaining);
        }
        return vec![];
    }

    // Glob segment - list directory and match
    let vfs_ref = vfs.borrow();
    let entries = match vfs_ref.list_dir(base) {
        Ok(entries) => entries,
        Err(_) => return vec![],
    };
    drop(vfs_ref);

    let mut results = Vec::new();

    for entry in entries {
        if glob_match(current_segment, &entry.name) {
            let matched_path = if base == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", base, entry.name)
            };

            if remaining.is_empty() {
                // This is the last segment - add match
                results.push(matched_path);
            } else {
                // More segments to match - recurse only if this is a directory
                let vfs_ref = vfs.borrow();
                if vfs_ref.is_dir(&matched_path) {
                    drop(vfs_ref);
                    results.extend(expand_glob_recursive(vfs, &matched_path, remaining));
                }
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // is_glob_pattern tests
    // =========================================================================

    #[test]
    fn is_glob_star() {
        assert!(is_glob_pattern("*.txt"));
        assert!(is_glob_pattern("file*"));
        assert!(is_glob_pattern("*"));
    }

    #[test]
    fn is_glob_question() {
        assert!(is_glob_pattern("file?.txt"));
        assert!(is_glob_pattern("?"));
    }

    #[test]
    fn is_glob_bracket() {
        assert!(is_glob_pattern("[abc]"));
        assert!(is_glob_pattern("file[0-9].txt"));
    }

    #[test]
    fn is_glob_escaped() {
        // Escaped metacharacters should not trigger glob
        assert!(!is_glob_pattern("file\\*.txt"));
        assert!(!is_glob_pattern("file\\?.txt"));
    }

    #[test]
    fn is_glob_plain() {
        assert!(!is_glob_pattern("file.txt"));
        assert!(!is_glob_pattern("path/to/file"));
        assert!(!is_glob_pattern(""));
    }

    // =========================================================================
    // glob_match tests - star
    // =========================================================================

    #[test]
    fn match_star_prefix() {
        assert!(glob_match("*.txt", "file.txt"));
        assert!(glob_match("*.txt", ".txt"));
        assert!(!glob_match("*.txt", "file.log"));
    }

    #[test]
    fn match_star_suffix() {
        assert!(glob_match("file*", "file.txt"));
        assert!(glob_match("file*", "file"));
        assert!(!glob_match("file*", "other.txt"));
    }

    #[test]
    fn match_star_middle() {
        assert!(glob_match("f*e", "file"));
        assert!(glob_match("f*e", "fe"));
        assert!(glob_match("f*e", "foobare"));
        assert!(!glob_match("f*e", "foo"));
    }

    #[test]
    fn match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn match_multiple_stars() {
        assert!(glob_match("*.*", "file.txt"));
        assert!(glob_match("*.*", ".txt"));
        assert!(glob_match("*.*", "a.b"));
        assert!(!glob_match("*.*", "noext"));
    }

    // =========================================================================
    // glob_match tests - question mark
    // =========================================================================

    #[test]
    fn match_question() {
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(glob_match("file?.txt", "fileA.txt"));
        assert!(!glob_match("file?.txt", "file.txt"));
        assert!(!glob_match("file?.txt", "file12.txt"));
    }

    #[test]
    fn match_multiple_questions() {
        assert!(glob_match("???", "abc"));
        assert!(!glob_match("???", "ab"));
        assert!(!glob_match("???", "abcd"));
    }

    // =========================================================================
    // glob_match tests - brackets
    // =========================================================================

    #[test]
    fn match_bracket_set() {
        assert!(glob_match("[abc]", "a"));
        assert!(glob_match("[abc]", "b"));
        assert!(glob_match("[abc]", "c"));
        assert!(!glob_match("[abc]", "d"));
    }

    #[test]
    fn match_bracket_range() {
        assert!(glob_match("[a-z]", "m"));
        assert!(glob_match("[0-9]", "5"));
        assert!(!glob_match("[a-z]", "M"));
        assert!(!glob_match("[0-9]", "a"));
    }

    #[test]
    fn match_bracket_negate() {
        assert!(glob_match("[!abc]", "d"));
        assert!(!glob_match("[!abc]", "a"));
        assert!(glob_match("[^0-9]", "x"));
        assert!(!glob_match("[^0-9]", "5"));
    }

    #[test]
    fn match_bracket_in_pattern() {
        assert!(glob_match("file[0-9].txt", "file5.txt"));
        assert!(!glob_match("file[0-9].txt", "fileA.txt"));
    }

    // =========================================================================
    // glob_match tests - escape
    // =========================================================================

    #[test]
    fn match_escaped_star() {
        assert!(glob_match("file\\*.txt", "file*.txt"));
        assert!(!glob_match("file\\*.txt", "fileX.txt"));
    }

    #[test]
    fn match_escaped_question() {
        assert!(glob_match("file\\?.txt", "file?.txt"));
        assert!(!glob_match("file\\?.txt", "fileX.txt"));
    }

    // =========================================================================
    // glob_match tests - edge cases
    // =========================================================================

    #[test]
    fn match_empty_pattern() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn match_literal() {
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "other"));
    }

    #[test]
    fn match_complex() {
        assert!(glob_match("*.tar.gz", "archive.tar.gz"));
        assert!(glob_match("test_*.rs", "test_foo.rs"));
        assert!(glob_match("[A-Z]*[0-9]", "File1"));
        assert!(!glob_match("[A-Z]*[0-9]", "file1"));
    }

    // =========================================================================
    // expand_glob tests (require VFS)
    // =========================================================================

    #[test]
    fn expand_no_glob() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let result = expand_glob(&vfs, "/", "plain.txt");
        assert_eq!(result, vec!["plain.txt"]);
    }

    #[test]
    fn expand_no_matches() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        // No files in VFS, pattern returns as-is
        let result = expand_glob(&vfs, "/workspace", "*.nonexistent");
        assert_eq!(result, vec!["*.nonexistent"]);
    }

    #[test]
    fn expand_with_matches() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.write_file("/workspace/file1.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/file2.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/other.log", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "*.txt");
        assert_eq!(result, vec!["file1.txt", "file2.txt"]);
    }

    #[test]
    fn expand_with_path() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.create_dir("/workspace/subdir", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file(
                "/workspace/subdir/a.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/subdir/b.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "subdir/*.txt");
        assert_eq!(result, vec!["subdir/a.txt", "subdir/b.txt"]);
    }

    #[test]
    fn expand_question_mark() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.write_file("/workspace/f1.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/f2.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/f10.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "f?.txt");
        assert_eq!(result, vec!["f1.txt", "f2.txt"]);
    }

    #[test]
    fn expand_bracket() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.write_file("/workspace/a.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/b.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/c.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file("/workspace/d.txt", b"", amla_vfs::Permission::ReadWrite)
                .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "[ab].txt");
        assert_eq!(result, vec!["a.txt", "b.txt"]);
    }

    // =========================================================================
    // Directory glob expansion tests
    // =========================================================================

    #[test]
    fn expand_dir_glob_star() {
        // Test /workspace/*/file.txt pattern
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            // Create directories
            v.create_dir_all("/workspace/dir1", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/dir2", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/other", amla_vfs::Permission::ReadWrite)
                .unwrap();
            // Create files in directories
            v.write_file(
                "/workspace/dir1/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/dir2/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/other/other.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "/workspace/*/file.txt");
        assert_eq!(
            result,
            vec!["/workspace/dir1/file.txt", "/workspace/dir2/file.txt"]
        );
    }

    #[test]
    fn expand_dir_glob_pattern() {
        // Test /workspace/dir*/file.txt pattern
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.create_dir_all("/workspace/dir1", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/dir2", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/other", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file(
                "/workspace/dir1/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/dir2/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/other/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "/workspace/dir*/file.txt");
        assert_eq!(
            result,
            vec!["/workspace/dir1/file.txt", "/workspace/dir2/file.txt"]
        );
    }

    #[test]
    fn expand_dir_glob_relative() {
        // Test relative dir/*/file.txt pattern
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.create_dir_all("/workspace/src/a", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/src/b", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file(
                "/workspace/src/a/lib.rs",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/src/b/lib.rs",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "src/*/lib.rs");
        assert_eq!(result, vec!["src/a/lib.rs", "src/b/lib.rs"]);
    }

    #[test]
    fn expand_dir_glob_multiple_levels() {
        // Test /foo/*/bar/*.txt (multiple glob segments)
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.create_dir_all("/workspace/a/sub1", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/a/sub2", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.create_dir_all("/workspace/b/sub1", amla_vfs::Permission::ReadWrite)
                .unwrap();
            v.write_file(
                "/workspace/a/sub1/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/a/sub2/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/b/sub1/file.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "/workspace/*/sub*/*.txt");
        assert_eq!(
            result,
            vec![
                "/workspace/a/sub1/file.txt",
                "/workspace/a/sub2/file.txt",
                "/workspace/b/sub1/file.txt"
            ]
        );
    }

    #[test]
    fn expand_dir_glob_no_matches() {
        // No matches should return original pattern
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.create_dir_all("/workspace/other", amla_vfs::Permission::ReadWrite)
                .unwrap();
        }

        let result = expand_glob(&vfs, "/workspace", "/workspace/*/missing.txt");
        assert_eq!(result, vec!["/workspace/*/missing.txt"]);
    }
}
