//! Glob pattern matching for method names.
//!
//! Supports patterns like:
//! - `stripe/charges/create` - exact match
//! - `stripe/charges/*` - matches single path segment
//! - `stripe/**` - matches zero or more segments
//!
//! # Example
//!
//! ```rust
//! use amla_capabilities::patterns::{method_matches_pattern, pattern_is_subset};
//!
//! // Single-level wildcard
//! assert!(method_matches_pattern("stripe/charges/create", "stripe/charges/*"));
//! assert!(!method_matches_pattern("stripe/charges/refund/full", "stripe/charges/*"));
//!
//! // Multi-level wildcard
//! assert!(method_matches_pattern("stripe/charges/create", "stripe/**"));
//! assert!(method_matches_pattern("stripe/charges/refund/full", "stripe/**"));
//!
//! // Subset checking for attenuation
//! assert!(pattern_is_subset("stripe/charges/create", "stripe/charges/*"));
//! assert!(pattern_is_subset("stripe/charges/*", "stripe/**"));
//! ```

/// Check if a method name matches a glob pattern.
///
/// Pattern syntax:
/// - `*` matches exactly one path segment (no slashes)
/// - `**` matches zero or more path segments (including slashes)
/// - All other characters match literally
///
/// # Examples
///
/// ```rust
/// use amla_capabilities::patterns::method_matches_pattern;
///
/// // Exact match
/// assert!(method_matches_pattern("stripe/charges/create", "stripe/charges/create"));
///
/// // Single wildcard
/// assert!(method_matches_pattern("stripe/charges/create", "stripe/charges/*"));
/// assert!(method_matches_pattern("stripe/charges/refund", "stripe/charges/*"));
/// assert!(!method_matches_pattern("stripe/charges/refund/full", "stripe/charges/*"));
///
/// // Double wildcard
/// assert!(method_matches_pattern("stripe/charges/create", "stripe/**"));
/// assert!(method_matches_pattern("stripe/charges/refund/full", "stripe/**"));
/// assert!(method_matches_pattern("stripe", "stripe/**")); // ** matches zero segments
///
/// // Mixed patterns
/// assert!(method_matches_pattern("api/v2/users/get", "api/*/users/*"));
/// assert!(!method_matches_pattern("api/v2/v3/users/get", "api/*/users/*"));
/// ```
#[must_use]
pub fn method_matches_pattern(method: &str, pattern: &str) -> bool {
    // Handle empty string specially - it has zero segments
    let method_parts: Vec<&str> = if method.is_empty() {
        vec![]
    } else {
        method.split('/').collect()
    };

    let pattern_parts: Vec<&str> = if pattern.is_empty() {
        vec![]
    } else {
        pattern.split('/').collect()
    };

    matches_parts(&method_parts, &pattern_parts)
}

/// Recursive helper to match method parts against pattern parts.
fn matches_parts(method_parts: &[&str], pattern_parts: &[&str]) -> bool {
    // Base cases
    if pattern_parts.is_empty() {
        return method_parts.is_empty();
    }

    let pattern_first = pattern_parts[0];

    // Handle ** (matches zero or more segments)
    if pattern_first == "**" {
        let rest_pattern = &pattern_parts[1..];

        // ** at end matches everything
        if rest_pattern.is_empty() {
            return true;
        }

        // Try matching ** against 0, 1, 2, ... segments
        for i in 0..=method_parts.len() {
            if matches_parts(&method_parts[i..], rest_pattern) {
                return true;
            }
        }
        return false;
    }

    // Need at least one method part for non-** patterns
    if method_parts.is_empty() {
        return false;
    }

    let method_first = method_parts[0];

    // Handle * (matches exactly one segment)
    if pattern_first == "*" {
        return matches_parts(&method_parts[1..], &pattern_parts[1..]);
    }

    // Literal match
    if pattern_first == method_first {
        return matches_parts(&method_parts[1..], &pattern_parts[1..]);
    }

    false
}

/// Check if a child pattern is a subset of a parent pattern.
///
/// This is used for attenuation validation: a child capability with
/// a more specific pattern can be delegated from a parent with a
/// broader pattern.
///
/// # Rules
///
/// A child pattern is a subset of parent if every method that matches
/// the child also matches the parent. In practice:
///
/// - Exact pattern is subset of any pattern that would match it
/// - `a/b` ⊆ `a/*` ⊆ `a/**` ⊆ `**`
/// - `a/*/c` ⊆ `a/**/c` (but not ⊆ `a/**`)
///
/// # Examples
///
/// ```rust
/// use amla_capabilities::patterns::pattern_is_subset;
///
/// // Exact is subset of wildcard
/// assert!(pattern_is_subset("stripe/charges/create", "stripe/charges/*"));
/// assert!(pattern_is_subset("stripe/charges/create", "stripe/**"));
///
/// // Narrower wildcard is subset of broader
/// assert!(pattern_is_subset("stripe/charges/*", "stripe/**"));
/// assert!(pattern_is_subset("stripe/*", "stripe/**"));
///
/// // Same pattern is subset of itself
/// assert!(pattern_is_subset("stripe/**", "stripe/**"));
///
/// // ** is subset of only **
/// assert!(pattern_is_subset("**", "**"));
/// assert!(!pattern_is_subset("**", "stripe/**"));
///
/// // Non-subsets
/// assert!(!pattern_is_subset("github/**", "stripe/**"));
/// assert!(!pattern_is_subset("stripe/charges/**", "stripe/charges/*"));
/// ```
#[must_use]
pub fn pattern_is_subset(child: &str, parent: &str) -> bool {
    // Handle empty string specially - it has zero segments
    let child_parts: Vec<&str> = if child.is_empty() {
        vec![]
    } else {
        child.split('/').collect()
    };

    let parent_parts: Vec<&str> = if parent.is_empty() {
        vec![]
    } else {
        parent.split('/').collect()
    };

    pattern_parts_subset(&child_parts, &parent_parts)
}

/// Recursive helper for pattern subset checking.
fn pattern_parts_subset(child: &[&str], parent: &[&str]) -> bool {
    // Base cases
    if parent.is_empty() && child.is_empty() {
        return true;
    }

    if parent.is_empty() {
        // Parent exhausted but child has more - only ok if child is all literals
        // that would never match (child is more specific)
        return false;
    }

    let parent_first = parent[0];

    // Parent ** matches any child suffix
    if parent_first == "**" {
        // ** at end of parent accepts any child remainder
        if parent.len() == 1 {
            return true;
        }

        // Parent has more after **: child must eventually match rest
        // For subset: any suffix of child that matches rest of parent works
        let parent_rest = &parent[1..];
        for i in 0..=child.len() {
            if pattern_parts_subset(&child[i..], parent_rest) {
                return true;
            }
        }
        return false;
    }

    if child.is_empty() {
        // Child exhausted, parent has non-** parts remaining
        return false;
    }

    let child_first = child[0];

    // Child ** - can only be subset if parent also has ** here
    // (child ** matches more than parent * or literal)
    if child_first == "**" {
        // Child ** is only subset of parent ** at same position
        return parent_first == "**" && pattern_parts_subset(&child[1..], &parent[1..]);
    }

    // Child * can be subset of parent * or parent **
    if child_first == "*" {
        if parent_first == "*" || parent_first == "**" {
            // If parent is **, it's handled above, but let's be safe
            if parent_first == "**" {
                // * is subset of ** (both match single segment at this position)
                // But ** can match more, so continue checking
                return pattern_parts_subset(&child[1..], parent);
            }
            // Both are *, continue
            return pattern_parts_subset(&child[1..], &parent[1..]);
        }
        // Child * vs parent literal: * matches more than literal, not subset
        return false;
    }

    // Child is literal
    if parent_first == "*" {
        // Parent * matches any single segment including this literal
        return pattern_parts_subset(&child[1..], &parent[1..]);
    }

    if parent_first == "**" {
        // Already handled above
        unreachable!()
    }

    // Both literals - must match exactly
    if child_first == parent_first {
        return pattern_parts_subset(&child[1..], &parent[1..]);
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // method_matches_pattern tests
    // =========================================================================

    #[test]
    fn test_exact_match() {
        assert!(method_matches_pattern(
            "stripe/charges/create",
            "stripe/charges/create"
        ));
        assert!(!method_matches_pattern(
            "stripe/charges/create",
            "stripe/charges/refund"
        ));
    }

    #[test]
    fn test_single_wildcard() {
        // * matches exactly one segment
        assert!(method_matches_pattern(
            "stripe/charges/create",
            "stripe/charges/*"
        ));
        assert!(method_matches_pattern(
            "stripe/charges/refund",
            "stripe/charges/*"
        ));

        // * does not match zero segments
        assert!(!method_matches_pattern(
            "stripe/charges",
            "stripe/charges/*"
        ));

        // * does not match multiple segments
        assert!(!method_matches_pattern(
            "stripe/charges/refund/full",
            "stripe/charges/*"
        ));

        // * at different positions
        assert!(method_matches_pattern(
            "stripe/v2/charges",
            "stripe/*/charges"
        ));
        assert!(method_matches_pattern("github/v1/repos", "github/*/repos"));
    }

    #[test]
    fn test_double_wildcard() {
        // ** matches zero or more segments
        assert!(method_matches_pattern("stripe", "stripe/**"));
        assert!(method_matches_pattern("stripe/charges", "stripe/**"));
        assert!(method_matches_pattern("stripe/charges/create", "stripe/**"));
        assert!(method_matches_pattern("stripe/a/b/c/d/e", "stripe/**"));

        // ** alone matches everything
        assert!(method_matches_pattern("anything", "**"));
        assert!(method_matches_pattern("a/b/c/d", "**"));
        assert!(method_matches_pattern("", "**"));

        // ** at the start
        assert!(method_matches_pattern("a/b/create", "**/create"));
        assert!(method_matches_pattern("create", "**/create"));

        // ** in the middle
        assert!(method_matches_pattern(
            "stripe/a/b/c/charges",
            "stripe/**/charges"
        ));
        assert!(method_matches_pattern(
            "stripe/charges",
            "stripe/**/charges"
        ));
    }

    #[test]
    fn test_mixed_wildcards() {
        assert!(method_matches_pattern("api/v2/users/get", "api/*/users/*"));
        assert!(!method_matches_pattern(
            "api/v2/v3/users/get",
            "api/*/users/*"
        ));

        assert!(method_matches_pattern(
            "api/v2/x/y/users/get",
            "api/**/users/*"
        ));
        assert!(method_matches_pattern("api/users/get", "api/**/users/*"));
    }

    #[test]
    fn test_no_match() {
        assert!(!method_matches_pattern("github/repos", "stripe/**"));
        assert!(!method_matches_pattern("stripe/charges", "github/*"));
    }

    #[test]
    fn test_empty_cases() {
        assert!(method_matches_pattern("", ""));
        assert!(method_matches_pattern("", "**"));
        assert!(!method_matches_pattern("", "*"));
        assert!(!method_matches_pattern("foo", ""));
    }

    // =========================================================================
    // pattern_is_subset tests
    // =========================================================================

    #[test]
    fn test_exact_is_subset_of_wildcard() {
        assert!(pattern_is_subset(
            "stripe/charges/create",
            "stripe/charges/*"
        ));
        assert!(pattern_is_subset("stripe/charges/create", "stripe/**"));
        assert!(pattern_is_subset("stripe/charges/create", "**"));
    }

    #[test]
    fn test_star_is_subset_of_double_star() {
        assert!(pattern_is_subset("stripe/charges/*", "stripe/charges/**"));
        assert!(pattern_is_subset("stripe/*", "stripe/**"));
        assert!(pattern_is_subset("*", "**"));
    }

    #[test]
    fn test_narrower_double_star_subset() {
        assert!(pattern_is_subset("stripe/charges/**", "stripe/**"));
        assert!(pattern_is_subset("stripe/**", "**"));
    }

    #[test]
    fn test_same_pattern_is_subset() {
        assert!(pattern_is_subset("stripe/charges/*", "stripe/charges/*"));
        assert!(pattern_is_subset("stripe/**", "stripe/**"));
        assert!(pattern_is_subset("**", "**"));
        assert!(pattern_is_subset(
            "stripe/charges/create",
            "stripe/charges/create"
        ));
    }

    #[test]
    fn test_double_star_not_subset_of_star() {
        // ** matches more than *, so ** is NOT subset of *
        assert!(!pattern_is_subset("stripe/charges/**", "stripe/charges/*"));
        assert!(!pattern_is_subset("stripe/**", "stripe/*"));
    }

    #[test]
    fn test_disjoint_patterns_not_subset() {
        assert!(!pattern_is_subset("github/**", "stripe/**"));
        assert!(!pattern_is_subset("stripe/charges/*", "stripe/refunds/*"));
    }

    #[test]
    fn test_global_double_star() {
        // ** is only subset of **
        assert!(pattern_is_subset("**", "**"));
        assert!(!pattern_is_subset("**", "stripe/**"));
        assert!(!pattern_is_subset("**", "*"));
    }

    #[test]
    fn test_complex_subset_cases() {
        // api/*/users/* is subset of api/**/users/*
        assert!(pattern_is_subset("api/v1/users/*", "api/*/users/*"));
        assert!(pattern_is_subset("api/*/users/*", "api/**/users/*"));

        // But not the other way
        assert!(!pattern_is_subset("api/**/users/*", "api/*/users/*"));
    }

    #[test]
    fn test_empty_patterns() {
        assert!(pattern_is_subset("", ""));
        assert!(pattern_is_subset("", "**"));
        assert!(!pattern_is_subset("foo", ""));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // Strategy for method path segments
    fn segment() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9]{0,10}"
    }

    // Strategy for method paths (1-5 segments)
    fn method_path() -> impl Strategy<Value = String> {
        prop::collection::vec(segment(), 1..=5).prop_map(|segs| segs.join("/"))
    }

    proptest! {
        /// A method always matches itself
        #[test]
        fn method_matches_itself(method in method_path()) {
            prop_assert!(method_matches_pattern(&method, &method));
        }

        /// ** matches any method
        #[test]
        fn double_star_matches_all(method in method_path()) {
            prop_assert!(method_matches_pattern(&method, "**"));
        }

        /// If child pattern is subset of parent, then any method matching child also matches parent
        #[test]
        fn subset_implies_match_preserved(method in method_path()) {
            // Build patterns from the method
            let parts: Vec<&str> = method.split('/').collect();
            if parts.len() >= 2 {
                // Create child pattern: exact match
                let child = method.clone();
                // Create parent pattern: replace last segment with *
                let mut parent_parts = parts.clone();
                parent_parts[parts.len() - 1] = "*";
                let parent = parent_parts.join("/");

                // Verify subset relationship
                if pattern_is_subset(&child, &parent) {
                    // If method matches child, it must match parent
                    if method_matches_pattern(&method, &child) {
                        prop_assert!(method_matches_pattern(&method, &parent),
                            "method {} matches child {} but not parent {}",
                            method, child, parent);
                    }
                }
            }
        }

        /// A pattern is always subset of itself
        #[test]
        fn pattern_is_subset_of_itself(method in method_path()) {
            prop_assert!(pattern_is_subset(&method, &method));
        }

        /// Everything is subset of **
        #[test]
        fn everything_is_subset_of_double_star(pattern in method_path()) {
            prop_assert!(pattern_is_subset(&pattern, "**"));
        }
    }
}
