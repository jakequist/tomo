//! Bridging `tomo-config`'s [`tomo_config::HistoryMode`] onto the engine's
//! pure [`tomo_engine::HistoryMode`] and onto the stable status/CLI label.
//!
//! The config type is the on-disk `history.mode` value; the engine type is what
//! the pressure controller interprets. These are separate types on purpose
//! (config parsing vs. pure state machine), so the CLI crate owns the one-way
//! mapping between them. Keeping it here — small and unit-tested — means both
//! the session loop and the status/log commands agree on the translation.

use tomo_config::HistoryMode as ConfigMode;
use tomo_engine::HistoryMode as EngineMode;

/// Translate the parsed config history mode into the engine's pressure-controller
/// mode. `interval_ms` carries through unchanged (an interval of `0` behaves like
/// every-change inside the controller, documented there).
pub fn to_engine(mode: &ConfigMode) -> EngineMode {
    match mode {
        ConfigMode::Adaptive => EngineMode::Adaptive,
        ConfigMode::EveryChange => EngineMode::EveryChange,
        ConfigMode::Off => EngineMode::Off,
        ConfigMode::Interval { interval_ms } => EngineMode::IntervalMs(*interval_ms),
    }
}

/// A stable, machine-readable label for a history mode, used in `status.json`
/// and the human `tomo status`/`tomo log` output. The interval variant collapses
/// to `"interval"` (the exact interval is a config detail, not a status field).
pub fn label(mode: &ConfigMode) -> &'static str {
    match mode {
        ConfigMode::Adaptive => "adaptive",
        ConfigMode::EveryChange => "every-change",
        ConfigMode::Off => "off",
        ConfigMode::Interval { .. } => "interval",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_config_mode_onto_engine() {
        assert_eq!(to_engine(&ConfigMode::Adaptive), EngineMode::Adaptive);
        assert_eq!(to_engine(&ConfigMode::EveryChange), EngineMode::EveryChange);
        assert_eq!(to_engine(&ConfigMode::Off), EngineMode::Off);
        assert_eq!(
            to_engine(&ConfigMode::Interval { interval_ms: 5000 }),
            EngineMode::IntervalMs(5000)
        );
        // An interval of zero maps to the zero-interval engine mode (which the
        // controller treats like every-change).
        assert_eq!(
            to_engine(&ConfigMode::Interval { interval_ms: 0 }),
            EngineMode::IntervalMs(0)
        );
    }

    #[test]
    fn labels_are_stable_strings() {
        assert_eq!(label(&ConfigMode::Adaptive), "adaptive");
        assert_eq!(label(&ConfigMode::EveryChange), "every-change");
        assert_eq!(label(&ConfigMode::Off), "off");
        assert_eq!(
            label(&ConfigMode::Interval { interval_ms: 250 }),
            "interval"
        );
    }
}
