# Harvester — Specification

> Status: planning complete + implementation started, 2026-08-01. This is the umbrella spec. Normative details live in the referenced docs; where they conflict, the referenced doc wins.
>
> Implemented so far: `crates/harvester-proto` (wire types), `crates/harvesterd` (daemon: socket loop, session spawn/stop/resume, pi passthrough, dialog policy, list_sessions) — verified end-to-end against a real exe.dev VM over an SSH-forwarded socket, including mid-turn stop and resume (`HARNESS_RESEARCH.md` §E2E log) — `providers/exe-dev` (the reference provider, live-tested), `crates/harvester-client-core` (bootstrap-over-SSH + socket forward + envelope client, live-tested including both upgrade paths) with its `crates/harvester-cli` dev driver, and `crates/harvester` (the gpui client, first vertical slice: one machine from argv, sessions sidebar, attach/resume, prompt, streaming turn; tokio lives on a dedicated bridge thread, the musl daemon is embedded via build.rs with a dev-time disk fallback). `list_models` is implemented (throwaway `PI_OFFLINE=1 pi --mode rpc --no-session` in the requested cwd answering `get_available_models`, data verbatim, 15s backstop, uncached; live-verified: 50 models in ~1.4s on exe.dev). The client also ships `ui.rs` (the design-system primitives) with a live catalog window (`harvester kitchen`) where every component's variants are verified before production use. Workspaces are implemented: client-side store (`~/.harvester/workspaces.json`), sidebar section, and the add-workspace modal (SSH-`ls` walking folder picker + `git rev-parse` validation) over ControlMaster-multiplexed SSH. Ghost chat is implemented: clicking a workspace row opens a client-side-only chat ("New session") whose prompt box carries the model control inline (agent-input pattern; the control opens a modal list fed by `list_models`, fetched on ghost open so the pi startup hides behind typing; no selection = pi's default), and the first prompt mints a uuidv7 sid and rides the bridge's create → attach → prompt chain (no readiness probe) into a real chat seeded with that prompt. Live sessions carry the same prompt-box model control: the label is the session's current model (passthrough `get_state` at open), the list is passthrough `get_available_models` against the running pi (no throwaway), and a pick fires passthrough `set_model` — the label updates only when pi confirms. Thinking-level picker deferred. Providers-in-UI is implemented: provider discovery/exec lives in client-core (`provider.rs`: one-shot `info`/`list` with a 30s kill-timeout, protocol gate, the bundled `exe-dev` script installed into `~/.harvester/providers/` on first run); a dedicated machines worker thread (tokio-on-a-thread, like the bridge) polls provider `list` every 300s and drives the sidebar dots with Harvester's own `ssh <machine> true` probe every 30s; the sidebar's MACHINES section lists `local` + provider machines + ad-hoc SSH destinations as read-only rows (dot + provider/link-state detail line). Bridges are per-machine now — created lazily on first touch, kept for the window's lifetime, events tagged with the machine key on one shared channel — and `harvester` launches with no arguments (an argv destination becomes an ad-hoc machine row; machines with stored workspaces auto-connect in the background so their sessions reappear, with a dim machine suffix on session rows once sessions span machines). Workspaces store machine keys (`<provider>/<id>`; keyless legacy entries stay valid as ad-hoc destinations); workspace rows are read-only except a per-row "+" that opens the ghost chat, and the add-workspace flow starts with a machine choice when more than one qualifies. The ghost's prompt box carries a machine control next to the model control (live only while ≥2 stored workspaces share the path; switching retargets the ghost and refetches models; after the first prompt the chat shows the machine as a read-only control). The `local` machine works end-to-end in code (harvesterd spawned as a detached child, direct Unix-socket connect, local `ls`/`git` for the picker) but has not been click-tested. Machine create/delete UI (provider `create`/`delete` + `create_fields` modal) is deferred to its own slice, as is the `{argv}` ssh escape hatch (such machines list but are marked unconnectable). Tool-call blocks are implemented: `tool_execution_start/update/end` render as collapsible cards in the chat (collapsed one-liner with per-tool label — `$ command` for bash, verb + basename + colored `+N −N` stats for read/write/edit, pattern for grep/find — status dot accent/faint/red, click to expand into a monospace body: context line, then edit's pre-rendered `details.diff` colored per line, write's synthesized all-added body, or the output snapshot tail-capped at 64 KiB), and history replay rebuilds identical blocks from assistant `toolCall` blocks matched to `toolResult` messages (which carry the same `details.diff`). A history call with no result stays "running" — that is also the attach-mid-turn case, and live events find the block by id. Grouping consecutive calls and rich per-tool bodies beyond edit/write are deferred polish. A stabilization pass (2026-08-04) landed the sidebar redesign: collapsible section headers (chevron toggles; the workspaces "+" moved into its header), machine rows reduced to label + source subtitle with the four-color dot carrying all link state (green connected / yellow connecting / red failed-or-unreachable / gray no-daemon-link; `local` has no subtitle), session rows always subtitled `workspace · machine` and opened by sid, the `local` machine's session list scoped to added workspaces (remote machines still list everything), a read-only repository control between the model and machine controls in both prompt boxes, and `StreamLocalBindUnlink=yes` on the ssh socket forward (a respawn racing a dying forward's teardown could hit "Address already in use"). Agreed slice order for what remains: dialogs-as-modals; machine CRUD was split out of the providers slice and is unordered so far.
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

**Status dot**: the machine row's whole story — rows carry no link-state text, only the machine's source (`exe-dev`, `ssh`; `local` has no subtitle). Green = daemon connected (a live bridge is proof, no probe needed); yellow = bridge connecting/bootstrapping; red = connect failed/lost, or Harvester's own probe (`ssh <machine> true`, 30s cadence; provider `list` polls at 300s) says unreachable; gray = no daemon link yet (reachable or not yet probed — indistinguishable without connecting). Never provider-reported status (advisory only). `local` follows the same mapping — gray until its daemon is spawned. Harness readiness: `pi --version` + `pi auth print-bearer-token --model <m>` (never `--list-models`, which exits 0 with no models).

The client also carries **ad-hoc machines** — plain SSH destination strings (the argv launch form, legacy workspaces.json entries) used verbatim, keyed by the string itself. They probe and connect like provider machines; they just come from nowhere but the user.

### Workspace

A git repo on a machine, stored as `machine → path`, displayed as `folder_name in machine_label`. Validation and browsing are plain SSH execs by the client (`ls -1Ap`, `git -C <path> rev-parse --git-dir`) — no daemon or provider involvement. All client SSH connections multiplex via `ControlMaster=auto` (`ControlPath` in the temp dir, `ControlPersist=60`), so a per-click listing skips the handshake and costs about one network round trip (measured on exe.dev: ~1.7s cold → ~0.35s multiplexed); `auto` degrades to a plain connection if the master is unusable. The picker starts at the remote `$HOME` (captured during bootstrap).

The workspace list lives client-side in `~/.harvester/workspaces.json` (plain JSON, `[{machine, path}]`). This is user configuration, not session state — the "client caches nothing authoritative" rule is about session data, whose truth stays on the VM's disk.

### Session

A running (or resumable) `pi --mode rpc` process in a workspace, owned by that machine's harvesterd. Session id = client-generated **uuidv7** (pi's own native id format). One pi process = one session file, always — the session-swapping pi commands (`new_session`, `switch_session`, `fork`, `clone`) are never sent.

The full lifecycle, launch line, envelope, and event semantics are normative in `DAEMON.md`. Summary:

- **Launch**: `PI_OFFLINE=1 GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND="ssh -o BatchMode=yes ..." pi --mode rpc --session-id <sid> --approve [--provider --model --name]`, cwd = workspace, stderr → `~/.harvester/log/<sid>.stderr.log`. `--approve` loads project `.pi/` resources (user's repo, user's call); the git env guards convert would-be credential hangs into fast failures.
- **Resume = create**: same call, same sid (`--session-id` is resume-or-create, source-verified).
- **Stop**: close stdin (never SIGTERM), 30s backstop, SIGKILL, reap.
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

Sidebar: `machines_header/list`, `workspaces_header/list`, `sessions_header/list`. Every section header is a collapse toggle (trailing chevron; the workspaces header carries its "+" before the chevron). Session rows always subtitle `workspace · machine` and navigate by sid — the list is a filtered view: remote machines show every session their daemon lists (the VM's disk is the only source of truth, nothing client-side to scope by), while the `local` machine's sessions are scoped to added workspaces (its daemon would otherwise surface every repo this computer ever ran pi in; the unfiltered list stays in memory, so adding a workspace reveals its sessions without a refetch).

### Rendering pi

The chat renders pi's event stream directly:

- Streaming text/thinking: `message_update` deltas (each carries the whole partial message — replace, don't append).
- Tool calls: `tool_execution_start/update/end` correlated by `toolCallId`; updates carry accumulated snapshots (replace the block). Pi runs tool executions in parallel — several `start`s can precede their `end`s, and ends arrive out of order (verified live, pi 0.83.0). The assistant `message_end` precedes its tool executions, so push order is render order. Edit results carry a pre-rendered numbered diff in `details.diff` (`" 1 ctx" / "-2 old" / "+2 new"` lines); `toolResult` history messages carry the same fields, so replayed blocks equal live ones.
- Turn done: `agent_settled` (not `agent_end` — retries/compaction/queued follow-ups may still run after `agent_end`).
- Busy states: `compaction_start/end`, `auto_retry_start/end`, `queue_update` → statusbar.
- Cost/tokens: `get_session_stats` → statusbar.

Extension dialogs map to native modals: `select` → list, `confirm` → yes/no, `input` → text field, `editor` → multiline; `notify`/`setStatus` → toast/statusbar. All payloads are plain data — pi disables TUI-only UI in RPC mode, so nothing unmappable can arrive.

### Flows

**Add machine** (deferred to the machine-CRUD slice) — "+" in machines_header → provider picker (from discovered providers advertising `create`) → modal generated from the provider's `create_fields` → provider `create` → connectivity assert (`ssh <machine> true`) → store.

**Add workspace** — "+" in workspaces_header → machine choice (a modal list of every connectable machine: `local`, provider machines, ad-hoc destinations; skipped when exactly one qualifies) → folder picker on that machine (`ls` walk starting at its `$HOME` — the picker waits on "connecting…" if the machine's bridge is still bootstrapping) → `git rev-parse` validation (clear error in the modal, re-pick on failure) → store as `{machine key, path}`.

**Start session (ghost chat)** — machine and workspace rows are read-only; the workspace row's trailing "+" opens an empty chat client-side only, titled "New session". The prompt box carries flat controls in its bottom row (agent-input pattern, `model · repository · machine`): the model control (modal picker fed by daemon `list_models`: throwaway `pi --no-session` answering `get_available_models`, fetched when the ghost opens; thinking-level picker deferred), a read-only repository label (the workspace's folder name), and a machine control — live only while ≥2 stored workspaces share the path, listing exactly those machines; switching retargets the ghost (lazy bridge connect) and refetches models. On first prompt: generate uuidv7 sid → `create_session` on the picked machine's bridge (with `name` derived from the prompt itself: first line, truncated) → attach → `prompt`; the machine control becomes a read-only label in the live chat. No readiness probe — pi's stdin queue drains in order (verified, `HARNESS_RESEARCH.md` §Round 4), so commands chained right after `create_session` are safe.

**Switch model (live session)** — the attached chat's prompt box shows the session's current model (passthrough `get_state`, fetched at open together with passthrough `get_available_models` — both are milliseconds against the running pi, no throwaway process). Picking from the same modal list sends passthrough `set_model {provider, modelId}`; on success pi returns the Model now in effect and the label updates (no optimistic update), on failure the statusbar shows pi's error and the session keeps its model. The live picker has no "default" row — a running session always has a concrete model.

**Rename session** — right-click → Rename → inline input → Enter → passthrough `set_session_name`.

**Resume session** — click an `exited` session → `create_session` with the same sid → attach → `get_entries` renders history.

**Reopen client** — machines with stored workspaces connect in the background at startup (each: connect, `ping` — re-bootstrap harvesterd on version mismatch — then `list_sessions`); every other machine connects lazily on first touch, and a failed or lost bridge respawns on the next touch. Re-attach to whatever the user opens. Nothing was lost because nothing lived in the client.

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

- **Client**: Rust + gpui (crates.io `gpui = "0.2"`), crate `crates/harvester` — gpui enters no other crate, same quarantine rule as clap-in-`harvester-cli`. Layout: `main.rs` (argv, keybindings, embedded musl daemon via build.rs), `workbench.rs` (top-level state: sidebar, content, statusbar), `chat.rs` (one session's conversation), `input.rs` (prompt input vendored from gpui's canonical example), `theme.rs` (palette), `ui.rs` (design-system primitives — every reusable visual component lives here), `kitchen.rs` (the component catalog behind `harvester kitchen`: each primitive's variants rendered live, no bridge/daemon). The backbone workers live in `harvester-client-core`, not here: `bridge` (one tokio-runtime thread per machine owning its daemon connection; UI ↔ bridge over reactor-independent tokio mpsc channels; command flows are straight-line futures on a `DaemonClient` pending-map) and `machines` (provider list + probe worker). That split *is* the reusable client — a second frontend (mobile) imports the same two modules.
- **harvesterd**: Rust, static musl binary (`x86_64-unknown-linux-musl`), no runtime deps, uploaded over SSH. Layout: `main.rs` (CLI + socket lock + accept), `daemon.rs` (pid table — the only state), `server.rs` (controller: frame/decode/route/encode), `ops.rs` (one function per command), `passthrough.rs` (pi relay + id correlation), `session.rs` (spawn/pump/dialog policy), `fslist.rs` (session listing).
- **Shared**: `harvester-proto` — serde types for the envelope only (pi payloads stay `serde_json::Value` — pass-through by design), plus the canonical framing helpers (`read_frame`/`encode_frame` — LF-only, strip one CR) next to the `LF`/`CR` constants; every frame in daemon and client goes through them.
- **Providers**: any language, one-shot JSON-stdio executables.
