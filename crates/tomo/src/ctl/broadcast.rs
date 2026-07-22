//! A bounded fan-out broadcaster for the control channel's event stream.
//!
//! One [`Broadcaster`] is shared by the session (the producer, via the
//! [`crate::report::Reporter`] tap) and every events-mode socket connection (a
//! consumer). Publishing is **non-blocking**: each subscriber owns a bounded
//! queue ([`SUBSCRIBER_CAP`] lines), and a subscriber that falls behind is
//! dropped rather than allowed to back-pressure the producer — sync latency is
//! never sacrificed for an observer (CLAUDE.md invariant #3). A dropped
//! subscriber's [`Subscription::lagged`] flag is set so its consumer can emit a
//! final best-effort `{"event":"lagged"}` line before closing.
//!
//! The logic is deliberately I/O-free and unit-tested (fill / lag / disconnect)
//! so the memory bound and drop policy are verified without sockets or threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

/// Per-subscriber queue depth. A subscriber this far behind is disconnected to
/// keep the producer non-blocking and memory bounded.
pub const SUBSCRIBER_CAP: usize = 1024;

/// The shared fan-out hub. Cheap to clone via `Arc`; every method takes `&self`.
#[derive(Debug)]
pub struct Broadcaster {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    subs: Vec<Sub>,
}

#[derive(Debug)]
struct Sub {
    tx: SyncSender<String>,
    lagged: Arc<AtomicBool>,
}

/// A consumer's handle to the stream: the receiving queue plus the shared
/// `lagged` flag the producer sets when it drops this subscriber for lagging.
#[derive(Debug)]
pub struct Subscription {
    rx: Receiver<String>,
    lagged: Arc<AtomicBool>,
}

impl Broadcaster {
    /// Create an empty broadcaster.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Broadcaster {
            inner: Mutex::new(Inner { subs: Vec::new() }),
        })
    }

    /// Register a new subscriber and return its [`Subscription`].
    pub fn subscribe(&self) -> Subscription {
        let (tx, rx) = sync_channel(SUBSCRIBER_CAP);
        let lagged = Arc::new(AtomicBool::new(false));
        self.lock().subs.push(Sub {
            tx,
            lagged: Arc::clone(&lagged),
        });
        Subscription { rx, lagged }
    }

    /// Publish one line to every subscriber, non-blocking.
    ///
    /// A subscriber whose queue is full is flagged lagged and dropped (its
    /// channel closes, so its consumer stops after draining what it has). A
    /// subscriber whose consumer already went away is simply removed. Both keep
    /// memory bounded and the producer un-blocked.
    pub fn publish(&self, line: &str) {
        let mut inner = self.lock();
        inner
            .subs
            .retain(|sub| match sub.tx.try_send(line.to_owned()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    sub.lagged.store(true, Ordering::SeqCst);
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
    }

    /// The number of currently registered subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.lock().subs.len()
    }

    /// Drop every subscriber (used at session shutdown): closes each queue so
    /// the blocked consumer threads observe end-of-stream and exit.
    pub fn close_all(&self) {
        self.lock().subs.clear();
    }

    /// Lock the inner state, recovering a poisoned mutex rather than panicking
    /// (a producer/consumer that paniced must not wedge the whole channel).
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Subscription {
    /// Block for the next line, or `None` when the broadcaster has closed this
    /// subscription (channel dropped).
    #[must_use]
    pub fn recv(&self) -> Option<String> {
        self.rx.recv().ok()
    }

    /// Whether this subscription was dropped for falling behind (its consumer
    /// should emit a final `lagged` line before closing).
    #[must_use]
    pub fn lagged(&self) -> bool {
        self.lagged.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn publish_delivers_to_a_subscriber() {
        let b = Broadcaster::new();
        let sub = b.subscribe();
        b.publish("hello");
        assert_eq!(sub.recv().as_deref(), Some("hello"));
        assert!(!sub.lagged());
    }

    #[test]
    fn each_subscriber_gets_its_own_copy() {
        let b = Broadcaster::new();
        let a = b.subscribe();
        let c = b.subscribe();
        assert_eq!(b.subscriber_count(), 2);
        b.publish("x");
        assert_eq!(a.recv().as_deref(), Some("x"));
        assert_eq!(c.recv().as_deref(), Some("x"));
    }

    #[test]
    fn a_lagging_subscriber_is_dropped_and_flagged() {
        let b = Broadcaster::new();
        let sub = b.subscribe();
        // Fill the queue exactly to capacity: still buffered, still subscribed.
        for _ in 0..SUBSCRIBER_CAP {
            b.publish("line");
        }
        assert_eq!(b.subscriber_count(), 1);
        // One more overflows the bounded queue → the subscriber is dropped.
        b.publish("overflow");
        assert_eq!(b.subscriber_count(), 0);
        // The consumer can still drain the buffered lines, then sees the close
        // and its lagged flag.
        let mut drained = 0;
        while let Some(_line) = sub.recv() {
            drained += 1;
        }
        assert_eq!(drained, SUBSCRIBER_CAP);
        assert!(
            sub.lagged(),
            "an overflowed subscriber must be flagged lagged"
        );
    }

    #[test]
    fn a_disconnected_consumer_is_removed_on_next_publish() {
        let b = Broadcaster::new();
        let sub = b.subscribe();
        assert_eq!(b.subscriber_count(), 1);
        drop(sub); // consumer went away
        b.publish("noone-home");
        assert_eq!(b.subscriber_count(), 0);
    }

    #[test]
    fn close_all_ends_every_subscription() {
        let b = Broadcaster::new();
        let sub = b.subscribe();
        b.publish("one");
        b.close_all();
        // The buffered line still drains, then the closed channel ends the loop
        // with no lag flag (a clean shutdown, not a lag).
        assert_eq!(sub.recv().as_deref(), Some("one"));
        assert_eq!(sub.recv(), None);
        assert!(!sub.lagged());
    }
}
