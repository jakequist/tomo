//! Adaptive history pressure controller (docs/SPEC.md §6.2).
//!
//! History capture is a *congestion controller*: under light load every change
//! becomes its own version immediately (purity); under a storm, per-file flush
//! intervals escalate along a ladder (`0ms → 250ms → 1s → 5s`), coalescing
//! bursts into checkpoints, then decay back toward `0` as pressure subsides.
//!
//! # What this controls, and what it does not (invariant #3)
//! This governs **history capture only**. It never sees, delays, or reorders
//! the live sync path — the transport ships the latest bytes immediately
//! regardless of what the controller decides here. The controller only answers
//! "should this version be recorded now, or coalesced into a later checkpoint?"
//!
//! # Purity and deterministic time (invariants #6, #7)
//! Like the rest of `tomo-engine`, this module performs no I/O and reads no
//! wall clock. Every method that needs "now" takes an explicit `now_ms`
//! parameter, which the caller is required to advance **monotonically**
//! (non-decreasing across calls). All escalation, decay, and due-time decisions
//! are pure functions of the fed timestamps, so storms are reproducible in
//! tests with simulated time — never `std::time`, never `sleep`.
//!
//! # The no-lost-final-write guarantee (invariant #4)
//! A staged capture is only ever **replaced by a newer capture for the same
//! path** (dropping the older *intermediate* — that is the whole point of
//! debouncing) or **returned by [`PressureController::poll_due`]**. There is no
//! code path that silently discards one, with a single loudly-documented
//! exception: [`HistoryMode::Off`], which disables history entirely and
//! swallows every capture ([`CaptureDecision::Dropped`]).
//!
//! # Bounded staleness
//! When a capture is staged, its deadline is `staged_at + ladder[rung]`. A
//! *replacement* keeps the slot's **original** deadline —
//! `due = min(previous_due, now + ladder[rung])` — rather than pushing it out.
//! Consequently a checkpoint for a continuously-edited file flushes at least
//! every `ladder[rung]` ms after the burst's first unflushed change (and thus
//! at most every `ladder.max()` ms), even while edits keep arriving; the
//! content flushed is always the newest seen. Extending the deadline on every
//! edit (`max(previous_due, …)`) would let a hot file starve forever — so we
//! never do that.

use std::collections::{BTreeMap, VecDeque};

use crate::clock::VectorClock;
use crate::index::EntryState;
use crate::path::RelPath;

/// Trailing window, in milliseconds, over which the event-arrival rate is
/// estimated. A 1000 ms window means the note count in the window *is* the
/// events-per-second estimate, which keeps [`PressureConfig::escalate_events_per_sec`]
/// intuitive.
const RATE_WINDOW_MS: u64 = 1000;

/// History-write-queue depth above which the controller climbs a rung
/// regardless of arrival rate (a full downstream queue is back-pressure).
const QUEUE_HIGH_WATER: u64 = 64;

/// Tuning for the adaptive controller. Defaults come from docs/SPEC.md §6.2.
#[derive(Debug, Clone, PartialEq)]
pub struct PressureConfig {
    /// Per-file flush intervals in milliseconds, ascending. Rung `0` (the
    /// default `0`) means "flush immediately"; higher rungs coalesce bursts.
    /// Must be non-empty; [`PressureController::new`] falls back to `[0]`.
    pub ladder_ms: Vec<u64>,
    /// Arrival-rate threshold (events per second, measured over the trailing
    /// [`RATE_WINDOW_MS`] window) above which the controller climbs one rung.
    pub escalate_events_per_sec: f64,
    /// Idle time in milliseconds with no `note` call after which the controller
    /// drops one rung. Evaluated lazily on any timestamped call — no timers.
    pub decay_idle_ms: u64,
    /// Total staged-bytes threshold above which the controller climbs one rung
    /// (chunking pressure), independent of arrival rate.
    pub max_staged_bytes: u64,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            ladder_ms: vec![0, 250, 1000, 5000],
            escalate_events_per_sec: 20.0,
            decay_idle_ms: 2000,
            max_staged_bytes: 8 * 1024 * 1024,
        }
    }
}

/// History-capture policy, mirroring `tomo-config`'s `history.mode`
/// (`adaptive | every-change | interval(_) | off`). The CLI layer maps the
/// parsed config value onto this; the engine only interprets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryMode {
    /// The adaptive ladder: purity under light load, debounce under pressure.
    Adaptive,
    /// Every change becomes its own version immediately; never staged.
    EveryChange,
    /// History disabled: every capture is swallowed
    /// ([`CaptureDecision::Dropped`]). Documented loudly because it is the sole
    /// exception to the no-lost-final-write guarantee.
    Off,
    /// A fixed flush interval in milliseconds, with no escalation or decay.
    /// An interval of `0` behaves like [`HistoryMode::EveryChange`].
    IntervalMs(u64),
}

/// The immutable payload of a note: the version to (maybe) record and the hints
/// the controller needs. The controller stamps `staged_at`/`due_at` itself, so
/// those are not part of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInput {
    /// The state (present-with-content or tombstone) to record.
    pub state: EntryState,
    /// The vector clock of this version — the only ordering authority.
    pub version: VectorClock,
    /// Whether this change originated locally (`true`) or from the peer
    /// (`false`). Carried through for history provenance; not used for timing.
    pub origin_is_local: bool,
    /// Approximate byte size awaiting chunking, feeding the staged-bytes
    /// pressure signal. `0` is a valid "unknown/irrelevant" hint.
    pub size_hint: u64,
}

/// A capture parked in the per-path staging buffer awaiting its flush deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedCapture {
    /// The state to record when this capture flushes.
    pub state: EntryState,
    /// The vector clock of the staged version.
    pub version: VectorClock,
    /// Whether the staged change originated locally.
    pub origin_is_local: bool,
    /// Timestamp (ms) at which the *current* content entered the slot. Updated
    /// on each replacement; the flush deadline is not.
    pub staged_at_ms: u64,
    /// Timestamp (ms) at or after which this capture is due to flush. Set from
    /// the burst's *first* unflushed change and only ever shortened, never
    /// extended (see the bounded-staleness note on the module).
    pub due_at_ms: u64,
    /// The `size_hint` of the staged content, contributing to staged bytes.
    pub size_hint: u64,
}

/// What [`PressureController::note`] decided for a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureDecision {
    /// Record this version now — nothing was staged.
    Immediate,
    /// The version was staged (or replaced an older staged one); it will be
    /// returned by [`PressureController::poll_due`] at or after `due_at_ms`.
    Deferred {
        /// The flush deadline of the staged slot.
        due_at_ms: u64,
    },
    /// History is [`HistoryMode::Off`]: the capture was swallowed — never
    /// staged, never recorded, never returned by `poll_due`.
    Dropped,
}

/// The adaptive history pressure controller: a pure state machine over fed
/// change notes and deterministic timestamps.
///
/// ```
/// use tomo_engine::{
///     CaptureDecision, CaptureInput, ContentHash, ContentSig, EntryState,
///     HistoryMode, PressureConfig, PressureController, RelPath, VectorClock,
/// };
///
/// let mut pc = PressureController::new(HistoryMode::Adaptive, PressureConfig::default());
/// let path = RelPath::new("src/main.rs").unwrap();
/// let cap = CaptureInput {
///     state: EntryState::Present(ContentSig { hash: ContentHash([1; 32]), size: 3 }),
///     version: VectorClock::new(),
///     origin_is_local: true,
///     size_hint: 3,
/// };
///
/// // A lone change under no load is versioned immediately.
/// assert_eq!(pc.note(path, cap, 0), CaptureDecision::Immediate);
/// assert_eq!(pc.rung(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct PressureController {
    mode: HistoryMode,
    config: PressureConfig,
    /// Current ladder rung; always a valid index into `config.ladder_ms`.
    rung: usize,
    /// Per-path staging buffer, ordered by path for deterministic iteration.
    staging: BTreeMap<RelPath, StagedCapture>,
    /// Running total of staged `size_hint`s (the staged-bytes signal).
    staged_bytes: u64,
    /// Timestamps of recent notes, for the sliding-window rate estimate.
    /// Pruned to the trailing [`RATE_WINDOW_MS`] window on each use.
    window: VecDeque<u64>,
    /// The point in time from which decay is measured. Reset to `now` on every
    /// `note` (a note is activity); advanced by whole `decay_idle_ms` steps as
    /// idle time is consumed, so decay is exact and lazy without timers.
    decay_anchor_ms: u64,
}

impl PressureController {
    /// Create a controller in `mode` with `config`.
    ///
    /// An empty `ladder_ms` is replaced by `[0]` so the rung index is always
    /// valid; every other field is used as given.
    pub fn new(mode: HistoryMode, mut config: PressureConfig) -> Self {
        if config.ladder_ms.is_empty() {
            config.ladder_ms = vec![0];
        }
        Self {
            mode,
            config,
            rung: 0,
            staging: BTreeMap::new(),
            staged_bytes: 0,
            window: VecDeque::new(),
            decay_anchor_ms: 0,
        }
    }

    /// Record that a `RecordVersion` action occurred for `path`: stage or
    /// replace this path's pending capture, or decide it should be recorded
    /// immediately.
    ///
    /// Replacing a pending capture **drops the older intermediate** version —
    /// the newest state always wins the slot and the slot keeps its original
    /// deadline (`min(previous_due, now + ladder[rung])`), giving bounded
    /// staleness. `now_ms` must be monotonically non-decreasing across calls.
    pub fn note(&mut self, path: RelPath, capture: CaptureInput, now_ms: u64) -> CaptureDecision {
        match self.mode {
            HistoryMode::Off => CaptureDecision::Dropped,
            HistoryMode::EveryChange => CaptureDecision::Immediate,
            HistoryMode::IntervalMs(interval) => self.decide(path, capture, now_ms, interval),
            HistoryMode::Adaptive => {
                self.decay(now_ms);
                self.window.push_back(now_ms);
                // A note escalates only on its own signals (arrival rate and
                // staged bytes) — never on a stale, previously-fed queue depth.
                self.escalate(now_ms, false);
                // A note is activity: restart the decay countdown from here.
                self.decay_anchor_ms = now_ms;
                let interval = self.config.ladder_ms[self.rung];
                self.decide(path, capture, now_ms, interval)
            }
        }
    }

    /// Stage `capture` (or record it immediately) for `path` at `interval`.
    ///
    /// Immediate only when the interval is `0` **and** nothing is already
    /// staged for the path — an interval-`0` note against a still-pending slot
    /// replaces the slot with `due = now`, so the pending capture is never
    /// discarded, only superseded and made immediately due.
    fn decide(
        &mut self,
        path: RelPath,
        capture: CaptureInput,
        now_ms: u64,
        interval: u64,
    ) -> CaptureDecision {
        if interval == 0 && !self.staging.contains_key(&path) {
            return CaptureDecision::Immediate;
        }
        let ladder_due = now_ms.saturating_add(interval);
        let (due_at_ms, prev_bytes) = match self.staging.get(&path) {
            Some(existing) => (existing.due_at_ms.min(ladder_due), existing.size_hint),
            None => (ladder_due, 0),
        };
        self.staged_bytes = self
            .staged_bytes
            .saturating_sub(prev_bytes)
            .saturating_add(capture.size_hint);
        self.staging.insert(
            path,
            StagedCapture {
                state: capture.state,
                version: capture.version,
                origin_is_local: capture.origin_is_local,
                staged_at_ms: now_ms,
                due_at_ms,
                size_hint: capture.size_hint,
            },
        );
        CaptureDecision::Deferred { due_at_ms }
    }

    /// Feed adapter-observed pressure signals (currently the history-write
    /// queue depth). Applies any pending decay and may climb a rung when the
    /// fed depth is over the high-water mark. The queue depth is a *momentary*
    /// signal — it escalates at most one rung here and is not remembered across
    /// later `note` calls. Staged bytes are tracked internally and need not be
    /// fed. `now_ms` must be monotonically non-decreasing across calls.
    pub fn signals(&mut self, history_queue_depth: u64, now_ms: u64) {
        if self.mode == HistoryMode::Adaptive {
            self.decay(now_ms);
            self.escalate(now_ms, history_queue_depth > QUEUE_HIGH_WATER);
        }
    }

    /// Remove and return every staged capture whose deadline is `<= now_ms`, in
    /// ascending `(due_at_ms, path)` order. Applies any pending decay first.
    /// `now_ms` must be monotonically non-decreasing across calls.
    pub fn poll_due(&mut self, now_ms: u64) -> Vec<(RelPath, StagedCapture)> {
        if self.mode == HistoryMode::Adaptive {
            self.decay(now_ms);
        }
        let due_paths: Vec<RelPath> = self
            .staging
            .iter()
            .filter(|(_, cap)| cap.due_at_ms <= now_ms)
            .map(|(path, _)| path.clone())
            .collect();
        let mut out: Vec<(RelPath, StagedCapture)> = Vec::with_capacity(due_paths.len());
        for path in due_paths {
            if let Some(cap) = self.staging.remove(&path) {
                self.staged_bytes = self.staged_bytes.saturating_sub(cap.size_hint);
                out.push((path, cap));
            }
        }
        out.sort_by(|a, b| {
            a.1.due_at_ms
                .cmp(&b.1.due_at_ms)
                .then_with(|| a.0.cmp(&b.0))
        });
        out
    }

    /// The earliest deadline among staged captures, for the caller's sleep
    /// computation. `None` when nothing is staged.
    pub fn next_due_ms(&self) -> Option<u64> {
        self.staging.values().map(|cap| cap.due_at_ms).min()
    }

    /// The current ladder rung (`0` == flush immediately).
    pub fn rung(&self) -> usize {
        self.rung
    }

    /// The number of captures currently staged.
    pub fn staged_len(&self) -> usize {
        self.staging.len()
    }

    /// Drop rungs for idle time elapsed since the decay anchor.
    ///
    /// One rung per whole `decay_idle_ms` of idleness; the anchor advances by
    /// exactly the consumed idle time so evaluation is lazy and idempotent for
    /// a given `now`. No-op unless the mode is adaptive.
    fn decay(&mut self, now_ms: u64) {
        if self.config.decay_idle_ms == 0 || now_ms <= self.decay_anchor_ms {
            return;
        }
        let idle = now_ms - self.decay_anchor_ms;
        let steps = idle / self.config.decay_idle_ms;
        if steps == 0 {
            return;
        }
        let drop = usize::try_from(steps).unwrap_or(usize::MAX);
        self.rung = self.rung.saturating_sub(drop);
        self.decay_anchor_ms = self
            .decay_anchor_ms
            .saturating_add(steps.saturating_mul(self.config.decay_idle_ms));
    }

    /// Climb one rung if any pressure signal is over threshold and we are not
    /// already at the top rung. `queue_high` is the caller-evaluated back-
    /// pressure signal (true only when [`PressureController::signals`] fed a
    /// depth over the high-water mark). No-op unless the mode is adaptive.
    fn escalate(&mut self, now_ms: u64, queue_high: bool) {
        if self.rung + 1 >= self.config.ladder_ms.len() {
            return;
        }
        let rate = self.observed_rate(now_ms);
        if rate > self.config.escalate_events_per_sec
            || self.staged_bytes > self.config.max_staged_bytes
            || queue_high
        {
            self.rung += 1;
        }
    }

    /// Prune the rate window to the trailing [`RATE_WINDOW_MS`] and return the
    /// events-per-second estimate (note count in the window; the window is
    /// 1000 ms, so the count is already a per-second rate).
    fn observed_rate(&mut self, now_ms: u64) -> f64 {
        let cutoff = now_ms.saturating_sub(RATE_WINDOW_MS);
        while let Some(&front) = self.window.front() {
            if front < cutoff {
                self.window.pop_front();
            } else {
                break;
            }
        }
        // The window holds at most a few thousand entries; usize→f64 is exact
        // far past that (2^53), so no meaningful precision is lost.
        #[allow(clippy::cast_precision_loss)]
        let rate = self.window.len() as f64;
        rate
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // fine in tests
mod tests {
    use super::*;
    use crate::clock::ReplicaId;
    use crate::index::{ContentHash, ContentSig};
    use proptest::prelude::*;

    // ---- helpers ----------------------------------------------------------

    fn path(name: &str) -> RelPath {
        RelPath::new(name).expect("valid test path")
    }

    /// A clock ticked `n` times on replica 0 — strictly increasing in `n`, so
    /// successive notes for a path have a well-defined "newest".
    fn clock(n: u64) -> VectorClock {
        let mut c = VectorClock::new();
        for _ in 0..n {
            c.tick(ReplicaId(0));
        }
        c
    }

    fn present(byte: u8, size: u64) -> EntryState {
        EntryState::Present(ContentSig {
            hash: ContentHash([byte; 32]),
            size,
        })
    }

    /// A capture whose version and content are derived from a counter, so the
    /// counter totally orders a path's notes.
    fn cap(n: u64) -> CaptureInput {
        CaptureInput {
            state: present(u8::try_from(n % 251).unwrap_or(0), n),
            version: clock(n),
            origin_is_local: true,
            size_hint: 0,
        }
    }

    fn adaptive() -> PressureController {
        PressureController::new(HistoryMode::Adaptive, PressureConfig::default())
    }

    // ---- mode behaviors ---------------------------------------------------

    #[test]
    fn every_change_is_always_immediate_and_never_stages() {
        let mut pc = PressureController::new(HistoryMode::EveryChange, PressureConfig::default());
        for t in 0..100 {
            // A dense storm that would escalate an adaptive controller.
            assert_eq!(pc.note(path("f"), cap(t), t), CaptureDecision::Immediate);
        }
        assert_eq!(pc.staged_len(), 0);
        assert_eq!(pc.rung(), 0);
    }

    #[test]
    fn off_swallows_everything() {
        let mut pc = PressureController::new(HistoryMode::Off, PressureConfig::default());
        for t in 0..50 {
            assert_eq!(pc.note(path("f"), cap(t), t), CaptureDecision::Dropped);
        }
        assert_eq!(pc.staged_len(), 0);
        assert!(pc.next_due_ms().is_none());
    }

    #[test]
    fn interval_defers_at_fixed_deadline_without_escalation() {
        let mut pc =
            PressureController::new(HistoryMode::IntervalMs(500), PressureConfig::default());
        // First change of a burst sets the deadline at staged_at + 500.
        assert_eq!(
            pc.note(path("f"), cap(0), 100),
            CaptureDecision::Deferred { due_at_ms: 600 }
        );
        // A dense burst never escalates, and the slot keeps its original deadline.
        for t in 101..400 {
            assert_eq!(
                pc.note(path("f"), cap(t), t),
                CaptureDecision::Deferred { due_at_ms: 600 }
            );
        }
        assert_eq!(pc.rung(), 0);
        let due = pc.poll_due(600);
        assert_eq!(due.len(), 1);
        // Newest content wins the slot.
        assert_eq!(due[0].1.version, clock(399));
    }

    #[test]
    fn interval_zero_behaves_like_every_change() {
        let mut pc = PressureController::new(HistoryMode::IntervalMs(0), PressureConfig::default());
        assert_eq!(pc.note(path("f"), cap(0), 0), CaptureDecision::Immediate);
        assert_eq!(pc.note(path("f"), cap(1), 1), CaptureDecision::Immediate);
        assert_eq!(pc.staged_len(), 0);
    }

    // ---- adaptive: calm ---------------------------------------------------

    #[test]
    fn adaptive_calm_is_always_immediate() {
        let mut pc = adaptive();
        // 300 ms apart → ~3 events/s, far below the 20/s threshold.
        for k in 0..20 {
            let t = k * 300;
            assert_eq!(pc.note(path("f"), cap(k), t), CaptureDecision::Immediate);
            assert_eq!(pc.rung(), 0);
        }
        assert_eq!(pc.staged_len(), 0);
    }

    // ---- adaptive: escalation ---------------------------------------------

    #[test]
    fn adaptive_storm_escalates_monotonically_to_max_and_stays() {
        let mut pc = adaptive();
        let max_rung = PressureConfig::default().ladder_ms.len() - 1;
        let mut prev = 0usize;
        // 100 notes, 10 ms apart: ~100 events/s, well above threshold.
        for k in 0..100 {
            let t = k * 10;
            pc.note(path("f"), cap(k), t);
            assert!(pc.rung() >= prev, "rung must never drop mid-storm");
            prev = pc.rung();
        }
        assert_eq!(pc.rung(), max_rung, "sustained storm pins the top rung");
    }

    #[test]
    fn adaptive_escalates_on_staged_bytes() {
        let config = PressureConfig {
            max_staged_bytes: 10,
            ..PressureConfig::default()
        };
        let mut pc = PressureController::new(HistoryMode::Adaptive, config);
        // First change is immediate (rung 0, no bytes staged yet), but only
        // once we are staging do bytes accumulate — so prime with a slow pair
        // that crosses the byte threshold to force a climb without needing a
        // high arrival rate.
        let big = CaptureInput {
            size_hint: 1000,
            ..cap(0)
        };
        // rung 0, nothing staged → immediate, no bytes retained.
        assert_eq!(
            pc.note(path("a"), big.clone(), 0),
            CaptureDecision::Immediate
        );
        // Feed a queue-depth signal to bump off rung 0 so subsequent changes
        // stage and accumulate bytes.
        pc.signals(QUEUE_HIGH_WATER + 1, 1);
        assert_eq!(pc.rung(), 1);
        pc.note(path("b"), big.clone(), 2);
        // Now staged_bytes (1000) exceeds max_staged_bytes (10): next note climbs.
        let before = pc.rung();
        pc.note(path("c"), big, 3);
        assert!(pc.rung() > before);
    }

    #[test]
    fn adaptive_escalates_on_queue_depth_via_signals() {
        let mut pc = adaptive();
        assert_eq!(pc.rung(), 0);
        pc.signals(QUEUE_HIGH_WATER + 1, 10);
        assert_eq!(pc.rung(), 1);
        pc.signals(QUEUE_HIGH_WATER + 1, 20);
        assert_eq!(pc.rung(), 2);
    }

    // ---- adaptive: decay --------------------------------------------------

    #[test]
    fn adaptive_decays_stepwise_to_zero_when_idle() {
        let mut pc = adaptive();
        // Drive to the top rung with a storm.
        for k in 0..100 {
            pc.note(path("f"), cap(k), k * 10);
        }
        let top = pc.rung();
        assert!(top >= 3);
        let last = 99 * 10;
        // Each idle decay_idle_ms (2000) drops exactly one rung.
        pc.signals(0, last + 2000);
        assert_eq!(pc.rung(), top - 1);
        pc.signals(0, last + 4000);
        assert_eq!(pc.rung(), top - 2);
        pc.signals(0, last + 6000);
        assert_eq!(pc.rung(), top - 3);
        // Saturates at 0.
        pc.signals(0, last + 100_000);
        assert_eq!(pc.rung(), 0);
    }

    #[test]
    fn decay_can_drop_multiple_rungs_in_one_jump() {
        let mut pc = adaptive();
        for k in 0..100 {
            pc.note(path("f"), cap(k), k * 10);
        }
        assert_eq!(pc.rung(), 3);
        // A single call after 3× the idle window drops three rungs at once.
        pc.signals(0, 99 * 10 + 6000);
        assert_eq!(pc.rung(), 0);
    }

    // ---- staging mechanics ------------------------------------------------

    #[test]
    fn replacement_keeps_original_deadline_and_newest_content() {
        let mut pc = adaptive();
        // Force onto a non-zero rung via queue back-pressure.
        pc.signals(QUEUE_HIGH_WATER + 1, 0);
        let rung = pc.rung();
        let interval = PressureConfig::default().ladder_ms[rung];
        assert!(interval > 0);

        let d0 = pc.note(path("f"), cap(1), 100);
        let CaptureDecision::Deferred { due_at_ms: due0 } = d0 else {
            panic!("expected deferred");
        };
        assert_eq!(due0, 100 + interval);

        // A later edit in the same burst keeps due0 (does not push it out).
        let d1 = pc.note(path("f"), cap(2), 150);
        assert_eq!(d1, CaptureDecision::Deferred { due_at_ms: due0 });

        // Flushing at the original deadline yields the newest content.
        let flushed = pc.poll_due(due0);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.version, clock(2));
        assert_eq!(flushed[0].1.staged_at_ms, 150);
        assert_eq!(pc.staged_len(), 0);
    }

    #[test]
    fn interval_zero_note_supersedes_a_pending_slot_without_discarding() {
        let mut pc = adaptive();
        // Stage something at a high rung.
        pc.signals(QUEUE_HIGH_WATER + 1, 0);
        pc.note(path("f"), cap(1), 100);
        assert_eq!(pc.staged_len(), 1);
        // Decay back to rung 0.
        pc.signals(0, 100 + 10_000);
        assert_eq!(pc.rung(), 0);
        // An interval-0 note against the pending slot does NOT return Immediate
        // (which would leave the older capture orphaned): it replaces the slot,
        // keeping the original — now already-overdue — deadline, so the pending
        // capture is superseded by the newest content and never discarded.
        let now = 100 + 10_000;
        let d = pc.note(path("f"), cap(2), now);
        assert!(
            matches!(d, CaptureDecision::Deferred { due_at_ms } if due_at_ms <= now),
            "interval-0 note over a pending slot must defer an immediately-due replacement, got {d:?}"
        );
        assert_eq!(pc.staged_len(), 1);
        let flushed = pc.poll_due(now);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.version, clock(2));
    }

    #[test]
    fn poll_due_returns_ascending_and_removes() {
        let mut pc =
            PressureController::new(HistoryMode::IntervalMs(100), PressureConfig::default());
        pc.note(path("b"), cap(1), 0); // due 100
        pc.note(path("a"), cap(2), 5); // due 105
        pc.note(path("c"), cap(3), 0); // due 100
        assert_eq!(pc.staged_len(), 3);
        let due = pc.poll_due(200);
        let order: Vec<(&str, u64)> = due.iter().map(|(p, c)| (p.as_str(), c.due_at_ms)).collect();
        // (due_at, path) ascending: (100,"b"),(100,"c"),(105,"a").
        assert_eq!(order, [("b", 100), ("c", 100), ("a", 105)]);
        assert_eq!(pc.staged_len(), 0);
    }

    #[test]
    fn next_due_ms_tracks_earliest_and_is_pollable() {
        let mut pc =
            PressureController::new(HistoryMode::IntervalMs(100), PressureConfig::default());
        assert_eq!(pc.next_due_ms(), None);
        pc.note(path("b"), cap(1), 50); // due 150
        pc.note(path("a"), cap(2), 0); // due 100
        assert_eq!(pc.next_due_ms(), Some(100));
        let d = pc.next_due_ms().unwrap();
        assert!(!pc.poll_due(d).is_empty());
    }

    // ---- property tests ---------------------------------------------------

    /// One simulated operation against the controller.
    #[derive(Debug, Clone)]
    enum Op {
        Note { path_idx: usize, dt: u64 },
        Poll { dt: u64 },
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0usize..4, 0u64..2500).prop_map(|(path_idx, dt)| Op::Note { path_idx, dt }),
            (0u64..2500).prop_map(|dt| Op::Poll { dt }),
        ]
    }

    const PATHS: [&str; 4] = ["a", "b", "c", "d"];

    proptest! {
        /// THE property (invariant #4): for any sequence of notes interleaved
        /// with polls, after the last note a poll at `last + ladder.max()`
        /// leaves nothing staged and every path's *latest* noted version has
        /// been recorded (immediately or via a flush); and every flushed
        /// capture is the newest version noted for its path up to that poll.
        #[test]
        fn prop_no_lost_final_write(ops in proptest::collection::vec(arb_op(), 1..200)) {
            let mut pc = adaptive();
            let max = *PressureConfig::default().ladder_ms.iter().max().unwrap();

            let mut now = 0u64;
            // Per-path monotonically increasing note counter → unique versions.
            let mut counter = [0u64; 4];
            // The latest version noted per path (whatever its decision).
            let mut latest: [Option<VectorClock>; 4] = [None, None, None, None];
            // Versions recorded (immediately or flushed).
            let mut recorded: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
            // Whether a path currently has a pending staged slot (mirror).
            let mut last_note_time = 0u64;

            let key = |v: &VectorClock| -> Vec<u8> {
                let mut out = Vec::new();
                for (r, c) in v.iter() {
                    out.extend_from_slice(&r.0.to_le_bytes());
                    out.extend_from_slice(&c.to_le_bytes());
                }
                out
            };

            for op in &ops {
                match op {
                    Op::Note { path_idx, dt } => {
                        now = now.saturating_add(*dt);
                        last_note_time = now;
                        counter[*path_idx] += 1;
                        let n = counter[*path_idx];
                        let input = CaptureInput {
                            state: present(u8::try_from(n % 251).unwrap_or(0), n),
                            version: clock(n),
                            origin_is_local: true,
                            size_hint: 0,
                        };
                        latest[*path_idx] = Some(clock(n));
                        match pc.note(path(PATHS[*path_idx]), input, now) {
                            CaptureDecision::Immediate => {
                                recorded.insert(key(&clock(n)));
                            }
                            CaptureDecision::Deferred { .. } => {}
                            CaptureDecision::Dropped => unreachable!("adaptive never drops"),
                        }
                    }
                    Op::Poll { dt } => {
                        now = now.saturating_add(*dt);
                        for (p, c) in pc.poll_due(now) {
                            // Newest-wins: the flushed version is the latest
                            // noted for that path so far.
                            let idx = PATHS.iter().position(|&x| x == p.as_str()).unwrap();
                            prop_assert_eq!(&c.version, latest[idx].as_ref().unwrap());
                            recorded.insert(key(&c.version));
                        }
                    }
                }
            }

            // Final drain well past every possible deadline.
            let drain_at = last_note_time.max(now).saturating_add(max).saturating_add(1);
            for (_, c) in pc.poll_due(drain_at) {
                recorded.insert(key(&c.version));
            }

            // Nothing left staged, and every path's final version is recorded.
            prop_assert_eq!(pc.staged_len(), 0);
            for (idx, lv) in latest.iter().enumerate() {
                if let Some(v) = lv {
                    prop_assert!(
                        recorded.contains(&key(v)),
                        "final version of path {} was lost",
                        PATHS[idx]
                    );
                }
            }
        }

        /// Monotonic escalation: with no idle gap ≥ decay_idle_ms and no polls,
        /// the rung never decreases across a note sequence.
        #[test]
        fn prop_monotonic_escalation(gaps in proptest::collection::vec(0u64..1999, 1..300)) {
            let mut pc = adaptive();
            let mut now = 0u64;
            let mut prev = 0usize;
            for (k, g) in gaps.iter().enumerate() {
                now = now.saturating_add(*g);
                pc.note(path("f"), cap(k as u64), now);
                prop_assert!(pc.rung() >= prev);
                prev = pc.rung();
            }
        }

        /// Determinism: the same op sequence yields identical decisions, rungs,
        /// and poll results on two independent runs.
        #[test]
        fn prop_deterministic(ops in proptest::collection::vec(arb_op(), 1..200)) {
            let run = |ops: &[Op]| {
                let mut pc = adaptive();
                let mut now = 0u64;
                let mut counter = [0u64; 4];
                let mut trace: Vec<String> = Vec::new();
                for op in ops {
                    match op {
                        Op::Note { path_idx, dt } => {
                            now = now.saturating_add(*dt);
                            counter[*path_idx] += 1;
                            let d = pc.note(path(PATHS[*path_idx]), cap(counter[*path_idx]), now);
                            trace.push(format!("n{path_idx}:{d:?}:r{}", pc.rung()));
                        }
                        Op::Poll { dt } => {
                            now = now.saturating_add(*dt);
                            let due = pc.poll_due(now);
                            let ids: Vec<&str> = due.iter().map(|(p, _)| p.as_str()).collect();
                            trace.push(format!("p:{ids:?}:r{}", pc.rung()));
                        }
                    }
                }
                trace
            };
            prop_assert_eq!(run(&ops), run(&ops));
        }

        /// next_due_ms consistency: whenever something is staged, polling at
        /// the reported earliest deadline returns at least one capture.
        #[test]
        fn prop_next_due_is_pollable(gaps in proptest::collection::vec(1u64..30, 1..100)) {
            // Interval mode guarantees staging (fixed 250 ms), decoupling the
            // property from escalation timing.
            let mut pc = PressureController::new(
                HistoryMode::IntervalMs(250),
                PressureConfig::default(),
            );
            let mut now = 0u64;
            for (k, g) in gaps.iter().enumerate() {
                now = now.saturating_add(*g);
                let idx = k % 4;
                pc.note(path(PATHS[idx]), cap(k as u64), now);
            }
            if pc.staged_len() > 0 {
                let d = pc.next_due_ms().unwrap();
                prop_assert!(!pc.poll_due(d).is_empty());
            }
        }

        /// poll_due output is always sorted ascending by (due_at, path).
        #[test]
        fn prop_poll_due_sorted(gaps in proptest::collection::vec(0u64..30, 1..100), at in 0u64..5000) {
            let mut pc = PressureController::new(
                HistoryMode::IntervalMs(100),
                PressureConfig::default(),
            );
            let mut now = 0u64;
            for (k, g) in gaps.iter().enumerate() {
                now = now.saturating_add(*g);
                pc.note(path(PATHS[k % 4]), cap(k as u64), now);
            }
            let due = pc.poll_due(at);
            for w in due.windows(2) {
                let (pa, ca) = &w[0];
                let (pb, cb) = &w[1];
                prop_assert!(
                    (ca.due_at_ms, pa) <= (cb.due_at_ms, pb)
                );
            }
        }
    }

    /// Bounded staleness (example, rigorous): while a single path is edited
    /// continuously, `poll_due` yields a checkpoint at least every
    /// `ladder.max()` ms (plus poll granularity), even though the edits never
    /// pause. Only *in-storm* flushes are bounded — the post-storm drain of the
    /// final still-pending slot is a tail, not a staleness violation.
    #[test]
    fn bounded_staleness_continuous_storm() {
        let mut pc = adaptive();
        let max = *PressureConfig::default().ladder_ms.iter().max().unwrap(); // 5000
        let gap = 10u64;
        let mut flush_times: Vec<u64> = Vec::new();
        // 20 s of storm at 10 ms spacing; poll at every note time.
        for k in 0..2000u64 {
            let t = k * gap;
            pc.note(path("f"), cap(k), t);
            for _ in pc.poll_due(t) {
                flush_times.push(t);
            }
        }

        assert!(
            flush_times.len() >= 3,
            "a 20 s storm must yield several checkpoints, got {}",
            flush_times.len()
        );
        // A uniform grid keeps deadlines aligned, so consecutive checkpoints are
        // ladder.max apart to within one poll granularity.
        for w in flush_times.windows(2) {
            let delta = w[1] - w[0];
            assert!(
                delta <= max + gap,
                "checkpoint gap {delta} exceeded ladder.max + poll granularity"
            );
        }
        // The final still-pending slot drains cleanly past its deadline.
        let last = 1999 * gap;
        pc.poll_due(last + max);
        assert_eq!(pc.staged_len(), 0);
    }

    /// Bounded staleness (property): random small gaps, long enough to reach
    /// and hold the top rung; consecutive in-storm checkpoints stay within
    /// `ladder.max()` plus timing-grid slack on both the deadline and the poll.
    #[test]
    fn bounded_staleness_property() {
        // A hand-rolled deterministic pseudo-storm across several seeds keeps
        // this cheap while still varying the timing grid.
        for seed in 0u64..8 {
            let mut pc = adaptive();
            let max = *PressureConfig::default().ladder_ms.iter().max().unwrap();
            let mut now = 0u64;
            let mut max_gap = 0u64;
            let mut flush_times: Vec<u64> = Vec::new();
            for k in 0..3000u64 {
                // Gaps in 5..=15 ms, dense enough to hold the top rung.
                let g = 5 + (k.wrapping_mul(2_654_435_761).wrapping_add(seed)) % 11;
                max_gap = max_gap.max(g);
                now += g;
                pc.note(path("f"), cap(k), now);
                for _ in pc.poll_due(now) {
                    flush_times.push(now);
                }
            }
            assert!(flush_times.len() >= 3, "seed {seed}: too few checkpoints");
            // A deadline lands up to one gap after the burst's first unflushed
            // change, and the flushing poll lands up to one gap after the
            // deadline, so the checkpoint interval is bounded by max + 2*gap.
            for w in flush_times.windows(2) {
                assert!(
                    w[1] - w[0] <= max + 2 * max_gap,
                    "seed {seed}: checkpoint gap {} exceeded bound",
                    w[1] - w[0]
                );
            }
            pc.poll_due(now + max);
            assert_eq!(pc.staged_len(), 0, "seed {seed}: slot not drained");
        }
    }
}
