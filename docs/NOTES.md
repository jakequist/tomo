# Tomo — Build Notes: Bugs & Improvement Backlog

Running log kept by the autonomous build (2026-07-17 overnight run). Two
sections: **Bugs** found while dogfooding the live CLI, and **Improvements**
beyond the spec worth doing once milestones are complete. Each entry gets a
status when addressed.

## Bugs

(none yet)

## Improvements

- **Editor temp-file churn**: rename-based saves briefly sync the temp file
  (e.g. `.main.rs.swp.123`) to the peer before its deletion propagates.
  Converges, but wasteful and will pollute history at M3. Consider default
  ignore patterns for common editor temps (`*.swp*`, `*~`, `.#*`, `4913`)
  and/or stateful rename pairing in the canonicalizer. (Found reviewing M1
  watch design, 2026-07-17.)
- **Local control socket**: `tomo status` reads a status file written by the
  watch process (M1 design). A local socket (SPEC future "API protocol")
  would give live queries without file staleness. Post-M6.

## Environment / build-run journal

- 2026-07-17: `rustup target add x86_64-unknown-linux-musl` failed with a
  rustup download-cache error on first attempt; retry needed before M6.
