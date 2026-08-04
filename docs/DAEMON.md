# Harvester Daemon Contract (client ↔ harvesterd, protocol v1)

> Status: agreed 2026-08-01, revised after review. Open questions listed at the bottom; everything else is normative.
> Pi behavior claims verified against pi docs + shipped source (v0.83.0); see `HARNESS_RESEARCH.md`.

**harvesterd** is a tiny per-machine daemon (static musl binary, bootstrapped over SSH) that owns `pi --mode rpc` child processes and relays their JSONL protocol to Harvester clients. It exists for exactly one reason: pi RPC shuts down when its stdin closes (source-verified: `rpc-mode.js` binds `process.stdin.on("end", shutdown)`; undocumented behavior, re-verify on pi upgrades), so a session cannot live inside an SSH channel owned by the client.

harvesterd is **stateless**. It persists nothing. Pi's own session files (documented, versioned v3) are the only source of truth; the daemon holds only an in-memory pid table, stdin handles, and the set of attached clients.

## Transport

- Daemon listens on a Unix socket: `~/.harvester/harvesterd.sock`.
- Remote machines: the client forwards the socket over SSH (OpenSSH forwards Unix sockets natively). Local machine: the client spawns harvesterd as a child and connects to the socket directly. Same protocol either way.
- Wire format: **newline-delimited JSON, pi's exact framing rules** — LF (`\n`) is the only record delimiter, strip a trailing `\r`, never split on Unicode separators (valid inside JSON strings).
- Requests carry an optional `id`; the response echoes it. Events carry no `id`. Responses may arrive **out of request order** — slow commands (pi passthroughs, `list_models`, `list_sessions`) answer from their own tasks so they never head-of-line block the connection; the `id` is the correlation, not arrival position.
- **Multiple clients may connect concurrently** — this is a first-class guarantee, not an accident: a desktop client and a (future) mobile client can watch and drive the same session simultaneously. Responses go to the requester; events go to every client attached to that session; any late-joining client recovers full state via `get_entries {since}`. Mobile needs only transport plumbing (an SSH lib), zero protocol changes.
- Socket auth is filesystem permissions (`0700` on `~/.harvester`). Sufficient for single-user VMs. **Future note**: if machines/infra are ever shared between users, revisit with per-user sockets or an auth token in `ping`.

Debugging is `socat - UNIX-CONNECT:~/.harvester/harvesterd.sock` and typing JSON.

## Envelope

Two request types, symmetric — ours and pi's:

```json
{"id": 1, "type": "harvester", "cmd": {"type": "list_sessions"}}
{"id": 2, "type": "pi", "sid": "01H...", "cmd": {"type": "prompt", "message": "fix the tests"}}
```

Responses mirror them:

```json
{"id": 1, "type": "harvester_response", "response": {"success": true, "data": {...}}}
{"id": 2, "type": "pi_response", "sid": "01H...", "response": {"type": "response", "command": "prompt", "success": true}}
```

Unsolicited lines are events (see Events).

## The pi launch line

One line, identical for first spawn and resume (`--session-id` is resume-or-create: exact-id match in the project's session dir opens it, no match creates it — source-verified):

```bash
cd <workspace> && \
PI_OFFLINE=1 GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND="ssh -o BatchMode=yes -o ConnectTimeout=5" \
pi --mode rpc \
  --session-id <sid> \
  --approve \
  [--provider <p>] [--model <m>] [--name <name>] \
  2>> ~/.harvester/log/<sid>.stderr.log
# stdin/stdout owned by harvesterd; stdin stays open for the process lifetime
```

- **`--approve`**: project-local `.pi/` resources (settings, extensions, skills) load normally — the user's repo, the user's machine, the user's call. The hang risk that trust introduces (git package installs prompting for credentials at startup) is neutralized by `GIT_TERMINAL_PROMPT=0` + `GIT_SSH_COMMAND` batch mode: installs fail fast instead of blocking. No readiness probe follows the spawn: pi drains its stdin queue in order, so commands chained straight after `create_session` are answered once it is up (verified live — `HARNESS_RESEARCH.md` §Round 4).
- **No `--session-dir`.** Pi's default per-cwd session dirs are kept; they make list-by-workspace free.
- `PI_OFFLINE=1` disables update checks and telemetry on spawn (see open question 1 re: package installs).
- Session ids are client-generated **uuidv7** — pi's own native session-id format (`session-manager.js` generates ids with `uuidv7()`), and canonical uuidv7 passes pi's id validation charset.
- **Terminate by closing stdin, never SIGTERM** (SIGTERM skips the final stdout flush and may truncate the last line).
- **The stderr log is appended, never truncated** (`2>>`), and each run is preceded by a `--- harvesterd: spawning pi for <sid> at <n> (unix seconds) ---` banner. Spawn *is* resume, so truncating would destroy the record of why the previous run died — exactly what the user is asking for when they resume a crashed session.

## Operations (`type: "harvester"`)

Only what pi cannot do itself. Everything else is the `pi` passthrough.

### `ping`

First command on every connection.

```json
{"id": 1, "type": "harvester", "cmd": {"type": "ping", "version": "0.1.0"}}
{"id": 1, "type": "harvester_response", "response": {"success": true, "data": {"version": "0.1.0"}}}
```

The client compares versions and re-bootstraps on mismatch (see Bootstrap). The string is `env!("CARGO_PKG_VERSION")` on both sides today, which is a **known hazard**: rebuilding harvesterd without bumping the workspace version leaves every bootstrapped machine on the stale binary forever (the probe sees a match and skips the upload), and a client-only bump force-upgrades every daemon, killing every running pi child for a change the wire never saw. The fix is decided — see §Decisions on record (C4).

### `list_sessions`

```json
{"id": 2, "type": "harvester", "cmd": {"type": "list_sessions"}}
{"id": 2, "type": "harvester_response", "response": {"success": true, "data": {"sessions": [
  {"sid": "01H...", "name": "Refactor auth", "cwd": "/work/repo",
   "file": "/home/u/.pi/agent/sessions/--work-repo--/2026-..._01H....jsonl",
   "mtime_ms": 1754049600000, "status": "running"}
]}}}
```

Implemented by readdir over pi's session dirs: header line gives id/cwd/timestamp, the last `session_info` entry gives the display name. `status` is `running` (pid alive in the daemon's table) or `exited`. Optional `cwd` field filters to one workspace.

### `create_session`

```json
{"id": 3, "type": "harvester", "cmd": {"type": "create_session", "sid": "01H...", "cwd": "/work/repo",
 "provider": "anthropic", "model": "sonnet", "name": "wip-a3f2"}}
```

Spawns the launch line. `provider`/`model`/`name` optional (pi defaults apply). Resuming an exited session is the same call with the same `sid` — resume-or-create makes them indistinguishable.

**Idempotent**: a `sid` that is already running answers `{"sid": ..., "already": true}` rather than an error, and no second pi is spawned (the pid table is the guard, so one pi = one session file still holds). This is what lets a client open any session with an unconditional `create_session` → `attach` chain: only the daemon knows whether a session died while that client was detached, so a client-side "is it running" flag would just go stale. A `cwd` that disagrees with the running session is ignored — the session file on disk stays authoritative.

The response means "process spawned"; there is no readiness probe (see the launch-line notes above).

The "ghost chat" from the spec is purely client-side; `create_session` fires on the first prompt.

### `list_models`

```json
{"id": 4, "type": "harvester", "cmd": {"type": "list_models", "cwd": "/work/repo"}}
```

For the ghost-chat model picker, which needs answers before any session exists. The daemon spawns a throwaway `PI_OFFLINE=1 pi --mode rpc --no-session` in `cwd`, sends `get_available_models`, returns pi's `data` verbatim, then closes stdin. Ordering matters: stdin stays open **until the response line is read** — pi treats EOF as shutdown and dies before answering otherwise (verified live on pi 0.83.0). A 15s backstop SIGKILLs a wedged pi (safe: `--no-session` has nothing durable to truncate). Deterministic, touches no session, never cached — every call costs a pi startup (~1s measured on exe.dev).

### `attach` / `detach`

```json
{"id": 5, "type": "harvester", "cmd": {"type": "attach", "sid": "01H..."}}
```

Subscribes this connection to the session's live event stream. A connection may attach to any number of sessions. The daemon guarantee stands as written, but **no client exercises it today**: the gpui client's bridge holds exactly one attachment per machine (one chat pane — `bridge.rs`'s `attached`), detaching from the previous session before attaching to the next. Reconnect/replay is NOT a daemon feature — after attaching, the client sends passthrough `get_entries {since: <last seen entry id>}` to recover history from pi's durable cursor, then renders live events. The verbose delta stream is never replayed.

### `stop`

```json
{"id": 6, "type": "harvester", "cmd": {"type": "stop", "sid": "01H..."}}
```

Non-blocking escalation ladder; responds `{"stopping": true}` immediately:

1. **`abort`** (daemon-injected passthrough) — any in-flight turn is recorded cleanly in the session file instead of dying mid-stream.
2. **stdin EOF** — pi's shutdown path. Source-verified (rpc-mode.js): EOF triggers immediate shutdown, never waits for a running turn, exits 0.
3. **SIGKILL after 30s** — pure hang breaker (wedged runtime, stuck extension `session_shutdown` handler); not expected to fire in normal operation.

`session_exit` is the authoritative completion signal — the `stop` response only means "shutdown initiated", and `session_exit` may arrive before or after it. Session file survives; the session shows as `exited` and can be resumed.

## The `pi` passthrough (`type: "pi"`)

The daemon writes `cmd` to the session's pi stdin (managing pi-level `id` correlation internally) and returns pi's response object verbatim. No translation, no normalization — **the client speaks Pi**. This covers, without any daemon knowledge: `prompt` (incl. `streamingBehavior`), `steer`, `follow_up`, `abort`, `get_state`, `get_messages`, `get_entries`, `get_tree`, `set_model`, `get_available_models`, `set_thinking_level`, `get_session_stats`, `compact`, `set_session_name` (= Harvester's rename), `get_commands`, `bash`, and `extension_ui_response`.

Commands with fire-and-forget semantics on the pi side (e.g. `extension_ui_response`) produce a `pi_response` with `response: null`.

Pi commands the client must never send: `new_session`, `switch_session`, `fork`, `clone` — they swap the session file under a live process. In Harvester, one pi process = one session, always. **The daemon enforces this**, rather than trusting client politeness: `pi_passthrough` refuses all four before injecting the pi-level `id`, answering `{"type": "response", "success": false, "error": "…one pi process = one session file, always…"}` — nothing is written to pi's stdin. "Multiple clients are first-class" means the trusted client is not the only client, and a session file swapped under a running pi is unrecoverable. The denylist is **pi-version-coupled** (pi 0.83.0 `modes/rpc/rpc-types.ts:26,60,61,62`); re-verify the command names on every pi upgrade.

## Events

Every line pi emits on stdout is forwarded verbatim to attached clients, wrapped:

```json
{"type": "event", "sid": "01H...", "event": {"type": "agent_settled"}}
```

Plus two daemon-originated events:

```json
{"type": "session_exit", "sid": "01H...", "exit_code": 0}
{"type": "event_gap", "sid": "01H...", "skipped": 12}
```

`exit_code` is `null` when pi was killed by a signal (e.g. the daemon's SIGKILL after the stop grace period). Exit-cause detail is not in the event — the client fetches `~/.harvester/log/<sid>.stderr.log` over plain SSH (`tail`) when it wants to show a reason.

`event_gap` means the daemon dropped `skipped` events for *this* connection because it was reading too slowly (the per-session ring overflowed). It is a signal, not a recovery: the client must resync with passthrough `get_entries {since: <last seen entry id>}`, which is authoritative. Losing events silently is not an option — a dropped `extension_ui_request` would otherwise leave pi waiting on a dialog the client never saw, and a dropped `agent_settled` would leave a turn spinning forever.

Ordering: events and responses travel independent paths inside the daemon (broadcast vs request), so an attached client may see `session_exit` *before* the `stop` response that caused it (observed live). Don't assume a sequence between the two.

Client-relevant pi semantics (details in `HARNESS_RESEARCH.md`): `agent_settled` (not `agent_end`) means the turn is truly finished; `message_update` deltas re-send whole partial messages; tool executions correlate by `toolCallId`.

## Dialogs (`extension_ui_request`)

Vanilla pi never asks questions — these only occur when extensions are loaded (auto-discovered from `~/.pi/agent/extensions/`, or project `.pi/extensions/` now that we launch with `--approve`). Policy:

- Dialog methods (`select`, `confirm`, `input`, `editor`):
  - ≥1 client attached → forwarded as a normal event; a client answers via passthrough `extension_ui_response`. First answer wins; the daemon drops responses for ids no longer pending. The daemon holds the dialog's envelope until it is answered and **replays every pending dialog to a client that attaches later** — otherwise a handoff between clients (A holds a dialog, B attaches, A leaves) would strand pi on a dialog nobody can see. Clients must therefore treat a dialog id they already know as a duplicate, not a second question. **Answering or cancelling a forwarded dialog is a client obligation whenever the room is non-empty** — the daemon's auto-cancel below is an empty-room safety net only, so an attached client that ignores a dialog leaves pi blocked indefinitely. No client meets that obligation today (`extension_ui_request`/`extension_ui_response` appear nowhere in `crates/harvester` or `crates/harvester-client-core`); the interim fix is decided, see §Decisions on record (C5).
  - 0 clients attached → the daemon immediately writes `{"type": "extension_ui_response", "id": <id>, "cancelled": true}` to pi. To the extension this is identical to a human pressing Escape; the agent never hangs on an empty room.
  - The last client detaching (or disconnecting) with dialogs still pending cancels them all the same way — an empty room never holds a dialog open, whichever order things emptied in.
- Dialogs carrying a `timeout` auto-resolve **inside pi, silently**: pi runs the timer itself, resolves with the default value and emits nothing on stdout (0.83.0 `modes/rpc/rpc-mode.ts:114-119`), then discards any response for that id (`:768-776`). Nothing on the wire announces it, so the daemon arms one timer of its own per timed dialog and **evicts** the remembered entry after `timeout` + a few seconds of slack — evicts, never cancels: pi has already dropped the dialog, so a cancel would answer something that no longer exists. Without the eviction the entry would be stale for the rest of the process's life (replayed to every later attacher, unanswerable, cancelled for nothing at the last detach). The timer holds a weak reference, so it never keeps a session alive past its pi.
  - **Known hole, deliberately open**: an extension can also abort a dialog through an `AbortSignal` (`rpc-mode.ts:107-111`), which strands the daemon's entry in exactly the same way and puts *nothing* on the wire — no id, no timeout, no event. It is undetectable from outside pi, so it is not fixed here. Bounded by the next room-empty drain, as before. Both paths need a third-party extension: no bundled pi 0.83.0 extension passes `timeout` or a `signal` to a dialog.
- Fire-and-forget methods (`notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`): forwarded, never answered.
- All dialog payloads are plain serializable data (pi disables TUI-only UI in RPC mode), so they map directly to gpui modals: `select` → list, `confirm` → yes/no, `input` → text field, `editor` → multiline.

## Bootstrap & upgrade

Implemented in `harvester-client-core::bootstrap_and_connect` (driveable via the `harvester-cli` dev bin). On every connect, over plain SSH (`BatchMode=yes` + `StrictHostKeyChecking=accept-new` — never prompts, first-seen host keys are trusted, changed keys hard-fail):

1. Probe, one round trip: print the remote `$HOME` (the socket forward needs an absolute remote path — ssh does not expand `~` in `-L`), then `~/.harvester/bin/harvesterd version`. Missing or mismatched → pipe the static binary over the same SSH connection into `~/.harvester/bin/harvesterd` (`cat > .next && chmod +x && mv` — atomic swap, no ETXTBSY against a running daemon).
2. Start it detached: `(setsid ~/.harvester/bin/harvesterd < /dev/null >> ~/.harvester/log/harvesterd.log 2>&1 &)`. **harvesterd does not self-daemonize** — this line is the whole daemonization story. Idempotent: **`~/.harvester/harvesterd.lock` is the single-instance lock** — the daemon takes an exclusive non-blocking `flock` on it at startup and exits 1 if another process holds it. The socket is only the rendezvous path: nothing unlinks it on exit, so a stale socket file is the normal case after a crash or a `pkill`, and the daemon that holds the lock unlinks it unconditionally before binding. The lock handle is never released explicitly — the kernel drops it when the process dies, SIGKILL included, so there is no stale-lock state. The parentheses are load-bearing: a bare trailing `&` would background the entire preceding `&&` chain, whose async-list stdin is `/dev/null` per POSIX — which truncates the binary upload to zero bytes.
3. Client forwards the socket (`ssh -N -L`) and sends `ping`; the version is re-checked on every connect. A running daemon that answers with the wrong version (started before an upgrade) is force-upgraded exactly once — `pkill -x harvesterd`, wait for it to die, reinstall, restart, re-ping; a second mismatch is an error (a stale client-shipped binary).

**Upgrading harvesterd kills its pi children** (their stdin pipes close — SIGTERM to the daemon is fine; it is SIGTERM to *pi* that is forbidden). That is acceptable by design: session state is on disk and resume is the same launch line. Upgrade flow: warn while sessions are `running`, stop them, replace the binary, restart. **Resume after upgrade is manual** — sessions show as `exited` and the user resumes the ones they care about (visible, boring).

## What is deliberately NOT in the contract

- **No daemon journal.** Pi's session `.jsonl` + `get_entries {since}` is the persistence and replay layer. (A stdout tee is a one-liner to add later if raw wire logs are ever missed.)
- **No session metadata files.** Names live in pi's `session_info` entries; everything else derives from the session file header.
- **No event normalization.** Clients consume pi's event vocabulary directly.
- **No approval/permission system.** Harvester ships zero pi extensions in v1.
- **No workspace/git/file-picker ops.** Those are plain SSH execs by the client (`ls`, `git rev-parse`), not daemon business.
- **No machine CRUD.** That's the provider contract (`PROVIDERS.md`).
- **Future, not v1**: "Update dependencies" machine action — plain `ssh <machine> 'exeuntu update pi && pi update --all'` from the client, no daemon involvement (`pi update` documentedly never prompts for trust).

## Decisions on record

Settled in the 2026-08-04 backend review; none is built yet. Recorded so the next round starts from the verdict.

- **C1 — the session-swapping denylist is enforced in the daemon**, not by client politeness: `pi_passthrough` rejects `new_session`, `switch_session`, `fork` and `clone` with `success: false` naming the invariant, before the pi-level id is injected. "Multiple clients are first-class" means the trusted client is not the only client, and the failure it prevents — a session file swapped under a live pi — is unrecoverable. The daemon already inspects `type` for the dialog policy, so the precedent for looking at a pi payload exists. *Built (2026-08-04): `SESSION_SWAPPING_COMMANDS` in `passthrough.rs`, rejected before the id injection; see §The `pi` passthrough.*
- **C4 — protocol version and build identity are two things and get split.** `PROTOCOL_VERSION` becomes an integer bumped only when the envelope changes (that is what `ping` exchanges and what `VersionMismatch` tests); build identity becomes separate — harvesterd hashes `/proc/self/exe` at startup, reports it in `PingData`, and the client installs iff it differs from the binary it ships. Answers both real questions (*is the file on disk the one I ship*, *is the running daemon that file*) with no remote dependency. *Decided, not built — see the hazard under §`ping`.*
- **C5 — until dialog modals exist, the client's bridge auto-cancels forwarded dialogs** (passthrough `extension_ui_response` with `cancelled: true`, plus a visible failure note), idempotently, since the daemon replays a pending dialog to every attaching client and the same id arrives twice by design. The obligation it discharges is stated in §Dialogs. Deleted wholesale when the modal slice lands. *Decided, not built.*

Architectural decisions from the same review (bridge reply tokens, the `Fleet` extraction, the client-side transcript replica) are recorded in `SPEC.md` §Decisions on record.

## Open questions

1. ~~`PI_OFFLINE` vs `--approve` package installs~~ **RESOLVED (smoke test 2026-08-01): no install in either mode on exeuntu (no node/npm on image), no hang, exit 0. Launch line stands as-is.**
2. `--session-id` and stdin-close shutdown are source-verified but undocumented pi API — **both verified live on 0.83.0 (smoke test)**; re-verify on every pi version bump (tracked in `HARNESS_RESEARCH.md`).
