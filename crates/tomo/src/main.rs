//! Tomo CLI entry point.
//!
//! Commands (docs/SPEC.md §9): init, connect, watch, status, log, restore,
//! conflicts. All informational commands must support `--json` from day one —
//! the e2e scenarios assert against it. Libraries return data; only this
//! crate prints.

fn main() {
    // TODO(M0): clap-based CLI skeleton with the commands above, each
    // returning a proper error (no unwrap) and rendering context.
    eprintln!("tomo: scaffold — no commands implemented yet (see docs/ROADMAP.md)");
    std::process::exit(2);
}
