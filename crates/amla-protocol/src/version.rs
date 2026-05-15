//! Protocol version type.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Protocol version.
///
/// Follows semantic versioning principles for compatibility:
/// - Same major version: compatible
/// - Different major version: incompatible
///
/// # Example
///
/// ```
/// use amla_protocol::Version;
///
/// let v1 = Version::new(0, 1);
/// let v2 = Version::new(0, 2);
///
/// assert!(v1.is_compatible_with(&v2));
/// assert_eq!(v1.to_string(), "0.1");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version {
    major: u32,
    minor: u32,
}

impl Version {
    /// Create a new version.
    ///
    /// # Panics
    ///
    /// This function does not panic as version components are unsigned.
    #[must_use]
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Get the major version component.
    #[must_use]
    pub const fn major(&self) -> u32 {
        self.major
    }

    /// Get the minor version component.
    #[must_use]
    pub const fn minor(&self) -> u32 {
        self.minor
    }

    /// Check if this version is compatible with another.
    ///
    /// Compatibility rules:
    /// - Same major version: compatible
    /// - Different major version: incompatible
    #[must_use]
    pub const fn is_compatible_with(&self, other: &Self) -> bool {
        self.major == other.major
    }

    /// Convert to a tuple for serialization.
    #[must_use]
    pub const fn to_tuple(&self) -> (u32, u32) {
        (self.major, self.minor)
    }

    /// Create from a tuple.
    #[must_use]
    pub const fn from_tuple(t: (u32, u32)) -> Self {
        Self::new(t.0, t.1)
    }

    /// Parse from a string like "0.1".
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 2 {
            return Err(Error::InvalidVersion(format!(
                "expected format 'major.minor', got '{s}'"
            )));
        }

        let major = parts[0]
            .parse()
            .map_err(|_| Error::InvalidVersion(format!("invalid major version: '{}'", parts[0])))?;
        let minor = parts[1]
            .parse()
            .map_err(|_| Error::InvalidVersion(format!("invalid minor version: '{}'", parts[1])))?;

        Ok(Self::new(major, minor))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl Default for Version {
    fn default() -> Self {
        PROTOCOL_VERSION
    }
}

/// Current protocol version.
pub const PROTOCOL_VERSION: Version = Version::new(0, 1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_ordering() {
        let v01 = Version::new(0, 1);
        let v02 = Version::new(0, 2);
        let v10 = Version::new(1, 0);

        assert!(v01 < v02);
        assert!(v02 < v10);
        assert!(v01 < v10);
    }

    #[test]
    fn test_version_compatibility() {
        let v01 = Version::new(0, 1);
        let v02 = Version::new(0, 2);
        let v10 = Version::new(1, 0);

        assert!(v01.is_compatible_with(&v02));
        assert!(!v01.is_compatible_with(&v10));
    }

    #[test]
    fn test_version_parse() {
        assert_eq!(Version::parse("0.1").unwrap(), Version::new(0, 1));
        assert_eq!(Version::parse("1.23").unwrap(), Version::new(1, 23));

        assert!(Version::parse("").is_err());
        assert!(Version::parse("1").is_err());
        assert!(Version::parse("1.2.3").is_err());
        assert!(Version::parse("a.b").is_err());
    }

    #[test]
    fn test_version_display() {
        assert_eq!(Version::new(0, 1).to_string(), "0.1");
        assert_eq!(Version::new(12, 34).to_string(), "12.34");
    }

    #[test]
    fn test_version_accessors() {
        let v = Version::new(5, 10);
        assert_eq!(v.major(), 5);
        assert_eq!(v.minor(), 10);
    }

    #[test]
    fn test_version_default() {
        let v: Version = Version::default();
        assert_eq!(v, PROTOCOL_VERSION);
        assert_eq!(v.major(), 0);
        assert_eq!(v.minor(), 1);
    }

    #[test]
    fn test_version_parse_invalid_minor() {
        // Test specifically the invalid minor version path
        let result = Version::parse("1.xyz");
        assert!(result.is_err());
        if let Err(Error::InvalidVersion(msg)) = result {
            assert!(msg.contains("minor"));
        } else {
            panic!("Expected InvalidVersion error with 'minor' in message");
        }
    }

    #[test]
    fn test_version_tuple_conversion() {
        let v = Version::new(3, 7);
        let tuple = v.to_tuple();
        assert_eq!(tuple, (3, 7));

        let v2 = Version::from_tuple(tuple);
        assert_eq!(v, v2);
    }

    #[test]
    fn test_version_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Version::new(0, 1));
        set.insert(Version::new(0, 1)); // Duplicate
        set.insert(Version::new(0, 2));

        assert_eq!(set.len(), 2);
        assert!(set.contains(&Version::new(0, 1)));
        assert!(set.contains(&Version::new(0, 2)));
    }
}
