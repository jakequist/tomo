//! Repo-relative, validated, normalized path newtype.
//!
//! The engine identifies every file by a [`RelPath`]: a forward-slash
//! separated path relative to the project root. Validation happens once, at
//! construction, so every path already inside the engine is guaranteed
//! well-formed — no absolute paths, no `..` traversal, no platform-specific
//! separators to normalize at decision time.
//!
//! # Why `.tomo` is unrepresentable (CLAUDE.md invariant #1)
//! Tomo must never sync, watch, or version its own state directory. The
//! lowest layer ignores `.tomo/**`, but we defend in depth: a path whose
//! *first* component is `.tomo` cannot be turned into a `RelPath` at all, so
//! no engine code path can accidentally construct, index, or transfer one.

use std::fmt;

/// Why a string was rejected as a [`RelPath`].
///
/// Each variant names one normalization rule. Kept as an explicit enum (rather
/// than a single opaque message) so callers — and future CLI diagnostics — can
/// react to specific failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The path was the empty string.
    Empty,
    /// The path was absolute (began with `/`).
    Absolute,
    /// The path ended with a `/` (a trailing separator).
    TrailingSlash,
    /// The path contained a backslash (not a legal separator here).
    Backslash,
    /// The path contained a NUL byte.
    NulByte,
    /// The path contained an empty component (e.g. `a//b`).
    EmptyComponent,
    /// The path contained a `.` or `..` component.
    DotComponent,
    /// The path's first component is the reserved `.tomo` state directory.
    ReservedTomo,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            PathError::Empty => "path is empty",
            PathError::Absolute => "path is absolute (must be repo-relative)",
            PathError::TrailingSlash => "path has a trailing slash",
            PathError::Backslash => "path contains a backslash",
            PathError::NulByte => "path contains a NUL byte",
            PathError::EmptyComponent => "path contains an empty component",
            PathError::DotComponent => "path contains a '.' or '..' component",
            PathError::ReservedTomo => "path is inside the reserved .tomo directory",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for PathError {}

/// A validated, normalized, repo-relative path.
///
/// Invariants guaranteed for any value that exists:
/// - non-empty, relative (no leading `/`), no trailing `/`;
/// - forward-slash separators only (no backslashes), no NUL bytes;
/// - no empty, `.`, or `..` components;
/// - first component is never `.tomo` (invariant #1, defense in depth).
///
/// Ordering is lexicographic over the underlying string, which gives the
/// deterministic iteration order the index and its canonical digest rely on.
///
/// ```
/// use tomo_engine::RelPath;
/// let p = RelPath::new("src/main.rs").expect("valid");
/// assert_eq!(p.as_str(), "src/main.rs");
/// assert_eq!(p.file_name(), "main.rs");
/// assert!(RelPath::new("../etc/passwd").is_err());
/// assert!(RelPath::new(".tomo/state.db").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Validate and normalize `raw` into a `RelPath`.
    ///
    /// # Errors
    /// Returns the specific [`PathError`] for the first rule violated. The
    /// checks are ordered so the most structural failure wins (e.g. an
    /// absolute path reports [`PathError::Absolute`], not a component error).
    pub fn new(raw: &str) -> Result<RelPath, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.contains('\0') {
            return Err(PathError::NulByte);
        }
        if raw.contains('\\') {
            return Err(PathError::Backslash);
        }
        if raw.starts_with('/') {
            return Err(PathError::Absolute);
        }
        if raw.ends_with('/') {
            return Err(PathError::TrailingSlash);
        }
        let mut first: Option<&str> = None;
        for comp in raw.split('/') {
            if comp.is_empty() {
                return Err(PathError::EmptyComponent);
            }
            if comp == "." || comp == ".." {
                return Err(PathError::DotComponent);
            }
            if first.is_none() {
                first = Some(comp);
            }
        }
        if first == Some(".tomo") {
            return Err(PathError::ReservedTomo);
        }
        Ok(RelPath(raw.to_owned()))
    }

    /// The path as a `&str` (forward-slash separated, no leading/trailing `/`).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterate the path components in order.
    ///
    /// Every yielded component is non-empty and is neither `.` nor `..`, by
    /// construction, so callers need not re-validate.
    pub fn components(&self) -> impl Iterator<Item = &str> + '_ {
        self.0.split('/')
    }

    /// The final path component (the file or directory name).
    ///
    /// Never empty: a `RelPath` always has at least one component.
    pub fn file_name(&self) -> &str {
        // `rsplit` on a validated, non-empty path always yields a first item.
        match self.0.rsplit('/').next() {
            Some(name) => name,
            // Unreachable: `split`/`rsplit` on any `&str` yields ≥1 element.
            None => &self.0,
        }
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<RelPath> for String {
    fn from(p: RelPath) -> String {
        p.0
    }
}

impl TryFrom<String> for RelPath {
    type Error = PathError;

    fn try_from(s: String) -> Result<RelPath, PathError> {
        RelPath::new(&s)
    }
}

// Deserialize re-runs full validation via `TryFrom<String>`, so an untrusted
// or corrupt wire/on-disk value can never yield an invalid `RelPath`.
impl<'de> serde::Deserialize<'de> for RelPath {
    fn deserialize<D>(deserializer: D) -> Result<RelPath, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        RelPath::new(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_simple_paths() {
        for ok in [
            "a",
            "a/b",
            "src/main.rs",
            "a/b/c/d.txt",
            ".tomofoo",
            "a/.tomo",
        ] {
            assert!(RelPath::new(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(RelPath::new(""), Err(PathError::Empty));
    }

    #[test]
    fn rejects_absolute() {
        assert_eq!(RelPath::new("/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(RelPath::new("/"), Err(PathError::Absolute));
    }

    #[test]
    fn rejects_trailing_slash() {
        assert_eq!(RelPath::new("a/b/"), Err(PathError::TrailingSlash));
        assert_eq!(RelPath::new("a/"), Err(PathError::TrailingSlash));
    }

    #[test]
    fn rejects_backslash() {
        assert_eq!(RelPath::new("a\\b"), Err(PathError::Backslash));
    }

    #[test]
    fn rejects_nul() {
        assert_eq!(RelPath::new("a\0b"), Err(PathError::NulByte));
    }

    #[test]
    fn rejects_empty_component() {
        assert_eq!(RelPath::new("a//b"), Err(PathError::EmptyComponent));
    }

    #[test]
    fn rejects_dot_components() {
        assert_eq!(RelPath::new("."), Err(PathError::DotComponent));
        assert_eq!(RelPath::new(".."), Err(PathError::DotComponent));
        assert_eq!(RelPath::new("a/./b"), Err(PathError::DotComponent));
        assert_eq!(RelPath::new("a/../b"), Err(PathError::DotComponent));
        assert_eq!(RelPath::new("../b"), Err(PathError::DotComponent));
    }

    #[test]
    fn rejects_dot_tomo_first_component() {
        assert_eq!(RelPath::new(".tomo"), Err(PathError::ReservedTomo));
        assert_eq!(RelPath::new(".tomo/state.db"), Err(PathError::ReservedTomo));
        assert_eq!(RelPath::new(".tomo/bin/tomo"), Err(PathError::ReservedTomo));
    }

    #[test]
    fn accessors() {
        let p = RelPath::new("src/a/main.rs").unwrap();
        assert_eq!(p.as_str(), "src/a/main.rs");
        assert_eq!(p.file_name(), "main.rs");
        assert_eq!(p.components().collect::<Vec<_>>(), ["src", "a", "main.rs"]);
        let single = RelPath::new("README").unwrap();
        assert_eq!(single.file_name(), "README");
    }

    /// A strategy yielding syntactically valid relative paths.
    ///
    /// Components are drawn from `[a-z][a-z0-9]*`, so none can be empty, `.`,
    /// `..`, or `.tomo`, and the join never introduces `//` or a leading /
    /// trailing slash.
    fn arb_relpath() -> impl Strategy<Value = RelPath> {
        proptest::collection::vec("[a-z][a-z0-9]{0,4}", 1..5)
            .prop_map(|parts| RelPath::new(&parts.join("/")).expect("generated path is valid"))
    }

    proptest! {
        #[test]
        fn generated_paths_are_valid(p in arb_relpath()) {
            prop_assert!(RelPath::new(p.as_str()).is_ok());
        }

        /// Serialization round-trip via the exact conversions serde uses
        /// (`#[serde(into = "String")]` / `TryFrom<String>`): every valid
        /// path survives serialize → deserialize unchanged.
        #[test]
        fn conversion_round_trip(p in arb_relpath()) {
            let s: String = p.clone().into();
            let back = RelPath::try_from(s).expect("round-trips");
            prop_assert_eq!(p, back);
        }

        /// The first component always drives the ordering prefix, and no
        /// generated path is ever rejected as `.tomo`.
        #[test]
        fn never_spuriously_reserved(p in arb_relpath()) {
            prop_assert_ne!(p.components().next(), Some(".tomo"));
        }
    }
}
