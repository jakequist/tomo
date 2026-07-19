//! The pure sync-boundary decision: may a change for a given path cross, and in
//! which direction?
//!
//! Tomo's path classes and directions are enforced on **both** send and receive.
//! Enforcing only on send is not enough: a peer running an older binary (or
//! holding a stale index head from a pre-upgrade sync) can still ship us a path
//! our config now ignores — most sharply, a `.git` tree. Two independent git
//! repositories that each carry their own `.git` must stay fully isolated, so an
//! ignored-class path is refused at ingress, never applied, never absorbed into
//! the engine, and never recorded in history — exactly as it is never shipped.
//!
//! The decision is a pure function of `(class, direction, flow)` — the path only
//! enters through its [`tomo_config::Classification`] — so it is exhaustively
//! unit-tested here (`path × class × direction → Ship/Apply/Drop`).

use tomo_config::{Direction, PathClass};

/// The direction a change is trying to cross the sync boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    /// A change received from the peer, seeking to be applied locally.
    Inbound,
    /// A local change (or local index head) seeking to be shipped to the peer.
    Outbound,
}

/// What to do with a change at the boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Crossing {
    /// Ship this local change/head to the peer (outbound, allowed).
    Ship,
    /// Apply this inbound change locally (inbound, allowed).
    Apply,
    /// Block: never ship, never apply, never absorb into the engine, never
    /// version. Emitted for an ignored-class path in either direction, or a
    /// path whose direction forbids this flow (push-only inbound, pull-only
    /// outbound).
    Drop,
}

/// Decide a change's disposition purely from its class, direction, and flow.
///
/// - `Ignored` → always [`Crossing::Drop`] (both directions).
/// - Otherwise the [`Direction`] gates the flow: `Both` allows either way;
///   `Push` is local→remote only (ships out, refuses inbound); `Pull` is
///   remote→local only (applies inbound, refuses outbound).
///
/// # Examples
/// ```
/// use tomo_config::{Direction, PathClass};
/// # // (crate-internal function; illustrated via the table below)
/// ```
#[must_use]
pub fn decide(class: PathClass, direction: Direction, flow: Flow) -> Crossing {
    if class == PathClass::Ignored {
        return Crossing::Drop;
    }
    match flow {
        Flow::Outbound => match direction {
            Direction::Both | Direction::Push => Crossing::Ship,
            Direction::Pull => Crossing::Drop,
        },
        Flow::Inbound => match direction {
            Direction::Both | Direction::Pull => Crossing::Apply,
            Direction::Push => Crossing::Drop,
        },
    }
}

/// The top-level path component, used to deduplicate the "not synced" note so an
/// ignored tree yields ONE note (`.git`), not one per file.
#[must_use]
pub fn note_prefix(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_is_always_dropped() {
        for dir in [Direction::Both, Direction::Push, Direction::Pull] {
            for flow in [Flow::Inbound, Flow::Outbound] {
                assert_eq!(decide(PathClass::Ignored, dir, flow), Crossing::Drop);
            }
        }
    }

    #[test]
    fn both_direction_crosses_either_way() {
        assert_eq!(
            decide(PathClass::SyncedVersioned, Direction::Both, Flow::Outbound),
            Crossing::Ship
        );
        assert_eq!(
            decide(PathClass::SyncedVersioned, Direction::Both, Flow::Inbound),
            Crossing::Apply
        );
        // Unversioned but synced behaves the same for crossing.
        assert_eq!(
            decide(
                PathClass::SyncedUnversioned,
                Direction::Both,
                Flow::Outbound
            ),
            Crossing::Ship
        );
        assert_eq!(
            decide(PathClass::SyncedUnversioned, Direction::Both, Flow::Inbound),
            Crossing::Apply
        );
    }

    #[test]
    fn push_only_ships_but_refuses_inbound() {
        assert_eq!(
            decide(PathClass::SyncedVersioned, Direction::Push, Flow::Outbound),
            Crossing::Ship
        );
        assert_eq!(
            decide(PathClass::SyncedVersioned, Direction::Push, Flow::Inbound),
            Crossing::Drop
        );
    }

    #[test]
    fn pull_only_applies_but_refuses_outbound() {
        assert_eq!(
            decide(PathClass::SyncedVersioned, Direction::Pull, Flow::Inbound),
            Crossing::Apply
        );
        assert_eq!(
            decide(PathClass::SyncedVersioned, Direction::Pull, Flow::Outbound),
            Crossing::Drop
        );
    }

    #[test]
    fn note_prefix_groups_by_top_component() {
        assert_eq!(note_prefix(".git/config"), ".git");
        assert_eq!(note_prefix(".git"), ".git");
        assert_eq!(note_prefix("vendor/lib/.git/HEAD"), "vendor");
        assert_eq!(note_prefix("solo"), "solo");
    }
}
