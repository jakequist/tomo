//! The interactive terminal UI (UX-V2 §3): a calm event stream with a heartbeat
//! status line (§3a) and a review-oriented conflict center (§3b), driven
//! entirely by the local control channel (docs/SPEC.md §13).
//!
//! The module is split so that all logic is testable without a terminal:
//!
//! - [`state`] — the pure core: a `Model` plus a `(Model, Msg) -> Model`
//!   reducer holding every interaction decision. No I/O, no clock, no threads.
//! - [`view`] — a pure `Model` → `ratatui` render function.
//! - [`run`] — the thin I/O shell: socket reader thread, crossterm input,
//!   command dispatch, and the render loop.
//!
//! It is currently reached only through the hidden `tomo dev tui` command; the
//! lead wires it as the default interactive surface once the `attach` lifecycle
//! lands.

pub mod run;
pub mod state;
pub mod view;

pub use run::run;
