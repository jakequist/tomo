//! The local control channel (UX-V2 §2; docs/SPEC.md §13): a unix-domain socket
//! at `.tomo/state/ctl.sock` served by every session, exposing a versioned event
//! stream and a command channel to any local client (`tomo events`, the future
//! TUI, scripts). State stays inside `.tomo/` (invariant #2), and `.tomo/**` is
//! hardcoded-ignored so the socket is never watched or synced (invariant #1).
//!
//! - [`proto`] — the newline-delimited JSON schema (versioned, additive-only).
//! - [`broadcast`] — the bounded fan-out that keeps a slow subscriber from ever
//!   back-pressuring the sync loop.
//! - [`server`] (private) — the [`ControlServer`] accept loop and command
//!   handlers.
//!
//! The session taps the [`crate::report::Reporter`] with an [`EventSink`] so the
//! same call sites that print human lines also publish structured records; no
//! logic is duplicated.

pub mod broadcast;
pub mod proto;
mod server;

use std::sync::Arc;

use broadcast::Broadcaster;
use proto::{ConflictSide, Event};

pub use server::{CommandContext, ControlServer};

/// A cheap, cloneable handle the [`crate::report::Reporter`] holds to publish
/// events to every attached control-channel subscriber.
///
/// The default (`None`) sink makes every emit a no-op, so a session with no
/// control server — or code paths that run before the server is attached — pays
/// nothing. Publishing is non-blocking (the [`Broadcaster`] drops slow
/// subscribers), so tapping the reporter never affects sync latency.
#[derive(Clone, Default)]
pub struct EventSink(Option<Arc<Broadcaster>>);

impl EventSink {
    /// Bind the sink to a live broadcaster.
    #[must_use]
    pub fn new(broadcaster: Arc<Broadcaster>) -> Self {
        EventSink(Some(broadcaster))
    }

    /// Publish one event to all subscribers (no-op when the sink is unbound).
    pub fn emit(&self, event: &Event) {
        if let Some(broadcaster) = &self.0 {
            broadcaster.publish(&proto::to_line(event));
        }
    }

    /// Whether any client is currently subscribed to the event stream. The
    /// session uses this to skip heartbeat work entirely when nobody is
    /// watching, so an idle session with no observer stays fully idle.
    #[must_use]
    pub fn has_subscribers(&self) -> bool {
        self.0.as_ref().is_some_and(|b| b.subscriber_count() > 0)
    }
}

/// Map a "winner is local" boolean to the wire [`ConflictSide`].
#[must_use]
pub fn winner_side(winner_is_local: bool) -> ConflictSide {
    if winner_is_local {
        ConflictSide::Local
    } else {
        ConflictSide::Peer
    }
}
