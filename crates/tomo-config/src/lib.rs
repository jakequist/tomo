//! Tomo configuration: the `.tomo/config.toml` model, the three path classes,
//! git-style glob rules, and the hardcoded `.tomo/**` ignore guarantee.
//!
//! This crate is **read-only I/O**: it may read a config file from disk, but it
//! never writes anything. See `docs/SPEC.md` §4 (state layout) and §7
//! (configuration).
//!
//! # Path classes
//!
//! Every path in the tree is classified into exactly one of three classes
//! ([`PathClass`]):
//!
//! - `synced+versioned` — source files; synced and captured in history. This is
//!   the global default for any path no rule matches.
//! - `synced+unversioned` — flows between machines but is never versioned
//!   (e.g. build artifacts you want mirrored back without history).
//! - `ignored` — never crosses the wire, never versioned (e.g. `target/`).
//!
//! # The `.tomo/**` supremacy rule
//!
//! [`is_tomo_internal`] is a **constant-level guarantee**, not a config default:
//! anything at or under the project-root `.tomo/` directory is always
//! [`PathClass::Ignored`], consulted *before* any user rule. No configuration
//! can make `.tomo/**` synced or versioned. See CLAUDE.md invariant #1.
//!
//! # Glob semantics
//!
//! Patterns are matched against the **full path relative to the project root**
//! (forward-slash separated, no leading `./`). Matching is git-*style* but
//! root-anchored:
//!
//! - `*` and `?` never cross a `/` (they match within a single path component).
//! - `**` matches across directory boundaries (`target/**`, `**/*.log`).
//! - A pattern without a `/` is still anchored to the root: `*.log` matches only
//!   top-level `*.log` files; use `**/*.log` to match in every directory.
//! - A trailing-slash pattern is auto-expanded for ergonomics: `target/` is
//!   treated as `target/**` (see [`Config::from_toml_str`]).
//!
//! When several rules match a path, the **last** matching rule in the file wins
//! (git-style precedence).
//!
//! # Built-in default ignores (precedence matters)
//!
//! Unless `[sync] default_ignores = false`, Tomo prepends a small set of
//! built-in ignore rules for common editor/tool temp files ([`DEFAULT_IGNORE_PATTERNS`])
//! **before** any user rule. Because the last matching rule wins, these defaults
//! sit at the *bottom* of the precedence stack: a user rule matching the same
//! path always overrides a default (e.g. a `synced+versioned` rule on `**/*.swp`
//! re-includes vim swap files). They exist to stop editor churn — swap files,
//! backups, emacs lockfiles, vim's `4913` write-probe — from crossing the wire
//! or polluting history by default. Set `default_ignores = false` to disable
//! them entirely and get the pre-defaults behavior.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::de::{self, Deserializer};
use serde::Deserialize;

/// Name of Tomo's per-project state directory (`<project_root>/.tomo`).
///
/// All Tomo state lives under this directory and nothing inside it is ever
/// synced or versioned. See [`is_tomo_internal`].
pub const TOMO_DIR: &str = ".tomo";

/// Config file name inside [`TOMO_DIR`].
const CONFIG_FILE: &str = "config.toml";

/// Built-in glob patterns for common editor/tool temp files, ignored by default.
///
/// Applied **before** any user rule (so a user rule for the same pattern wins —
/// last match wins), and only when `[sync] default_ignores` is `true` (the
/// default). Each is anchored with `**/` so it matches in every directory,
/// including the project root. See the crate-level "Built-in default ignores"
/// section.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "**/*.swp",  // vim swap files
    "**/*.swx",  // vim swap files (secondary)
    "**/.*.sw?", // vim dot-prefixed swap variants (.main.rs.swp, …)
    "**/*~",     // emacs/gedit/kate backup files
    "**/.#*",    // emacs lock files
    "**/#*#",    // emacs auto-save files
    "**/4913",   // vim's writability write-probe file
];

/// Build the built-in default ignore rules ([`DEFAULT_IGNORE_PATTERNS`], each
/// [`PathClass::Ignored`] in [`Direction::Both`]).
fn default_ignore_rules() -> Vec<Rule> {
    DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|pattern| Rule {
            pattern: (*pattern).to_owned(),
            class: PathClass::Ignored,
            direction: Direction::Both,
        })
        .collect()
}

/// Returns `true` iff `path` is the project-root state directory itself
/// (`.tomo`) or lives beneath it (`.tomo` is its first component).
///
/// This is the lowest-layer, constant-level enforcement of CLAUDE.md invariant
/// #1: **no configuration can ever make `.tomo/**` synced or versioned.**
/// [`Config::classify`] calls this before consulting any user rule.
///
/// Only the *first* path component counts — a nested `.tomo` (e.g. `a/.tomo`)
/// is an ordinary user path, because Tomo's state directory is always at the
/// project root.
///
/// `path` is expected to be a forward-slash-separated path relative to the
/// project root.
///
/// # Examples
///
/// ```
/// use tomo_config::is_tomo_internal;
///
/// assert!(is_tomo_internal(".tomo"));
/// assert!(is_tomo_internal(".tomo/db/history.sqlite"));
/// assert!(!is_tomo_internal(".tomodachi")); // not the .tomo component
/// assert!(!is_tomo_internal("a/.tomo")); // only the FIRST component counts
/// assert!(!is_tomo_internal("src/main.rs"));
/// ```
#[must_use]
pub fn is_tomo_internal(path: &str) -> bool {
    path == TOMO_DIR
        || path
            .strip_prefix(TOMO_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// One of the three path classes a file can belong to (`docs/SPEC.md` §7).
///
/// [`PathClass::SyncedVersioned`] is the global default applied to any path that
/// matches no rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum PathClass {
    /// Synced between machines and captured in history. The default class.
    #[default]
    #[serde(rename = "synced+versioned")]
    SyncedVersioned,
    /// Synced between machines but never versioned.
    #[serde(rename = "synced+unversioned")]
    SyncedUnversioned,
    /// Never synced and never versioned.
    #[serde(rename = "ignored")]
    Ignored,
}

/// Sync direction a rule applies to.
///
/// Direction is load-bearing: a pull-only rule on a server build directory stops
/// a `target/`-spraying build from growing history at build speed
/// (`docs/SPEC.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Sync in both directions. The default.
    #[default]
    Both,
    /// Local → remote only.
    Push,
    /// Remote → local only.
    Pull,
}

/// How history capture behaves (`docs/SPEC.md` §6.2).
///
/// # TOML syntax
///
/// The `history.mode` value is either one of the named strings or an inline
/// table selecting a fixed interval:
///
/// ```toml
/// [history]
/// mode = "adaptive"              # default: purity under light load, debounce under pressure
/// # mode = "every-change"       # every save becomes a version
/// # mode = "off"                # no history capture
/// # mode = { interval_ms = 5000 } # coalesce into one version per interval
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HistoryMode {
    /// Flush immediately under light load, escalate the flush interval under
    /// pressure, decay back to immediate when idle. The default.
    #[default]
    Adaptive,
    /// Every canonical change becomes its own version.
    EveryChange,
    /// Disable history capture entirely.
    Off,
    /// Coalesce a burst into one version per fixed interval, in milliseconds.
    Interval {
        /// Flush interval in milliseconds.
        interval_ms: u64,
    },
}

impl<'de> Deserialize<'de> for HistoryMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Accept either a bare string ("adaptive" | "every-change" | "off") or
        // an inline table `{ interval_ms = N }`. `untagged` tries the string
        // form first, then the table form.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Named(String),
            Interval { interval_ms: u64 },
        }

        match Repr::deserialize(deserializer)? {
            Repr::Named(name) => match name.as_str() {
                "adaptive" => Ok(HistoryMode::Adaptive),
                "every-change" => Ok(HistoryMode::EveryChange),
                "off" => Ok(HistoryMode::Off),
                other => Err(de::Error::unknown_variant(
                    other,
                    &["adaptive", "every-change", "off", "{ interval_ms = N }"],
                )),
            },
            Repr::Interval { interval_ms } => Ok(HistoryMode::Interval { interval_ms }),
        }
    }
}

/// The `[history]` configuration section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// History capture strategy. Defaults to [`HistoryMode::Adaptive`].
    #[serde(default)]
    pub mode: HistoryMode,
}

/// The `[sync]` configuration section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    /// Whether the built-in editor/tool temp-file ignore rules
    /// ([`DEFAULT_IGNORE_PATTERNS`]) are applied. `true` by default; set to
    /// `false` to disable them and restore the pre-defaults behavior. They are
    /// always overridable by a user rule regardless (last match wins).
    #[serde(default = "default_true")]
    pub default_ignores: bool,
}

/// Serde default for [`Sync::default_ignores`] (built-in ignores on by default).
fn default_true() -> bool {
    true
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            default_ignores: true,
        }
    }
}

/// The optional `[remote]` section describing the sync peer.
///
/// Written by `tomo connect` in a later milestone; parsed and exposed now.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// SSH target string (e.g. `user@host`).
    pub host: String,
    /// Remote project-root path. Kept as an opaque string because it is the
    /// peer's path, which may use conventions the local platform does not.
    pub path: String,
}

/// A single `[[rules]]` entry: a glob pattern bound to a class and direction.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Glob pattern, relative to the project root. See the crate-level docs for
    /// glob semantics.
    pub pattern: String,
    /// Class assigned to paths this rule matches.
    pub class: PathClass,
    /// Direction assigned to paths this rule matches. Defaults to
    /// [`Direction::Both`].
    #[serde(default)]
    pub direction: Direction,
}

/// The result of classifying a path: its resolved class and direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    /// Resolved path class.
    pub class: PathClass,
    /// Resolved sync direction.
    pub direction: Direction,
}

impl Classification {
    /// The classification applied to a path that matches no rule:
    /// [`PathClass::SyncedVersioned`] in [`Direction::Both`].
    const DEFAULT: Self = Self {
        class: PathClass::SyncedVersioned,
        direction: Direction::Both,
    };

    /// The classification forced onto `.tomo/**`: [`PathClass::Ignored`].
    const IGNORED: Self = Self {
        class: PathClass::Ignored,
        direction: Direction::Both,
    };
}

/// The `[history]`/`[remote]`/`[[rules]]` sections exactly as they appear on
/// disk, before glob compilation. Private wire model for [`Config`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    history: History,
    #[serde(default)]
    sync: Sync,
    #[serde(default)]
    remote: Option<Remote>,
    #[serde(default)]
    rules: Vec<Rule>,
}

/// A fully parsed and compiled Tomo configuration.
///
/// Construct one with [`Config::load`] (from `<root>/.tomo/config.toml`),
/// [`Config::from_toml_str`] (from an in-memory document), or
/// [`Config::default`] (the empty configuration, matching an absent file).
///
/// [`Config::default`] and an absent config file are equivalent: adaptive
/// history, no remote, no rules — so every path outside `.tomo/` is
/// `synced+versioned`, `both`.
#[derive(Debug, Clone)]
pub struct Config {
    /// The `[history]` section.
    pub history: History,
    /// The `[sync]` section.
    pub sync: Sync,
    /// The optional `[remote]` section.
    pub remote: Option<Remote>,
    /// The user's `[[rules]]` exactly as written. Order is significant: the last
    /// matching rule wins. Does **not** include the built-in default ignores —
    /// see [`Config::effective`] for the list actually matched against.
    pub rules: Vec<Rule>,
    /// The rule list [`Config::classify`] actually matches against: the built-in
    /// default ignores (when enabled) first, then the user's [`Config::rules`].
    /// Last match wins, so a user rule overrides a same-pattern default.
    effective: Vec<Rule>,
    /// Compiled matcher; the `i`-th glob corresponds to `effective[i]`.
    rule_set: GlobSet,
}

impl Default for Config {
    fn default() -> Self {
        // The empty document with defaults on: adaptive history, default ignores
        // enabled, no remote, no user rules. Built from a `RawConfig::default`
        // so the built-in ignore rules are compiled in; the fallback is
        // unreachable (the default patterns are known-valid globs) but avoids an
        // `unwrap`/`expect` in library code (rust-hygiene).
        Self::from_raw(RawConfig::default(), None).unwrap_or_else(|_| Self {
            history: History::default(),
            sync: Sync::default(),
            remote: None,
            rules: Vec::new(),
            effective: Vec::new(),
            rule_set: GlobSet::empty(),
        })
    }
}

impl Config {
    /// Loads and compiles `<project_root>/.tomo/config.toml`.
    ///
    /// A missing file yields [`Config::default`]. A read failure, malformed
    /// TOML, or an invalid glob yields a [`ConfigError`] carrying the file path
    /// and cause.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Read`] if the file exists but cannot be read.
    /// - [`ConfigError::Parse`] if the file is not valid TOML for the schema.
    /// - [`ConfigError::Glob`] if a rule pattern is not a valid glob.
    pub fn load(project_root: &Path) -> Result<Self, ConfigError> {
        let path = project_root.join(TOMO_DIR).join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let raw: RawConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            location: Some(path.clone()),
            source: Box::new(source),
        })?;
        Self::from_raw(raw, Some(&path))
    }

    /// Parses and compiles a configuration from an in-memory TOML document.
    ///
    /// Useful for tests and callers that already hold the document text. Errors
    /// carry no file path (there is none).
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Parse`] if `text` is not valid TOML for the schema.
    /// - [`ConfigError::Glob`] if a rule pattern is not a valid glob.
    ///
    /// # Examples
    ///
    /// ```
    /// use tomo_config::{Config, Direction, PathClass};
    ///
    /// let cfg = Config::from_toml_str(
    ///     r#"
    ///     [[rules]]
    ///     pattern = "target/"          # trailing slash auto-expands to target/**
    ///     class = "ignored"
    ///
    ///     [[rules]]
    ///     pattern = "target/release/**"
    ///     class = "synced+unversioned"
    ///     direction = "pull"
    ///     "#,
    /// )
    /// .unwrap();
    ///
    /// // Last matching rule wins.
    /// let c = cfg.classify("target/release/app");
    /// assert_eq!(c.class, PathClass::SyncedUnversioned);
    /// assert_eq!(c.direction, Direction::Pull);
    ///
    /// assert_eq!(cfg.classify("target/debug/app").class, PathClass::Ignored);
    /// assert_eq!(cfg.classify("src/main.rs").class, PathClass::SyncedVersioned);
    /// ```
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Parse {
            location: None,
            source: Box::new(source),
        })?;
        Self::from_raw(raw, None)
    }

    /// Compiles a [`RawConfig`] into a [`Config`], building the glob matcher.
    ///
    /// The effective rule list is the built-in default ignores (when
    /// `[sync] default_ignores` is enabled) followed by the user's rules, so a
    /// user rule always overrides a same-pattern default (last match wins).
    fn from_raw(raw: RawConfig, location: Option<&Path>) -> Result<Self, ConfigError> {
        let mut effective = if raw.sync.default_ignores {
            default_ignore_rules()
        } else {
            Vec::new()
        };
        effective.extend(raw.rules.iter().cloned());

        let mut builder = GlobSetBuilder::new();
        for rule in &effective {
            let expanded = expand_pattern(&rule.pattern);
            let glob = GlobBuilder::new(&expanded)
                // git-style: `*`/`?` stay within a component, `**` crosses `/`.
                .literal_separator(true)
                .build()
                .map_err(|source| ConfigError::Glob {
                    location: location.map(Path::to_path_buf),
                    pattern: rule.pattern.clone(),
                    source: Box::new(source),
                })?;
            builder.add(glob);
        }
        let rule_set = builder.build().map_err(|source| ConfigError::Glob {
            location: location.map(Path::to_path_buf),
            // Individual globs already validated; a build failure is not tied to
            // one pattern.
            pattern: String::new(),
            source: Box::new(source),
        })?;
        Ok(Self {
            history: raw.history,
            sync: raw.sync,
            remote: raw.remote,
            rules: raw.rules,
            effective,
            rule_set,
        })
    }

    /// Classifies a path into its resolved [`PathClass`] and [`Direction`].
    ///
    /// Resolution order:
    ///
    /// 1. If [`is_tomo_internal`] is true, the path is **always**
    ///    [`PathClass::Ignored`] — no rule is consulted (CLAUDE.md invariant #1).
    /// 2. Otherwise the last matching rule wins (git-style precedence), where
    ///    the rule list is the built-in default ignores (when enabled) followed
    ///    by the user's rules — so a user rule always overrides a same-pattern
    ///    default.
    /// 3. With no matching rule, the default is
    ///    [`PathClass::SyncedVersioned`] in [`Direction::Both`].
    ///
    /// `path` is a forward-slash-separated path relative to the project root.
    #[must_use]
    pub fn classify(&self, path: &str) -> Classification {
        if is_tomo_internal(path) {
            return Classification::IGNORED;
        }
        match self
            .rule_set
            .matches(path)
            .into_iter()
            .max()
            .and_then(|idx| self.effective.get(idx))
        {
            Some(rule) => Classification {
                class: rule.class,
                direction: rule.direction,
            },
            None => Classification::DEFAULT,
        }
    }
}

/// Expands a trailing-slash directory pattern (`target/`) to a recursive
/// pattern (`target/**`) for ergonomics; leaves other patterns untouched.
fn expand_pattern(pattern: &str) -> String {
    match pattern.strip_suffix('/') {
        Some(prefix) => format!("{prefix}/**"),
        None => pattern.to_owned(),
    }
}

/// Errors produced while loading or parsing a [`Config`].
///
/// Every variant carries the failure cause; disk-backed loads also carry the
/// config file path. Only the `tomo` CLI renders these to humans.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file exists but could not be read.
    #[error("failed to read config file {}: {source}", path.display())]
    Read {
        /// Path of the file that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The config document is not valid TOML for the schema.
    #[error("invalid config{}: {source}", describe_location(location.as_ref()))]
    Parse {
        /// File the document came from, if any.
        location: Option<PathBuf>,
        /// Underlying TOML deserialization error.
        source: Box<toml::de::Error>,
    },

    /// A rule pattern is not a valid glob.
    #[error(
        "invalid glob pattern {pattern:?}{}: {source}",
        describe_location(location.as_ref())
    )]
    Glob {
        /// File the pattern came from, if any.
        location: Option<PathBuf>,
        /// The offending pattern (as written in the config).
        pattern: String,
        /// Underlying glob compilation error.
        source: Box<globset::Error>,
    },
}

/// Renders the optional config-file location as a ` in <path>` suffix (or an
/// empty string) for [`ConfigError`] messages.
fn describe_location(location: Option<&PathBuf>) -> String {
    location
        .map(|path| format!(" in {}", path.display()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    // Tests may panic freely; the library code paths above may not.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use proptest::prelude::*;

    // ---- is_tomo_internal edge cases -------------------------------------

    #[test]
    fn tomo_internal_matches_dir_and_descendants() {
        assert!(is_tomo_internal(".tomo"));
        assert!(is_tomo_internal(".tomo/x"));
        assert!(is_tomo_internal(".tomo/db/history.sqlite"));
        assert!(is_tomo_internal(".tomo/"));
    }

    #[test]
    fn tomo_internal_rejects_lookalikes_and_nested() {
        assert!(!is_tomo_internal(".tomodachi"));
        assert!(!is_tomo_internal("a/.tomo")); // only the first component counts
        assert!(!is_tomo_internal("a/.tomo/x"));
        assert!(!is_tomo_internal("src/main.rs"));
        assert!(!is_tomo_internal(""));
        assert!(!is_tomo_internal(".tomoo/x"));
    }

    // ---- parsing ---------------------------------------------------------

    #[test]
    fn empty_document_equals_default() {
        let cfg = Config::from_toml_str("").unwrap();
        let def = Config::default();
        assert_eq!(cfg.history, def.history);
        assert_eq!(cfg.remote, def.remote);
        assert_eq!(cfg.rules, def.rules);
        assert_eq!(cfg.history.mode, HistoryMode::Adaptive);
        assert!(cfg.remote.is_none());
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn full_example_parses() {
        let cfg = Config::from_toml_str(
            r#"
            [history]
            mode = { interval_ms = 5000 }

            [remote]
            host = "user@build-server"
            path = "/srv/projects/tomo"

            [[rules]]
            pattern = "target/"
            class = "ignored"

            [[rules]]
            pattern = "**/*.log"
            class = "ignored"

            [[rules]]
            pattern = "dist/**"
            class = "synced+unversioned"
            direction = "pull"

            [[rules]]
            pattern = "scripts/**"
            class = "synced+versioned"
            direction = "push"
            "#,
        )
        .unwrap();

        assert_eq!(
            cfg.history.mode,
            HistoryMode::Interval { interval_ms: 5000 }
        );
        let remote = cfg.remote.as_ref().unwrap();
        assert_eq!(remote.host, "user@build-server");
        assert_eq!(remote.path, "/srv/projects/tomo");
        assert_eq!(cfg.rules.len(), 4);
        assert_eq!(cfg.rules[0].direction, Direction::Both); // defaulted
        assert_eq!(cfg.rules[2].direction, Direction::Pull);
    }

    #[test]
    fn history_mode_named_variants() {
        for (text, expected) in [
            ("adaptive", HistoryMode::Adaptive),
            ("every-change", HistoryMode::EveryChange),
            ("off", HistoryMode::Off),
        ] {
            let doc = format!("[history]\nmode = \"{text}\"\n");
            let cfg = Config::from_toml_str(&doc).unwrap();
            assert_eq!(cfg.history.mode, expected);
        }
    }

    #[test]
    fn history_mode_interval_variant() {
        let cfg = Config::from_toml_str("[history]\nmode = { interval_ms = 250 }\n").unwrap();
        assert_eq!(cfg.history.mode, HistoryMode::Interval { interval_ms: 250 });
    }

    #[test]
    fn absent_history_section_defaults_to_adaptive() {
        let cfg = Config::from_toml_str("[remote]\nhost = \"h\"\npath = \"/p\"\n").unwrap();
        assert_eq!(cfg.history.mode, HistoryMode::Adaptive);
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        let err = Config::from_toml_str("this is = = not toml").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_history_mode_is_parse_error() {
        let err = Config::from_toml_str("[history]\nmode = \"sometimes\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn bad_glob_is_glob_error() {
        // An unterminated character class is an invalid glob.
        let err = Config::from_toml_str("[[rules]]\npattern = \"a[bc\"\nclass = \"ignored\"\n")
            .unwrap_err();
        match err {
            ConfigError::Glob { pattern, .. } => assert_eq!(pattern, "a[bc"),
            other => panic!("expected Glob error, got {other:?}"),
        }
    }

    // ---- classification / precedence -------------------------------------

    #[test]
    fn unmatched_path_gets_default_class() {
        let cfg = Config::default();
        let c = cfg.classify("src/main.rs");
        assert_eq!(c.class, PathClass::SyncedVersioned);
        assert_eq!(c.direction, Direction::Both);
    }

    #[test]
    fn last_matching_rule_wins() {
        let cfg = Config::from_toml_str(
            r#"
            [[rules]]
            pattern = "logs/**"
            class = "synced+versioned"

            [[rules]]
            pattern = "logs/**"
            class = "ignored"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.classify("logs/today.log").class, PathClass::Ignored);
    }

    #[test]
    fn later_narrower_rule_overrides_earlier_broad_rule() {
        let cfg = Config::from_toml_str(
            r#"
            [[rules]]
            pattern = "build/**"
            class = "ignored"

            [[rules]]
            pattern = "build/keep/**"
            class = "synced+versioned"
            direction = "push"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.classify("build/tmp/x.o").class, PathClass::Ignored);
        let keep = cfg.classify("build/keep/artifact.bin");
        assert_eq!(keep.class, PathClass::SyncedVersioned);
        assert_eq!(keep.direction, Direction::Push);
    }

    #[test]
    fn trailing_slash_pattern_is_auto_expanded() {
        let cfg = Config::from_toml_str("[[rules]]\npattern = \"target/\"\nclass = \"ignored\"\n")
            .unwrap();
        assert_eq!(cfg.classify("target/debug/app").class, PathClass::Ignored);
        assert_eq!(cfg.classify("target/a/b/c").class, PathClass::Ignored);
    }

    #[test]
    fn star_does_not_cross_slash_but_doublestar_does() {
        let cfg = Config::from_toml_str(
            r#"
            [[rules]]
            pattern = "*.log"
            class = "ignored"
            "#,
        )
        .unwrap();
        // top-level match
        assert_eq!(cfg.classify("server.log").class, PathClass::Ignored);
        // nested does NOT match a root-anchored single-star pattern
        assert_eq!(
            cfg.classify("sub/server.log").class,
            PathClass::SyncedVersioned
        );
    }

    #[test]
    fn direction_defaults_to_both() {
        let cfg =
            Config::from_toml_str("[[rules]]\npattern = \"a/**\"\nclass = \"ignored\"\n").unwrap();
        assert_eq!(cfg.rules[0].direction, Direction::Both);
    }

    // ---- built-in default editor-temp ignores ---------------------------

    #[test]
    fn default_ignores_classify_editor_temps_as_ignored() {
        let cfg = Config::default();
        // One representative path per built-in pattern, at the root and nested.
        for p in [
            "main.rs.swp",
            "src/main.rs.swp",
            "a.swx",
            "deep/dir/a.swx",
            ".main.rs.swp", // .*.sw? (dot-prefixed vim swap)
            "src/.main.rs.swo",
            "notes.txt~",
            "src/notes.txt~",
            ".#lockfile",
            "src/.#lockfile",
            "#autosave#",
            "src/#autosave#",
            "4913",
            "src/4913",
        ] {
            assert_eq!(
                cfg.classify(p).class,
                PathClass::Ignored,
                "expected {p} to be ignored by the built-in default rules"
            );
        }
        // A perfectly ordinary source file is untouched by the defaults.
        assert_eq!(
            cfg.classify("src/main.rs").class,
            PathClass::SyncedVersioned
        );
        // `.rules` stays the USER rule list (empty here) — defaults are internal.
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn user_rule_overrides_a_default_ignore() {
        // A user rule re-including *.swp must win over the built-in default
        // (last match wins; defaults sit earliest).
        let cfg = Config::from_toml_str(
            r#"
            [[rules]]
            pattern = "**/*.swp"
            class = "synced+versioned"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.classify("a.swp").class, PathClass::SyncedVersioned);
        assert_eq!(cfg.classify("src/a.swp").class, PathClass::SyncedVersioned);
        // A different editor-temp pattern the user did NOT override stays ignored.
        assert_eq!(cfg.classify("a.swx").class, PathClass::Ignored);
    }

    #[test]
    fn default_ignores_false_restores_old_behavior() {
        let cfg = Config::from_toml_str("[sync]\ndefault_ignores = false\n").unwrap();
        assert!(!cfg.sync.default_ignores);
        // With the defaults disabled, an editor temp is an ordinary synced file.
        assert_eq!(cfg.classify("a.swp").class, PathClass::SyncedVersioned);
        assert_eq!(cfg.classify("4913").class, PathClass::SyncedVersioned);
        assert_eq!(cfg.classify("notes.txt~").class, PathClass::SyncedVersioned);
    }

    #[test]
    fn sync_defaults_to_enabled_when_section_absent() {
        let cfg = Config::from_toml_str("").unwrap();
        assert!(cfg.sync.default_ignores);
        assert_eq!(cfg.classify("x.swp").class, PathClass::Ignored);
    }

    // ---- .tomo supremacy -------------------------------------------------

    #[test]
    fn tomo_dir_is_ignored_even_without_rules() {
        let cfg = Config::default();
        assert_eq!(cfg.classify(".tomo").class, PathClass::Ignored);
        assert_eq!(cfg.classify(".tomo/db/x").class, PathClass::Ignored);
    }

    #[test]
    fn user_rule_cannot_make_tomo_synced() {
        // A hostile rule trying to sync + version .tomo/** must have no effect.
        let cfg = Config::from_toml_str(
            r#"
            [[rules]]
            pattern = ".tomo/**"
            class = "synced+versioned"
            direction = "both"

            [[rules]]
            pattern = ".tomo"
            class = "synced+unversioned"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.classify(".tomo").class, PathClass::Ignored);
        assert_eq!(
            cfg.classify(".tomo/db/history.sqlite").class,
            PathClass::Ignored
        );
        assert_eq!(cfg.classify(".tomo/config.toml").class, PathClass::Ignored);
    }

    // ---- loading from disk ----------------------------------------------

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        // No .tomo/config.toml exists.
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.history.mode, HistoryMode::Adaptive);
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn load_reads_and_parses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let tomo = dir.path().join(TOMO_DIR);
        std::fs::create_dir_all(&tomo).unwrap();
        std::fs::write(
            tomo.join(CONFIG_FILE),
            "[history]\nmode = \"off\"\n\n[[rules]]\npattern = \"target/**\"\nclass = \"ignored\"\n",
        )
        .unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.history.mode, HistoryMode::Off);
        assert_eq!(cfg.classify("target/debug/app").class, PathClass::Ignored);
    }

    #[test]
    fn load_malformed_file_errors_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let tomo = dir.path().join(TOMO_DIR);
        std::fs::create_dir_all(&tomo).unwrap();
        std::fs::write(tomo.join(CONFIG_FILE), "not = = toml").unwrap();

        let err = Config::load(dir.path()).unwrap_err();
        match err {
            ConfigError::Parse { location, .. } => {
                assert!(location.unwrap().ends_with("config.toml"));
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn load_bad_glob_errors_with_path_and_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let tomo = dir.path().join(TOMO_DIR);
        std::fs::create_dir_all(&tomo).unwrap();
        std::fs::write(
            tomo.join(CONFIG_FILE),
            "[[rules]]\npattern = \"a[bc\"\nclass = \"ignored\"\n",
        )
        .unwrap();

        let err = Config::load(dir.path()).unwrap_err();
        match err {
            ConfigError::Glob {
                location, pattern, ..
            } => {
                assert_eq!(pattern, "a[bc");
                assert!(location.unwrap().ends_with("config.toml"));
            }
            other => panic!("expected Glob error, got {other:?}"),
        }
    }

    // ---- property tests --------------------------------------------------

    proptest! {
        /// No path whose first component is not exactly `.tomo` is ever
        /// treated as internal.
        #[test]
        fn non_tomo_first_component_is_never_internal(
            first in "[a-zA-Z0-9._-]{1,12}",
            rest in "(/[a-zA-Z0-9._-]{1,8}){0,4}",
        ) {
            prop_assume!(first != ".tomo");
            let path = format!("{first}{rest}");
            prop_assert!(!is_tomo_internal(&path));
        }

        /// `.tomo` supremacy holds for every descendant regardless of config:
        /// any path under `.tomo/` classifies as Ignored.
        #[test]
        fn tomo_descendants_always_ignored(
            rest in "(/[a-zA-Z0-9._-]{1,8}){1,5}",
        ) {
            // A config that tries to force .tomo to be synced+versioned.
            let cfg = Config::from_toml_str(
                "[[rules]]\npattern = \".tomo/**\"\nclass = \"synced+versioned\"\n",
            )
            .unwrap();
            let path = format!(".tomo{rest}");
            prop_assert_eq!(cfg.classify(&path).class, PathClass::Ignored);
        }
    }
}
