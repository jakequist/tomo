//! The pure canonicalizer: raw platform events → coherent pending changes.
//!
//! This module is the testability backbone of `tomo-watch`. It performs **no
//! I/O**: it is a pure function of the raw-event stream, so every editor
//! save pattern (vim's write-temp-then-rename, VS Code's atomic replace,
//! truncate-then-write, plain writes, deletes, renames) can be exercised with
//! synthetic sequences and no real filesystem — exactly the discipline
//! `CLAUDE.md` mandates ("Testing the watcher against the real filesystem in
//! unit tests → simulate events").
//!
//! # What it does
//! 1. **Relativize + validate**: turn each absolute event path into a
//!    [`RelPath`] under the project root, dropping anything that escapes the
//!    root, is non-UTF-8, or is `.tomo/**` (invariant #1, enforced here at the
//!    lowest layer because [`RelPath::new`] cannot even represent a `.tomo`
//!    path).
//! 2. **Classify**: drop paths the [`Config`] marks [`PathClass::Ignored`].
//! 3. **Reduce** the five raw event kinds to two outcomes a re-stat can act on:
//!    [`PendingKind::Dirty`] ("this path may have new content — stat and hash
//!    it") and [`PendingKind::Gone`] ("this path is gone").
//! 4. **Dedupe** consecutive identical outcomes, so a truncate-then-write burst
//!    (`Modify, Modify, Modify`) and inotify's redundant rename summary collapse
//!    to one change.
//!
//! # What it deliberately does *not* do
//! It never *defers* a change to wait for a later event: invariant #3 forbids
//! trading sync latency for coalescing, so each distinct outcome is emitted the
//! instant its raw event arrives. Echo suppression is **not** here either — it
//! lives in the engine's journal (a change Tomo itself wrote is recognised by
//! its post-write signature downstream), keeping this layer a pure event
//! transform.

use std::path::{Component, Path, PathBuf};

use tomo_config::{Config, PathClass};
use tomo_engine::RelPath;

/// A platform-agnostic raw filesystem event.
///
/// The watcher adapter maps each `notify::Event` into zero or more of these
/// (see [`crate::watcher`]); the canonicalizer consumes them. Keeping the raw
/// vocabulary tiny is what lets the reduction logic stay pure and exhaustively
/// testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    /// The absolute path the event concerns.
    pub path: PathBuf,
    /// What the platform reported happening to it.
    pub kind: RawKind,
}

/// The kind of a [`RawEvent`], normalized across platforms.
///
/// Renames are split into their two half-events ([`RawKind::RenameFrom`] and
/// [`RawKind::RenameTo`]) because that is how inotify delivers them and because
/// treating each half independently is provably correct: the "from" path is
/// gone, the "to" path is dirty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    /// A file was created.
    Create,
    /// A file's content or metadata may have changed.
    Modify,
    /// A file was removed.
    Remove,
    /// The source half of a rename (the path was moved away).
    RenameFrom,
    /// The destination half of a rename (a path was moved into place).
    RenameTo,
}

/// A canonicalized, engine-relative change awaiting a filesystem re-stat.
///
/// It is intermediate: the [`crate::sig`] resolver turns each one into a
/// [`tomo_engine::LocalChange`] by hashing (for [`PendingKind::Dirty`]) or by
/// recording a removal (for [`PendingKind::Gone`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    /// The affected repo-relative path.
    pub rel: RelPath,
    /// What should happen to it.
    pub kind: PendingKind,
}

/// The two outcomes the canonicalizer reduces every raw event to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// The path may have new content: stat it, and if it exists, hash it.
    ///
    /// Chosen for creates, modifications, and rename destinations. It is also
    /// self-correcting for ambiguous events: if the re-stat finds nothing, the
    /// resolver downgrades it to a removal.
    Dirty,
    /// The path is gone.
    Gone,
}

/// Stateful reducer from [`RawEvent`]s to [`PendingChange`]s.
///
/// Holds the project root (for relativizing paths) and the [`Config`] (for
/// ignore classification), plus a one-slot memory of the last change it emitted
/// so it can suppress consecutive duplicates. Construct one with
/// [`Canonicalizer::new`] and drive it with [`Canonicalizer::ingest`].
///
/// ```
/// use std::path::PathBuf;
/// use tomo_config::Config;
/// use tomo_watch::{Canonicalizer, RawEvent, RawKind, PendingKind};
///
/// let mut canon = Canonicalizer::new(PathBuf::from("/proj"), Config::default());
///
/// // A plain save of /proj/src/main.rs becomes one Dirty change.
/// let out = canon.ingest(RawEvent {
///     path: PathBuf::from("/proj/src/main.rs"),
///     kind: RawKind::Modify,
/// });
/// assert_eq!(out.len(), 1);
/// assert_eq!(out[0].rel.as_str(), "src/main.rs");
/// assert_eq!(out[0].kind, PendingKind::Dirty);
///
/// // A path inside .tomo/ is dropped (invariant #1).
/// assert!(canon.ingest(RawEvent {
///     path: PathBuf::from("/proj/.tomo/db/x"),
///     kind: RawKind::Modify,
/// }).is_empty());
/// ```
#[derive(Debug)]
pub struct Canonicalizer {
    root: PathBuf,
    config: Config,
    last: Option<PendingChange>,
}

impl Canonicalizer {
    /// Create a canonicalizer for a project rooted at `root`.
    ///
    /// `root` is used purely as a string prefix to strip; it is never touched
    /// on disk (this type does no I/O). The watcher is responsible for handing
    /// in an already-canonicalized absolute root so the prefix match succeeds.
    pub fn new(root: PathBuf, config: Config) -> Self {
        Self {
            root,
            config,
            last: None,
        }
    }

    /// Reduce one raw event to zero or more canonical pending changes.
    ///
    /// Returns an empty vector when the event's path is filtered (escapes the
    /// root, is `.tomo/**`, is non-UTF-8, or is classified
    /// [`PathClass::Ignored`]) or when the outcome exactly repeats the previous
    /// one (consecutive-duplicate suppression). Otherwise returns exactly one
    /// change. It never returns more than one today, but the signature is a
    /// `Vec` so a future rename-pairing refinement can emit both a `Gone` and a
    /// `Dirty` without an API break.
    pub fn ingest(&mut self, raw: RawEvent) -> Vec<PendingChange> {
        // Destructure to consume `raw` (the by-value signature is intentional:
        // it lets the watcher hand ownership straight through without cloning).
        let RawEvent { path, kind } = raw;
        let Some(rel) = self.relativize(&path) else {
            return Vec::new();
        };
        let kind = match kind {
            RawKind::Create | RawKind::Modify | RawKind::RenameTo => PendingKind::Dirty,
            RawKind::Remove | RawKind::RenameFrom => PendingKind::Gone,
        };
        let change = PendingChange { rel, kind };
        // Consecutive-duplicate suppression: a truncate-then-write burst and
        // inotify's redundant rename summary both re-emit the same outcome for
        // the same path back-to-back. We emit distinct changes immediately (no
        // deferral — invariant #3) and only swallow exact repeats.
        if self.last.as_ref() == Some(&change) {
            return Vec::new();
        }
        self.last = Some(change.clone());
        vec![change]
    }

    /// Turn an absolute event path into a validated, non-ignored [`RelPath`],
    /// or `None` if it should be dropped.
    fn relativize(&self, path: &Path) -> Option<RelPath> {
        let rel = path.strip_prefix(&self.root).ok()?;
        // Build a forward-slash, repo-relative string from normal components
        // only. Anything exotic (a `..`, a root, non-UTF-8) means the path is
        // not a tracked file, so we drop it rather than guess.
        let mut parts = Vec::new();
        for comp in rel.components() {
            match comp {
                Component::Normal(os) => parts.push(os.to_str()?),
                _ => return None,
            }
        }
        if parts.is_empty() {
            return None;
        }
        let joined = parts.join("/");
        // `RelPath::new` is the lowest-layer guard for invariant #1: a `.tomo`
        // first component is unrepresentable and returns an error here.
        let rel = RelPath::new(&joined).ok()?;
        match self.config.classify(rel.as_str()).class {
            PathClass::Ignored => None,
            PathClass::SyncedVersioned | PathClass::SyncedUnversioned => Some(rel),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;

    fn canon() -> Canonicalizer {
        Canonicalizer::new(PathBuf::from("/proj"), Config::default())
    }

    fn abs(p: &str) -> PathBuf {
        PathBuf::from("/proj").join(p)
    }

    fn ev(p: &str, kind: RawKind) -> RawEvent {
        RawEvent { path: abs(p), kind }
    }

    /// Every kind maps to the expected outcome for a plain tracked file.
    #[test]
    fn kind_mapping() {
        for (kind, expected) in [
            (RawKind::Create, PendingKind::Dirty),
            (RawKind::Modify, PendingKind::Dirty),
            (RawKind::RenameTo, PendingKind::Dirty),
            (RawKind::Remove, PendingKind::Gone),
            (RawKind::RenameFrom, PendingKind::Gone),
        ] {
            let mut c = canon();
            let out = c.ingest(ev("a.txt", kind));
            assert_eq!(out.len(), 1, "{kind:?} should emit one change");
            assert_eq!(out[0].rel.as_str(), "a.txt");
            assert_eq!(out[0].kind, expected, "{kind:?}");
        }
    }

    /// Vim's classic save: write a dotted temp, then rename it onto the target.
    /// The target ends with a single Dirty; the (dotted, but not config-ignored)
    /// temp surfaces its own events, none of which is a Removed on the target.
    #[test]
    fn vim_write_temp_then_rename() {
        let mut c = canon();
        let mut out = Vec::new();
        out.extend(c.ingest(ev("src/.main.rs.swp", RawKind::Create)));
        out.extend(c.ingest(ev("src/.main.rs.swp", RawKind::Modify)));
        out.extend(c.ingest(ev("src/.main.rs.swp", RawKind::RenameFrom)));
        out.extend(c.ingest(ev("src/main.rs", RawKind::RenameTo)));

        // The final, and only, change touching the target is a single Dirty.
        let target: Vec<_> = out
            .iter()
            .filter(|c| c.rel.as_str() == "src/main.rs")
            .collect();
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].kind, PendingKind::Dirty);
        // The target is never marked Gone.
        assert!(!out
            .iter()
            .any(|c| c.rel.as_str() == "src/main.rs" && c.kind == PendingKind::Gone));
    }

    /// VS Code / atomic-replace where the temp is config-ignored: only the
    /// target's Dirty survives.
    #[test]
    fn atomic_save_with_ignored_temp() {
        let cfg = Config::from_toml_str("[[rules]]\npattern = \"**/*.tmp\"\nclass = \"ignored\"\n")
            .unwrap();
        let mut c = Canonicalizer::new(PathBuf::from("/proj"), cfg);
        let mut out = Vec::new();
        out.extend(c.ingest(ev("doc.txt.tmp", RawKind::Create)));
        out.extend(c.ingest(ev("doc.txt.tmp", RawKind::Modify)));
        out.extend(c.ingest(ev("doc.txt.tmp", RawKind::RenameFrom)));
        out.extend(c.ingest(ev("doc.txt", RawKind::RenameTo)));

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rel.as_str(), "doc.txt");
        assert_eq!(out[0].kind, PendingKind::Dirty);
    }

    /// Truncate-then-write: several consecutive Modifys collapse to one Dirty.
    #[test]
    fn dedupes_consecutive_dirty() {
        let mut c = canon();
        assert_eq!(c.ingest(ev("f", RawKind::Modify)).len(), 1);
        assert!(c.ingest(ev("f", RawKind::Modify)).is_empty());
        assert!(c.ingest(ev("f", RawKind::Create)).is_empty()); // also Dirty
    }

    /// A Dirty then Gone for the same path are distinct and both emitted — we
    /// never defer to coalesce them (invariant #3).
    #[test]
    fn dirty_then_gone_both_emitted() {
        let mut c = canon();
        assert_eq!(
            c.ingest(ev("f", RawKind::Modify))[0].kind,
            PendingKind::Dirty
        );
        assert_eq!(
            c.ingest(ev("f", RawKind::Remove))[0].kind,
            PendingKind::Gone
        );
    }

    /// An interleaving of two paths does not let one path's dedup swallow the
    /// other's identical outcome.
    #[test]
    fn dedup_is_only_consecutive() {
        let mut c = canon();
        assert_eq!(c.ingest(ev("a", RawKind::Modify)).len(), 1);
        assert_eq!(c.ingest(ev("b", RawKind::Modify)).len(), 1);
        // "a" again is not consecutive with the previous "a", so it emits.
        assert_eq!(c.ingest(ev("a", RawKind::Modify)).len(), 1);
    }

    /// A real in-tree rename yields Gone(source) + Dirty(dest).
    #[test]
    fn in_tree_rename() {
        let mut c = canon();
        let gone = c.ingest(ev("old/name.rs", RawKind::RenameFrom));
        let dirty = c.ingest(ev("new/name.rs", RawKind::RenameTo));
        assert_eq!(gone[0].rel.as_str(), "old/name.rs");
        assert_eq!(gone[0].kind, PendingKind::Gone);
        assert_eq!(dirty[0].rel.as_str(), "new/name.rs");
        assert_eq!(dirty[0].kind, PendingKind::Dirty);
    }

    /// `.tomo/**` is dropped at this lowest layer regardless of event kind
    /// (invariant #1).
    #[test]
    fn drops_tomo_internal() {
        let mut c = canon();
        for kind in [RawKind::Create, RawKind::Modify, RawKind::Remove] {
            assert!(c.ingest(ev(".tomo/db/history.sqlite", kind)).is_empty());
            assert!(c.ingest(ev(".tomo", kind)).is_empty());
        }
    }

    /// Config-ignored paths are dropped.
    #[test]
    fn drops_ignored() {
        let cfg = Config::from_toml_str("[[rules]]\npattern = \"target/\"\nclass = \"ignored\"\n")
            .unwrap();
        let mut c = Canonicalizer::new(PathBuf::from("/proj"), cfg);
        assert!(c.ingest(ev("target/debug/app", RawKind::Modify)).is_empty());
        assert_eq!(c.ingest(ev("src/lib.rs", RawKind::Modify)).len(), 1);
    }

    /// Paths outside the root, the root itself, and `..`-escaping paths are all
    /// dropped rather than misattributed.
    #[test]
    fn drops_out_of_tree_and_root() {
        let mut c = canon();
        assert!(c
            .ingest(RawEvent {
                path: PathBuf::from("/elsewhere/x"),
                kind: RawKind::Modify,
            })
            .is_empty());
        assert!(c
            .ingest(RawEvent {
                path: PathBuf::from("/proj"),
                kind: RawKind::Modify,
            })
            .is_empty());
    }
}
