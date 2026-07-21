# Tomo v0.2 — Interface & UX requirements

Status: **requirements capture only** — no implementation yet. Recorded
2026-07-21 from a design discussion with Jake. This document collects the
decided direction for the next major version's interface; docs/SPEC.md remains
authoritative for shipped behavior. When implementation begins, each section
graduates into SPEC.md with its design decisions argued there.

## 0. The one-sentence model

**A sync session is a server; every interface — TUI, plain stream, JSON,
scripts — is a client that attaches to it.** tmux semantics: sessions run
headless or attached, UIs come and go, sync never blocks on a UI.

## 1. Session lifecycle: detach & attach

- `tomo sync -d | --detach` — start the session in the background. Prints the
  pid and how to attach. The single-session flock (unchanged) refuses a second
  session.
- `tomo attach` — join the running session from any terminal: TUI by default,
  `--plain` for the current line-stream, `--json` for the event stream.
  Detaching (keystroke or SIGHUP) never disturbs the session. Multiple
  simultaneous attachments are supported (all can issue commands; the session
  serializes them — same as two terminals running CLI commands today).
- `tomo stop` — clean shutdown of the running session (SIGTERM path already
  exists). `tomo logs [-f]` — the session log (`.tomo/logs/`), follow mode.
- Foreground `tomo sync` becomes exactly equivalent to `sync -d` + implicit
  `attach`, so there is ONE session codepath, not two.
- Lifecycle survives the terminal: closing the attached terminal detaches; it
  must never kill the session unless the session was started foreground-attached
  and receives its own SIGINT/SIGTERM.

## 2. The control channel (architectural prerequisite)

- A local control socket at `.tomo/state/ctl.sock` (unix domain socket; state
  stays inside `.tomo/` per invariant #2), serving:
  - a **versioned event stream** (connect/disconnect, file synced/removed,
    transfer progress, conflict recorded, adoption events, pressure/rung
    changes, errors) — the same events the CLI prints today, as structured
    records;
  - a **command channel** (resolve conflict, restore version, pause/resume,
    stop, query state) — every command the CLI exposes, same semantics.
- `status.json` stays (cheap reads, remote-friendly, no socket needed); the
  socket is the low-latency push channel.
- Scriptability: `tomo events --json` streams the event feed for scripts/CI;
  schema is versioned and additive-only once shipped.
- The engine stays a pure state machine (invariant #6): the control server is
  an adapter in the CLI crate, not an engine concern.

## 3. TUI: the default interactive surface

TUI becomes the default when stdout is a tty; every non-TUI surface remains
first-class (see §5). Candidate stack: ratatui + crossterm (record as a
dependency decision in SPEC when building). Requirements:

- **Header**: peer name/addr (from the v0.1.13 peer identity), connection
  state, reconnect countdown when offline, session uptime, queue depth,
  current transfer with progress/throughput.
- **Activity stream**: the live synced/removed/conflict feed, filterable by
  path substring; big transfers render progress inline (transient, like the
  current CLI's transfer lines).
- **Conflict center**: badge with unresolved count; a pane listing conflicts;
  per-conflict inline winner-vs-loser diff (reuse the `tomo diff` textdiff
  machinery); single-key resolution — keep current / take loser / keep both /
  skip. Resolution applies immediately and syncs live; the pane is reachable
  the moment the ⚠ appears.
- **History browsing**: pick a path (from the stream or a fuzzy finder), see
  its version timeline, diff any two versions, restore with confirmation —
  the "time machine" without leaving the session.
- **Adoption view**: on a first-contact sync (genesis), summarize what
  adoption did — "12 files adopted from vm8, 3 kept local — review list" —
  informational, never blocking (invariant #5).
- **Pause/resume** (`space`): pause applies/ships while continuing to observe
  and queue (the offline-queue machinery already models this). Paused state is
  loud in the header. Resume replays the queue. [open question #3]
- **Keys**: arrows + vim keys, `?` help overlay, `d` detach, `q` quit-with-
  confirm (quit = stop session ONLY if started foreground; otherwise detach).
- **Degradation**: honors NO_COLOR / TOMO_COLOR / TOMO_ASCII; not-a-tty falls
  back to the plain stream automatically; tiny terminals get a minimal
  single-pane layout rather than a broken one.
- The TUI renders event-stream data only — no filesystem walking or DB writes
  from the render path; commands go through the control channel like every
  other client.

## 4. Conflict UX (works in both worlds — decided before the TUI discussion)

These land regardless of the TUI and are its command-level foundation:

1. **Actionable conflict lines**: the session prints the ready-to-paste
   command with the id inline —
   `⚠ conflict src/main.rs — kept peer's copy · yours: tomo conflicts resolve 7 --take-loser`.
2. **Path-based resolve**: `tomo conflicts resolve <path> --take-loser`
   resolves that path's newest unresolved conflict; numeric ids remain for
   ambiguous stacks.
3. **See before deciding**: `tomo conflicts show <id|path>` prints an inline
   winner-vs-loser diff.
4. **Keep both**: `--both` materializes the loser alongside the winner (e.g.
   `src/main.rs.theirs`) for manual merging; sync stays non-blocking; the
   sidecar file syncs like any file.
5. **Interactive fallback**: `tomo conflicts resolve --interactive` — a plain
   prompt loop (diff, then keep/take/both/skip per conflict) for non-TUI
   contexts.

## 5. Scriptability guarantees (non-negotiable)

- Every TUI action has a 1:1 CLI equivalent; the TUI is a client of the same
  control channel, never a privileged interface.
- `--plain` (current line stream) and `--json` remain on `sync`/`attach`;
  auto-fallback to plain when not a tty; `TOMO_TUI=0` opts out globally.
- All existing one-shot commands (`status`, `log`, `diff`, `conflicts`,
  `restore`) keep working headless against a live session, as today
  (read-only DB opens; 5s busy timeout on the write paths).
- JSON event/output schemas are versioned; changes are additive.
- Exit codes and `--json` shapes are covered by scenarios (the e2e suite
  asserts on them today; that contract extends to the event stream).

## 6. Explicitly out of scope for v0.2 UX (tracked, not forgotten)

- Desktop notifications on conflict (nice; platform-dependent; later).
- Editor integration (open conflict in $EDITOR as a 3-way merge) — revisit
  after `--both` ships and real usage shows the need.
- Web UI / remote dashboard — the control socket makes it possible someday;
  not now.
- Three-plus-machine topologies — separate track, not a UX feature.

## 7. Open questions (Jake to rule when implementation nears)

1. `tomo attach` vs `tomo` (bare, in an initialized project) as the attach
   spelling — or both?
2. Should `-d` be the DEFAULT for `tomo sync` once attach exists (i.e. sync
   always daemonizes, foreground is `sync --attach`)? Current lean: keep
   foreground default; muscle memory and least surprise.
3. Pause/resume: include in v0.2 or defer? (Machinery exists; the UX risk is
   a forgotten-paused session — mitigated by loud header + status badge.)
4. Multiple attached clients: all get command rights, or
   first-attached-writes / rest-observe?
5. Event schema: settle naming/versioning before first ship (it becomes API).

## 8. Sequencing sketch (when we do build)

1. Control socket + event stream (foundation; replaces nothing, adds channel).
2. Conflict UX package (§4) — ships value immediately, TUI-independent.
3. `-d` / `attach` / `stop` / `logs` lifecycle.
4. TUI v1: header + activity stream + conflict center.
5. TUI v2: history browsing, adoption view, pause/resume.

Each step lands behind the usual gates (scenarios asserting the scriptable
surfaces; TUI logic kept thin enough that the control channel carries the
testable behavior).
