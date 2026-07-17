//! The live watcher adapter: `notify` → [`RawEvent`] → canonicalize → resolve.
//!
//! This module is deliberately **thin**. All the interesting logic lives in the
//! pure [`crate::canon`] canonicalizer and the [`crate::sig`] resolver; here we
//! only translate `notify`'s event vocabulary into [`RawEvent`]s, plumb them
//! through, and forward the results on a channel. The one piece of real logic —
//! the `notify::Event` → [`RawEvent`] mapping — is itself a pure function
//! ([`map_event`]) and is unit-tested with constructed events. The live watcher
//! end-to-end is covered by `scenarios/`, never by unit tests against a real
//! filesystem (per `CLAUDE.md`).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use tomo_config::Config;
use tomo_engine::LocalChange;

use crate::canon::{Canonicalizer, RawEvent, RawKind};
use crate::error::WatchError;
use crate::sig;

/// What the watcher delivers to its consumer.
///
/// A stream of [`WatchSignal::Change`]s interrupted, on watcher overflow, by a
/// [`WatchSignal::NeedsRescan`] the caller answers with [`crate::scan_diff`].
/// Making the "rescan" a first-class signal (rather than a returned error) is
/// what lets the sync loop recover from dropped events without tearing down the
/// watch (`docs/SPEC.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSignal {
    /// A canonical local change ready for the engine.
    Change(LocalChange),
    /// The event stream lost integrity (overflow, or a backend error); the
    /// caller should run a full [`crate::scan_diff`] to reconcile.
    NeedsRescan,
}

/// A running filesystem watch. Dropping it stops watching cleanly.
///
/// Start one with [`Watcher::start`]. The struct owns the underlying `notify`
/// watcher; when it is dropped, `notify`'s own `Drop` unregisters every watch
/// and joins its worker thread, so no further signals are sent on the channel.
#[derive(Debug)]
pub struct Watcher {
    // Held solely to keep the backend (and its thread) alive; interaction
    // happens entirely through the channel handed to `start`.
    _inner: notify::RecommendedWatcher,
}

impl Watcher {
    /// Begin recursively watching `root`, sending [`WatchSignal`]s on `tx`.
    ///
    /// `root` is canonicalized to an absolute path up front so that every event
    /// path shares that prefix (the canonicalizer strips it to derive
    /// repo-relative paths). `config` is moved into the watcher's callback for
    /// ignore classification.
    ///
    /// # Errors
    /// [`WatchError::Io`] if `root` cannot be canonicalized;
    /// [`WatchError::Backend`] if the platform watcher cannot start or register
    /// the recursive watch.
    pub fn start(
        root: &Path,
        config: Config,
        tx: Sender<WatchSignal>,
    ) -> Result<Watcher, WatchError> {
        let root = root.canonicalize().map_err(|source| WatchError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let mut canon = Canonicalizer::new(root.clone(), config);
        let handler_root = root.clone();

        let handler = move |res: notify::Result<Event>| {
            // A backend error means we can no longer trust the stream; ask for
            // a reconciling rescan rather than dropping the event silently.
            let Ok(event) = res else {
                let _ = tx.send(WatchSignal::NeedsRescan);
                return;
            };
            if event.need_rescan() {
                let _ = tx.send(WatchSignal::NeedsRescan);
                return;
            }
            // A directory APPEARING is a correctness hazard, not a no-op: with
            // inotify there is a window between the directory's creation (or
            // rename-in) and `notify` establishing the recursive watch on it.
            // Files landing inside during that window emit NO events and would
            // be silently lost (scenario 02 caught this as a ~50% flake). A
            // reconciling rescan closes the window. Rename/Any kinds need a
            // stat to know a directory arrived; Create(Folder) is explicit.
            let dir_appeared = matches!(event.kind, EventKind::Create(CreateKind::Folder))
                || (matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Name(
                            RenameMode::To | RenameMode::Both | RenameMode::Any
                        ))
                        | EventKind::Any
                ) && event.paths.iter().any(|p| p.is_dir()));
            if dir_appeared {
                let _ = tx.send(WatchSignal::NeedsRescan);
            }
            for raw in map_event(&event) {
                for pending in canon.ingest(raw) {
                    match sig::resolve(&handler_root, &pending) {
                        Ok(change) => {
                            let _ = tx.send(WatchSignal::Change(change));
                        }
                        // A transient read failure: fall back to a rescan so the
                        // change is not silently lost.
                        Err(_) => {
                            let _ = tx.send(WatchSignal::NeedsRescan);
                        }
                    }
                }
            }
        };

        let mut inner = notify::recommended_watcher(handler)?;
        inner.watch(&root, RecursiveMode::Recursive)?;
        Ok(Watcher { _inner: inner })
    }
}

/// Map one `notify::Event` into zero or more [`RawEvent`]s.
///
/// This is the entire platform-specific surface of the crate, isolated as a
/// pure function so it can be unit-tested with hand-built events. Overflow /
/// rescan events are handled by the caller (via `Event::need_rescan`) *before*
/// this is called, so they never appear here.
///
/// # Mapping
/// | `notify` `EventKind`            | `RawEvent`(s)                         |
/// |---------------------------------|---------------------------------------|
/// | `Create(Folder)`                | *(none here — but the live handler emits `NeedsRescan`: see below)* |
/// | `Create(_)`                     | `Create` per path                     |
/// | `Modify(Data(_))`               | `Modify` per path                     |
/// | `Modify(Metadata(_))`           | *(none — content is unchanged)*       |
/// | `Modify(Name(From))`            | `RenameFrom` per path                 |
/// | `Modify(Name(To))`              | `RenameTo` per path                   |
/// | `Modify(Name(Both))`            | `RenameFrom(first)` + `RenameTo(last)`|
/// | `Modify(Name(Any\|Other))`      | `Modify` per path (re-stat)           |
/// | `Modify(Any\|Other)`            | `Modify` per path (re-stat)           |
/// | `Remove(Folder)`                | *(none — per-file removes cover it)*  |
/// | `Remove(_)`                     | `Remove` per path                     |
/// | `Access(_)` / `Other`           | *(none)*                              |
/// | `Any` (imprecise backends)      | `Modify` per path (re-stat)           |
///
/// Ambiguous kinds map to `Modify` (a "re-stat this" `Dirty`) because the
/// resolver self-corrects to a removal if the path turns out to be gone — so an
/// imprecise, coalesced macOS `FSEvents` notification still does the right thing.
/// Pure metadata changes (permissions, atime from a mere read) are dropped to
/// avoid re-hashing on every file access; a real content write always also
/// carries a `Data` (or create/rename) event.
///
/// NOTE: this pure mapping intentionally yields nothing for a directory
/// creation, but [`Watcher::start`]'s live handler additionally emits
/// [`WatchSignal::NeedsRescan`] whenever a directory appears (create or
/// rename-in), because inotify cannot deliver events for files created inside
/// a new directory before the recursive watch is established on it.
#[must_use]
pub fn map_event(event: &Event) -> Vec<RawEvent> {
    let each = |kind: RawKind| -> Vec<RawEvent> {
        event
            .paths
            .iter()
            .map(|p| RawEvent {
                path: p.clone(),
                kind,
            })
            .collect()
    };

    match event.kind {
        // No file-content change: directory create/remove (their children emit
        // their own events), pure metadata (permissions, atime), access, and
        // meta "Other" events.
        EventKind::Create(CreateKind::Folder)
        | EventKind::Remove(RemoveKind::Folder)
        | EventKind::Modify(ModifyKind::Metadata(_))
        | EventKind::Access(_)
        | EventKind::Other => Vec::new(),

        EventKind::Create(_) => each(RawKind::Create),
        EventKind::Remove(_) => each(RawKind::Remove),

        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => each(RawKind::RenameFrom),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => each(RawKind::RenameTo),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => rename_both(&event.paths),

        // Content writes, ambiguous renames, other/imprecise modifications, and
        // the imprecise `Any` catch-all (coalesced FSEvents): re-stat the path.
        EventKind::Modify(_) | EventKind::Any => each(RawKind::Modify),
    }
}

/// Expand a correlated rename (`Name(Both)`) carrying `[from, to]` into a
/// `RenameFrom(from)` + `RenameTo(to)` pair. Degenerate shapes (fewer than two
/// paths) fall back to treating each path as a destination.
fn rename_both(paths: &[PathBuf]) -> Vec<RawEvent> {
    match (paths.first(), paths.last()) {
        (Some(from), Some(to)) if paths.len() >= 2 => vec![
            RawEvent {
                path: from.clone(),
                kind: RawKind::RenameFrom,
            },
            RawEvent {
                path: to.clone(),
                kind: RawKind::RenameTo,
            },
        ],
        _ => paths
            .iter()
            .map(|p| RawEvent {
                path: p.clone(),
                kind: RawKind::RenameTo,
            })
            .collect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;
    use notify::event::{DataChange, MetadataKind};

    fn evk(kind: EventKind, paths: &[&str]) -> Event {
        let mut e = Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    #[test]
    fn create_file_maps_to_create() {
        let out = map_event(&evk(EventKind::Create(CreateKind::File), &["/r/a"]));
        assert_eq!(
            out,
            vec![RawEvent {
                path: PathBuf::from("/r/a"),
                kind: RawKind::Create
            }]
        );
    }

    #[test]
    fn create_folder_is_dropped() {
        assert!(map_event(&evk(EventKind::Create(CreateKind::Folder), &["/r/d"])).is_empty());
    }

    #[test]
    fn data_modify_maps_to_modify() {
        let out = map_event(&evk(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            &["/r/a"],
        ));
        assert_eq!(out[0].kind, RawKind::Modify);
    }

    #[test]
    fn metadata_modify_is_dropped() {
        let out = map_event(&evk(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            &["/r/a"],
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn rename_from_and_to() {
        let from = map_event(&evk(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/r/a"],
        ));
        assert_eq!(from[0].kind, RawKind::RenameFrom);
        let to = map_event(&evk(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            &["/r/b"],
        ));
        assert_eq!(to[0].kind, RawKind::RenameTo);
    }

    #[test]
    fn rename_both_expands_to_pair() {
        let out = map_event(&evk(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &["/r/a", "/r/b"],
        ));
        assert_eq!(
            out,
            vec![
                RawEvent {
                    path: PathBuf::from("/r/a"),
                    kind: RawKind::RenameFrom
                },
                RawEvent {
                    path: PathBuf::from("/r/b"),
                    kind: RawKind::RenameTo
                },
            ]
        );
    }

    #[test]
    fn remove_file_maps_to_remove() {
        let out = map_event(&evk(EventKind::Remove(RemoveKind::File), &["/r/a"]));
        assert_eq!(out[0].kind, RawKind::Remove);
    }

    #[test]
    fn remove_folder_is_dropped() {
        assert!(map_event(&evk(EventKind::Remove(RemoveKind::Folder), &["/r/d"])).is_empty());
    }

    #[test]
    fn access_is_dropped() {
        use notify::event::AccessKind;
        assert!(map_event(&evk(EventKind::Access(AccessKind::Read), &["/r/a"])).is_empty());
    }

    #[test]
    fn imprecise_any_maps_to_modify() {
        let out = map_event(&evk(EventKind::Any, &["/r/a"]));
        assert_eq!(out[0].kind, RawKind::Modify);
    }

    #[test]
    fn multi_path_create_maps_each() {
        let out = map_event(&evk(EventKind::Create(CreateKind::File), &["/r/a", "/r/b"]));
        assert_eq!(out.len(), 2);
    }
}
