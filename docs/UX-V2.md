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

## 1. Session lifecycle: detach & attach — **implemented (v0.2)**

Graduated into docs/SPEC.md §13.4. Provisional rulings decided 2026-07-22 (Jake):
the attach verb is **`tomo attach`** (no bare-`tomo` shortcut — open question #1);
**foreground stays the default** for `tomo sync`, with `-d` opting into the
background (open question #2); and **all attached clients have command rights**
(the control socket already serves any local client — open question #4). The
detached child detaches via its own process group + SIGHUP-ignore rather than
`setsid(2)`, because the workspace `unsafe_code = "forbid"` lint rules out the
unsafe `pre_exec` closure `setsid` would need.

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

## 3. TUI: the default interactive surface — **implemented (v0.2)**: §3a/§3b
shipped per the mockups; the TUI is the default on a tty for `tomo attach` and
foreground `tomo sync` (which now runs as detached-session + attached TUI, one
codepath; `q` stop-confirm / `d` detach). Deviations recorded in SPEC §13.
Event schema (question 5) shipped as v1, additive-only. Pause/resume ruled IN
(Jake, 2026-07-22 — the forgotten-paused-session risk accepted) and ships in
v0.2.1 alongside the history browser and real conflict-center undo.

TUI becomes the default when stdout is a tty; every non-TUI surface remains
first-class (see §5). Candidate stack: ratatui + crossterm (record as a
dependency decision in SPEC when building). Requirements:

General requirements:

- **History browsing** (TUI v2): pick a path (from the stream or a fuzzy
  finder), see its version timeline, diff any two versions, restore with
  confirmation — the "time machine" without leaving the session.
- **Pause/resume** (`space`): pause applies/ships while continuing to observe
  and queue (the offline-queue machinery already models this). Paused state is
  loud in the status line. Resume replays the queue. [open question #3]
- **Degradation**: honors NO_COLOR / TOMO_COLOR / TOMO_ASCII; not-a-tty falls
  back to the plain stream automatically; tiny terminals get a minimal
  single-pane layout rather than a broken one.
- The TUI renders event-stream data only — no filesystem walking or DB writes
  from the render path; commands go through the control channel like every
  other client.
- **Alternate screen with an exit summary**: the TUI runs on the alt screen
  (stable chrome needs it), which would otherwise leave the real terminal's
  scrollback empty — so on exit tomo prints a compact session summary
  (`synced 214 files · 2 conflicts resolved · 1 open · 47 min`). `--plain`
  remains the no-alt-screen mode, byte-compatible with today's stream.

### 3a. Main screen (decided 2026-07-22): the stream plus a heartbeat

Design principle: keep the calm of the current output. The body IS today's
stream — same glyphs, colors, and wording; the TUI adds exactly two lines of
chrome and two invisible-until-used capabilities. No dashboard, no panes.

```
  ✓ src/train.py
  ✓ src/config.yaml
  ⚠ conflict src/train.py — kept vm8's copy · c to review
  ✓ assets/logo.png

  ⇡ model.ckpt  ██████████░░░░░░  58% · 41 MB/s        ← pinned transfer zone
 ─────────────────────────────────────────────────────
  vm8 ✓ connected · ⚠ 1 · last sync 2s ago · c conflicts  ? help
```

- **Status line** (1 line, bottom): peer name + connection state (reconnect
  countdown when offline), conflict badge, and `last sync Ns ago` — the
  heartbeat that makes a silent screen legible (working vs dead is the core
  anxiety of every sync tool). Paused state renders loudly here.
- **Pinned transfer zone**: the transient progress the CLI already draws, at
  a stable position above the status line; concurrent transfers stack; empty
  (zero height) when idle.
- **Stream scrollback**: PgUp browses history without losing tail-follow; new
  events show a `▾ new activity` nudge; `End`/`G` re-sticks to the tail.
- **Stream filter**: `/substr` narrows the stream to matching paths (less-
  style), `Esc` clears. Filter state shows in the status line.
- **Keys on the main screen — five, not fifteen**: `c` conflict center,
  `/` filter, `d` detach, `q` quit (stop only if started foreground-attached,
  else detach — §1 semantics), `?` help overlay. History browsing and pause
  join later without changing this layout.

### 3b. Conflict center (decided 2026-07-22)

Framing that shapes everything: a tomo conflict never blocks anything — the
tree already converged deterministically and the loser is safe in history, so
this is a REVIEW flow, not an unblocking flow. One-keystroke verdicts are safe
to offer because nothing is waiting and every choice is undoable.

Entry: `c` from the main screen (badge shows the count the moment a ⚠ lands
in the stream — no modal interruption, ever).

```
┌ tomo ── vm8 (192.168.1.40) ── connected ── ⚠ 3 ────────────────────┐
│ CONFLICTS                          │ src/train.py                  │
│ > src/train.py     2m ago  ⚠      │  on disk now — vm8, 14:32:07  │
│     kept: vm8's copy               │  in history  — you, 14:32:05  │
│   src/config.yaml  2m ago  ⚠      │ ─────────────────────────────  │
│   adoption from vm8 (12 files) ▸   │  @@ -18,7 +18,9 @@            │
│                                    │ -    lr = 3e-4                │
│                                    │ +    lr = 1e-4                │
│                                    │ +    warmup = 500             │
├────────────────────────────────────┴───────────────────────────────┤
│ enter keep · t take yours · b keep both · u undo · a ack all · ?   │
│ = tomo conflicts resolve 7 --keep-current                          │
└────────────────────────────────────────────────────────────────────┘
```

- **List + live diff**: `j`/`k` selects; the diff pane follows instantly
  (reuses the `tomo diff` textdiff machinery). Newest first.
- **Semantic framing, never `a`/`b`**: "on disk now — **vm8's** copy,
  14:32:07" vs "in history — **yours**, 14:32:05". Peer names come from the
  v0.1.13 peer identity; sides keep consistent colors across the whole TUI
  (yours cyan, peer magenta). Timestamps are display-only wall time
  (invariant #7 untouched).
- **Single-key verdicts, Gmail-style auto-advance**: `Enter`/`k` keep current
  (acknowledge — the common case), `t` take yours (restores the preserved
  version and syncs out live), `b` keep both (materializes `<path>.theirs`,
  which syncs like any file — merge by hand on either machine), `space` skip,
  `a` ack-all-remaining (with count confirm). After a verdict the selection
  auto-advances; last one resolved → "0 conflicts 🎉" → back to the stream.
- **`u` undo — the trust-builder**: a resolution is itself reversible because
  every version is in history; `u` flips the last verdict. This is the
  property git conflict UX cannot offer; surface it prominently.
- **Adoption groups**: genesis adoptions arrive as a collapsible group row
  (`adoption from vm8 (12 files) ▸`). A verdict on the header applies to the
  whole group; expand to cherry-pick. Mass review without hiding anything.
- **CLI echo footer** (magit's trick): the bottom line always shows the exact
  CLI equivalent of the highlighted action (`= tomo conflicts resolve 7
  --keep-current`) — passive use of the TUI teaches the scriptable surface,
  and proves the TUI holds no privileged powers.
- **Escalation for hard merges**: `b` + your editor is the v0.2 answer;
  $EDITOR/3-way integration stays out of scope (§6) until usage proves the
  need.

### 3c. Adoption view

On a first-contact sync (genesis), summarize what adoption did — "12 files
adopted from vm8, 3 kept local — review: c" — informational, never blocking
(invariant #5); the review list is the adoption group in the conflict center
(§3b).

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

- Desktop notifications on conflict — **dropped from consideration**
  (Jake, 2026-07-22).
- Editor integration (open conflict in $EDITOR as a 3-way merge) — revisit
  after `--both` ships and real usage shows the need.
- Web UI / remote dashboard — **a v0.3 candidate** (Jake, 2026-07-22); the
  control socket is the foundation it would build on.
- Three-plus-machine topologies — separate track, not a UX feature.

## 7. Open questions (Jake to rule when implementation nears)

1. `tomo attach` vs `tomo` (bare, in an initialized project) as the attach
   spelling — or both? **Decided 2026-07-22: `tomo attach` only (no bare-`tomo`
   shortcut).**
2. Should `-d` be the DEFAULT for `tomo sync` once attach exists (i.e. sync
   always daemonizes, foreground is `sync --attach`)? Current lean: keep
   foreground default; muscle memory and least surprise. **Decided 2026-07-22:
   foreground stays the default; `-d` opts into the background.**
3. Pause/resume: include in v0.2 or defer? (Machinery exists; the UX risk is
   a forgotten-paused session — mitigated by loud header + status badge.)
4. Multiple attached clients: all get command rights, or
   first-attached-writes / rest-observe? **Decided 2026-07-22: all attached
   clients have command rights (the socket serves any local client).**
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
