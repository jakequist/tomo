# Tomo — Roadmap

Milestones in order. Each is gated by its Definition of Done in
`docs/TESTING.md`. Do not start milestone N+1 with N's scenarios red.

## M0 — Foundations
- Workspace builds; CI green; scenario harness runs (self-SSH smoke test).
- `tomo-engine`: index model, canonical change-event type, vector clock
  implementation with full property-test suite.
- `tomo-config`: config parsing, three path classes, glob rules, hardcoded
  `.tomo/**` ignore constant.

## M1 — One-way, then two-way local sync (Tier 1: scenarios 01–03)
- `tomo-watch`: inotify adapter (Linux first — that's the dev VM), atomic-save
  canonicalization, echo-suppression journal.
- Engine wiring: watcher → engine → apply-actions with staging + atomic rename.
- Run both "machines" as two local processes on one box before SSH exists
  (harness supports a `--local` transport mode via unix socket or pipes) —
  proves the engine loop without transport risk.

## M2 — SSH transport & bootstrap (Tier 1 complete: scenario 04)
- `tomo-proto` framing + handshake; `tomo-transport` SSH session, remote
  process spawn, stdio tunnel, SFTP binary push with SHA-256 verify.
- Self-SSH scenarios pass end-to-end over real sshd.

## M3 — History (Tier 2: scenarios 05–06)
- `tomo-history`: FastCDC + BLAKE3 + zstd CAS, SQLite metadata,
  `tomo log`/`tomo restore`.
- Adaptive pressure controller in engine; wire pressure signals from adapters.

## M4 — Conflicts (Tier 2 complete: scenarios 07–08)
- Concurrent detection, deterministic LWW tiebreak, conflict records,
  `tomo conflicts`, non-blocking CLI signaling.

## M5 — Robustness (Tier 3: scenarios 09–13)
- Crash-safe resume, offline queueing, transfer interleaving/prioritization,
  lag variants green at 50/200 ms RTT.

## M6 — Release engineering
- Cross-build matrix (musl x2, darwin x2), binary embedding behind a feature
  flag, `cargo deny` config, versioned release artifacts, macOS FSEvents
  adapter (needs a mac or CI runner — flag for the human if unavailable).

## Later / ambitions
- API protocol (JSON over local socket) for tooling and debugging.
- Rename tracking; multi-replica; Windows; the git-replacement road.
