# Harvester — Specification

> Status: planning complete + implementation started, 2026-08-01. This is the umbrella spec. Normative details live in the referenced docs; where they conflict, the referenced doc wins.
>
> Implemented so far: `crates/harvester-proto` (wire types), `crates/harvesterd` (daemon: socket loop, session spawn/stop/resume, pi passthrough, dialog policy, list_sessions) — verified end-to-end against a real exe.dev VM over an SSH-forwarded socket, including mid-turn stop and resume (`HARNESS_RESEARCH.md` §E2E log) — and `providers/exe-dev` (the reference provider, live-tested). `list_models` is a deliberate stub until the ghost-chat UI exists. Not started: bootstrap-over-SSH flow, gpui client.
>
> - `HARNESS_RESEARCH.md` — verified facts about Pi and exe.dev (every claim sourced or marked UNVERIFIED)
> - `PROVIDERS.md` — machine provider contract (protocol v1)
> - `DAEMON.md` — client ↔ harvesterd contract (protocol v1)
> - `INITIAL_SPEC.md` — original product sketch (superseded by this file where they differ)

## What Harvester is

A Rust + gpui desktop app that orchestrates **Pi** coding-agent sessions on remote machines. Sessions run detached on the machines — the client is a window onto them, not their owner. Close the laptop, the agents keep working; reopen it, everything is still there.

**Scope (locked):** Pi is the only harness. exe.dev is the only built-in infra target (its VMs are always-on). Other platforms are reachable through the provider contract, not through built-in integrations.

## Architecture

```
┌──────────────┐   SSH (socket forward)   ┌────────────────┐   stdin/stdout   ┌──────────────┐
│ gpui client  │ ───────────────────────► │  harvesterd    │ ───────────────► │ pi --mode rpc │  × N sessions
│  (desktop)   │ ◄─────────────────────── │  (per machine) │ ◄─────────────── │  (per session)│
└──────────────┘                          └────────────────┘                  └──────────────┘
       │                                          │                                  │
       │ one-shot exec                            │ spawns/reaps                     │ appends
       ▼                                          ▼                                  ▼
┌──────────────┐                          in-memory only:                    ~/.pi/agent/sessions/
│  providers   │                          pid table, stdin                   <cwd>/<ts>_<sid>.jsonl
│ (~/.harvester│                          handles, attached                  (the ONLY source of truth)
│  /providers) │                          clients
└──────────────┘
```

Design invariants:

- **Disk on the machine is the source of truth.** Pi's own session files (documented, versioned v3, durable `get_entries {since}` cursor) are the persistence layer. harvesterd persists nothing; the client caches nothing authoritative.
- **Each layer dies and recovers independently.** Client crash → sessions unaffected. harvesterd crash/upgrade → pi children exit, state is on disk, resume is one call. pi crash → session file survives, `session_exit` event, resume-or-create relaunch.
- **The client speaks Pi.** No normalized event model, no translation layer. Pi RPC commands and events pass through harvesterd verbatim inside a thin envelope.
- **Multiple clients are first-class.** Desktop and (future) mobile can attach to the same session simultaneously; events broadcast, state recovers via the durable cursor.

## Citizens

### Machine

Where agents run. Keyed `<provider>/<id>` (e.g. `exe-dev/bloggy`). Two kinds:

- **local** — hardcoded, read-only, never deletable. The client spawns harvesterd as a child process and talks over the Unix socket directly, no SSH.
- **provider machines** — enumerated by provider executables in `~/.harvester/providers/` (see `PROVIDERS.md`). Providers answer *what machines exist and how do I SSH into them* plus create/delete; they are control-plane only. Session traffic always flows over plain SSH using the returned connection info (`{host, user, port}` or the `{argv: [...]}` ProxyCommand-style escape hatch).

Harvester ships one default provider, `exe-dev` — a thin wrapper over exe.dev's SSH control plane (`ssh exe.dev ls|new|rm --json`), implemented in the same format as user providers. No privileged code paths.

**Status dot**: driven by Harvester's own probe (`ssh <machine> true`), never by provider-reported status (advisory only). Harness readiness: `pi --version` + `pi auth print-bearer-token --model <m>` (never `--list-models`, which exits 0 with no models).

### Workspace

A git repo on a machine, stored as `machine → path`, displayed as `folder_name in machine_label`. Validation and browsing are plain SSH execs by the client (`ls`, `git -C <path> rev-parse --git-dir`) — no daemon or provider involvement.

### Session

A running (or resumable) `pi --mode rpc` process in a workspace, owned by that machine's harvesterd. Session id = client-generated **uuidv7** (pi's own native id format). One pi process = one session file, always — the session-swapping pi commands (`new_session`, `switch_session`, `fork`, `clone`) are never sent.

The full lifecycle, launch line, envelope, and event semantics are normative in `DAEMON.md`. Summary:

- **Launch**: `PI_OFFLINE=1 GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND="ssh -o BatchMode=yes ..." pi --mode rpc --session-id <sid> --approve [--provider --model --name]`, cwd = workspace, stderr → `~/.harvester/log/<sid>.stderr.log`. `--approve` loads project `.pi/` resources (user's repo, user's call); the git env guards convert would-be credential hangs into fast failures.
- **Resume = create**: same call, same sid (`--session-id` is resume-or-create, source-verified).
- **Stop**: close stdin (never SIGTERM), 5s grace, SIGKILL, reap.
- **Rename**: pi's `set_session_name` — persisted as a `session_info` entry in the session file itself.
- **List**: readdir pi's per-cwd session dirs; header line + last `session_info` entry; `running` if pid alive in daemon table, else `exited`.
- **Reconnect**: attach → `get_entries {since: last-seen-entry-id}` → live events. Deltas are never replayed.

## UI

### Layout

```
{sidebar} {sessions_tabs}
{sidebar} {chat}
{sidebar} {statusbar}
```

Sidebar: `machines_header/list`, `workspaces_header/list`, `sessions_header/list`.

### Rendering pi

The chat renders pi's event stream directly:

- Streaming text/thinking: `message_update` deltas (each carries the whole partial message — replace, don't append).
- Tool calls: `tool_execution_start/update/end` correlated by `toolCallId`; updates carry accumulated snapshots (replace the block).
- Turn done: `agent_settled` (not `agent_end` — retries/compaction/queued follow-ups may still run after `agent_end`).
- Busy states: `compaction_start/end`, `auto_retry_start/end`, `queue_update` → statusbar.
- Cost/tokens: `get_session_stats` → statusbar.

Extension dialogs map to native modals: `select` → list, `confirm` → yes/no, `input` → text field, `editor` → multiline; `notify`/`setStatus` → toast/statusbar. All payloads are plain data — pi disables TUI-only UI in RPC mode, so nothing unmappable can arrive.

### Flows

**Add machine** — "+" in machines_header → provider picker (from discovered providers advertising `create`) → modal generated from the provider's `create_fields` → provider `create` → connectivity assert (`ssh <machine> true`) → store.

**Add workspace** — "+" in workspaces_header → machine dropdown → folder picker (SSH `ls` walk) → `git rev-parse` validation (clear error, re-pick on failure) → store.

**Start session (ghost chat)** — "+" on a workspace → empty chat opens client-side only, with model picker (daemon `list_models`: throwaway `pi --no-session` answering `get_available_models`) and thinking-level picker. On first prompt: generate uuidv7 sid → `create_session` → health-probe `get_state` → `prompt` → session appears in sessions_list with a temporary randomized title.

**Rename session** — right-click → Rename → inline input → Enter → passthrough `set_session_name`.

**Resume session** — click an `exited` session → `create_session` with the same sid → attach → `get_entries` renders history.

**Reopen client** — for each known machine: connect, `ping` (re-bootstrap harvesterd on version mismatch), `list_sessions`, re-attach to whatever the user opens. Nothing was lost because nothing lived in the client.

## Auth & model routing (machine-side)

- Pi resolves credentials as: `--api-key` > `~/.pi/agent/auth.json` > env vars > `models.json` keys. auth.json outranks env vars — beware stale baked-in files.
- **exe.dev VMs: keyless, VERIFIED end-to-end (smoke test 2026-08-01).** The image ships a pi extension (`~/.pi/agent/extensions/exe-dev`) that discovers the VM's LLM-gateway models and registers them in pi (full Claude/GPT/grok/deepseek/kimi catalog, `*@llm` ids, cost metadata included). No keys, no `models.json`. Harvester enables it by pre-seeding `~/.pi/agent/exe-dev-llm-integration.json` = `{"version":1,"useExeIntegration":true}` (otherwise the extension pops a startup `select` dialog and, unanswered, silently skips the integration).
- Fallback for non-exe.dev machines: user's own keys via env or auth.json.

## Non-goals (v1)

- Other harnesses (Claude Code, Codex) — cut.
- Built-in platform integrations beyond exe.dev — the provider contract is the extension point.
- Approval/permission system, Harvester-shipped pi extensions.
- Event normalization, daemon-side journaling, session metadata files.
- Machine mutation (resize/rename), provider push/events, provider health ops.
- Windows (provider discovery relies on the executable bit) — Linux/macOS first.

## Post-v1 ideas (on record, not designed)

- **TypeScript SDK** (`import { provider } from "harvester"`): pre-built APIs to compile spec-compliant provider binaries; derives `capabilities` from supplied handlers. SDK = convenience, stdio contract = spec.
- **Update dependencies** button per machine: `ssh <machine> 'exeuntu update pi && pi update --all'` (`pi update` never prompts for trust). No daemon involvement.
- **Mobile client**: protocol already supports it (multi-client attach + durable cursor); needs only an SSH transport lib.
- Per-workspace trust toggle if `--approve`-by-default ever needs opting out.
- Shared-machine socket auth (token in `ping`) if machines stop being single-user.
- **Pi protocol v2 watch**: upstream is incubating a binary CBOR client↔server protocol (bearer-token hello, authoritative snapshots). If pi ships a server mode, harvesterd shrinks toward a bootstrapper; our pass-through design keeps the client's rendering untouched (`HARNESS_RESEARCH.md` §Watch).

## Open questions

Consolidated from the contract docs; all non-blocking, burn down while building:

1. ~~PI_OFFLINE vs --approve installs~~, ~~gateway~~, ~~VM username~~, ~~host keys~~, ~~stdout purity~~, ~~image pi version~~ — **resolved by the 2026-08-01 smoke test** (`HARNESS_RESEARCH.md` §Smoke-test log). Notable: all exe.dev hosts share ONE RSA host key (same as the control-plane pin) — no per-VM key management.
2. `--session-id` and stdin-close shutdown: verified live on 0.83.0, but undocumented pi API — re-verify per pi upgrade.
3. exe.dev: `--setup-script` execution context; full `ls --json` status value set (`HARNESS_RESEARCH.md` §Open questions).
4. Providers: Windows resolution, `list` cache tuning, `secret` field handling (`PROVIDERS.md` §Open decisions).

## Tech stack

- **Client**: Rust + gpui.
- **harvesterd**: Rust, static musl binary (`x86_64-unknown-linux-musl`), no runtime deps, uploaded over SSH. Layout: `main.rs` (CLI + socket lock + accept), `daemon.rs` (pid table — the only state), `server.rs` (controller: frame/decode/route/encode), `ops.rs` (one function per command), `passthrough.rs` (pi relay + id correlation), `session.rs` (spawn/pump/dialog policy), `fslist.rs` (session listing).
- **Shared**: `harvester-proto` — serde types for the envelope only (pi payloads stay `serde_json::Value` — pass-through by design), plus the `LF`/`CR` framing constants.
- **Providers**: any language, one-shot JSON-stdio executables.
