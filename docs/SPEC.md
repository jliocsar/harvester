# Harvester — Specification

> This is the umbrella spec. Normative details live in the referenced docs; where they conflict, the referenced doc wins.
>
> - `HARNESS_RESEARCH.md` — verified facts about Pi and exe.dev (every claim sourced or marked UNVERIFIED)
> - `PROVIDERS.md` — machine provider contract (protocol v1)
> - `DAEMON.md` — client ↔ harvesterd contract (protocol v1)
> - `INITIAL_SPEC.md` — original product sketch (superseded by this file where they differ)

## Build state

The whole path works end to end: bootstrap a machine over SSH, start a session, prompt it, watch the turn stream, resume it after it exits. What follows is what exists, not what this document specifies — the rest of the file is the spec, and it describes things this table calls deferred.

| Slice | State | Notes |
| --- | --- | --- |
| Wire protocol (`harvester-proto`) | **Built** | Envelope types + framing helpers, shared by both ends |
| harvesterd | **Built** | Socket loop, spawn/stop/resume, passthrough, dialog policy, `list_sessions`, `list_models`. Live-verified against an exe.dev VM incl. mid-turn stop and resume |
| `exe-dev` provider | **Built** | The reference provider, live-tested |
| Bootstrap + connect (`client-core`) | **Built** | SSH bootstrap, socket forward, envelope client; both upgrade paths live-tested |
| `harvester-cli` dev driver | **Built** | |
| Fleet | **Built** | Bridge pool, machine table, probe verdicts, connect policy — one `MachineState` per machine (C3) |
| gpui client core loop | **Built** | Sessions sidebar, attach/resume, prompt, streaming turn |
| Design system (`ui.rs` + `harvester kitchen`) | **Built** | Every primitive's variants verified in the catalog window before production use |
| Workspaces | **Built** | `~/.harvester/workspaces.json`, sidebar section, add-workspace folder picker over multiplexed SSH |
| Chat (unstarted → starting → live) | **Built** | One component, one state (§Flows "Start session") |
| Model control | **Built** | Both states; thinking-level picker deferred |
| Providers in UI | **Built** | Discovery/exec in `provider.rs`, machines worker, `ssh <machine> true` probes, machine rows |
| Tool-call blocks | **Built** | Live + history replay produce identical blocks. Grouping consecutive calls and rich bodies beyond edit/write are deferred polish |
| Transcript replica + `get_entries {since}` | **Built** | Repairs fetch only what was missed (C6) |
| `local` machine | **Built, untested** | Works in code — detached child daemon, direct socket, local `ls`/`git` — but has never been click-tested |
| Dialogs as modals | **Deferred** | Nothing renders `extension_ui_request`; the bridge auto-cancels every forwarded dialog so pi is never left blocked (C5, `DAEMON.md` §Dialogs). **Next slice.** |
| Stop a session | **Deferred** | Complete in harvesterd's `ops.rs`; the bridge has no `Stop` command and nothing sends one |
| Session rename | **Deferred** | No `set_session_name` call, no UI |
| Cost/token statusbar | **Deferred** | `get_session_stats` is never fetched |
| Machine create/delete UI | **Deferred** | Provider `create`/`delete` + a `create_fields` modal; unordered |
| `{argv}` ssh escape hatch | **Deferred** | Such machines list, but are marked unconnectable |
| Harness-readiness probe | **Deferred** | Only `ssh <machine> true` runs — see §Machine |

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

- **Disk on the machine is the source of truth.** Pi's own session files (documented, versioned v3, durable `get_entries {since}` cursor) are the persistence layer. harvesterd persists nothing; the client caches nothing authoritative. A client *may* hold a **repairable, evictable transcript replica** keyed by a pi entry-id cursor — fetch once, extend with `get_entries {since}` — because repair is always available: drop the cursor and refetch. What stays forbidden: the replica is never the answer to "what happened" (any doubt is resolved by refetching, never by reconciling), it never outlives the process that holds it (in-memory, evictable at any time — closing a session may discard it), and a cursor pi rejects (`Entry not found: <since>`, source-verified in `rpc-mode.js`) means refetch from scratch. The session file on the machine remains the only authority.
  - *Built.* The bridge holds the cursor (pi's `leafId`, tagged with the sid it came from, so it cannot be spent on the session that replaced it); the frontend's `Transcript` holds the replica and records how much of it pi confirmed. A repair sends `get_entries {since}` and extends; a delta that fails for any reason falls back to a full refetch, which covers the retired-cursor case without matching on pi's error text. **Live events carry no entry ids**, so items built from them are dropped before a delta is applied — the delta is the authoritative account of the very span they were guessing at.
- **Each layer dies and recovers independently.** Client crash → sessions unaffected. harvesterd crash/upgrade → pi children exit, state is on disk, resume is one call. pi crash → session file survives, `session_exit` event, resume-or-create relaunch.
- **The client speaks Pi.** No normalized event model, no translation layer. Pi RPC commands and events pass through harvesterd verbatim inside a thin envelope.
- **Multiple clients are first-class.** Desktop and (future) mobile can attach to the same session simultaneously; events broadcast, state recovers via the durable cursor.

## Citizens

### Machine

Where agents run. Keyed `<provider>/<id>` (e.g. `exe-dev/bloggy`). Two kinds:

- **local** — hardcoded, read-only, never deletable. The client spawns harvesterd as a child process and talks over the Unix socket directly, no SSH.
- **provider machines** — enumerated by provider executables in `~/.harvester/providers/` (see `PROVIDERS.md`). Providers answer *what machines exist and how do I SSH into them* plus create/delete; they are control-plane only. Session traffic always flows over plain SSH using the returned connection info (`{host, user, port}` or the `{argv: [...]}` ProxyCommand-style escape hatch).

Harvester ships one default provider, `exe-dev` — a thin wrapper over exe.dev's SSH control plane (`ssh exe.dev ls|new|rm --json`), implemented in the same format as user providers. No privileged code paths.

**Status dot**: the machine row's whole story — rows carry no link-state text, only the machine's source (`exe-dev`, `ssh`; `local` has no subtitle). Green = daemon connected (a live bridge is proof, no probe needed); yellow = bridge connecting/bootstrapping; red = connect failed/lost, or Harvester's own probe (`ssh <machine> true`, 30s cadence; provider `list` polls at 300s) says unreachable; gray = no daemon link yet (reachable or not yet probed — indistinguishable without connecting). Never provider-reported status (advisory only). `local` follows the same mapping — gray until its daemon is spawned.

**Harness readiness is specified, not implemented.** The design answer — does this machine have a working pi? — is `pi --version` + `pi auth print-bearer-token --model <m>` (never `--list-models`, which exits 0 with no models). Nothing runs it: the only probe in the client is `ssh <machine> true`, so a reachable machine with no pi, or with pi but no credentials, is indistinguishable from a healthy one until a session fails to start.

The client also carries **ad-hoc machines** — plain SSH destination strings (the argv launch form, legacy workspaces.json entries) used verbatim, keyed by the string itself. They probe and connect like provider machines; they just come from nowhere but the user.

### Workspace

A git repo on a machine, stored as `machine → path`, displayed as `folder_name in machine_label`. Validation and browsing are plain SSH execs by the client (`ls -1Ap`, `git -C <path> rev-parse --git-dir`) — no daemon or provider involvement. Every SSH exec **the client itself** makes multiplexes via `ControlMaster=auto` (`ControlPersist=60`), so a per-click listing skips the handshake and costs about one network round trip (measured on exe.dev: ~1.7s cold → ~0.35s multiplexed); `auto` degrades to a plain connection if the master is unusable. The control socket lives in **`~/.harvester/run`, mode 0700** — never the temp dir: a predictable path in a world-writable directory can be squatted before we bind, and ssh's mux protocol is unauthenticated, so a fake mux server would MITM every exec (the temp dir is used only as a loud fallback when `$HOME` is unusable). Same reasoning as the daemon's socket dir; see `DAEMON.md` §Transport.

The claim stops at the client's own execs. **Providers are separate programs and build their own ssh command lines** — the bundled `exe-dev` sets neither `ControlMaster` nor keepalives, so each 300s `list` cycle pays a cold handshake. That is correct per `PROVIDERS.md`, not an oversight: what a provider does with its control plane is its business, and Harvester cannot multiplex a connection it does not make. The picker starts at the machine's `$HOME`, read by a one-shot exec (`printf '%s\n' "$HOME"`) when the picker opens. All three ops run on the machines worker, keyed by `Host` and correlated by request token — the picker never touches a machine's daemon, so browsing a machine Harvester has never bootstrapped costs one round trip, not an install.

The workspace list lives client-side in `~/.harvester/workspaces.json` (plain JSON, `[{machine, path}]`). This is user configuration, not session state — the "client caches nothing authoritative" rule is about session data, whose truth stays on the VM's disk.

### Session

A running (or resumable) `pi --mode rpc` process in a workspace, owned by that machine's harvesterd. Session id = client-generated **uuidv7** (pi's own native id format). One pi process = one session file, always — the session-swapping pi commands (`new_session`, `switch_session`, `fork`, `clone`) are never sent.

The full lifecycle, launch line, envelope, and event semantics are normative in `DAEMON.md`. Summary:

- **Launch**: `PI_OFFLINE=1 GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND="ssh -o BatchMode=yes ..." pi --mode rpc --session-id <sid> --approve [--provider --model --name]`, cwd = workspace, stderr → `~/.harvester/log/<sid>.stderr.log`. `--approve` loads project `.pi/` resources (user's repo, user's call); the git env guards convert would-be credential hangs into fast failures.
- **Resume = create**: same call, same sid (`--session-id` is resume-or-create, source-verified).
- **Stop**: close stdin (never SIGTERM), 30s backstop, SIGKILL, reap.
- **Rename**: pi's `set_session_name` — persisted as a `session_info` entry in the session file itself.
- **List**: readdir pi's per-cwd session dirs; header line + last `session_info` entry; `running` if pid alive in daemon table, else `exited`.
- **Reconnect**: attach → `get_entries {since: last-seen-entry-id}` → live events. Deltas are never replayed. An *open* still fetches the whole session — there is no replica to repair when the chat is being created — but every `event_gap` resync after it costs only the entries since the last fetch.

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

**Add workspace** — "+" in workspaces_header → machine choice (a modal list of every connectable machine: `local`, provider machines, ad-hoc destinations; skipped when exactly one qualifies) → folder picker on that machine (`ls` walk starting at its `$HOME`, both plain host execs — no bridge, so nothing to wait for beyond the exec itself) → `git rev-parse` validation (clear error in the modal, re-pick on failure) → store as `{machine key, path}`.

**Start session** — machine and workspace rows are read-only; the workspace row's trailing "+" opens a chat with no session behind it yet, titled "New session". This is **the same chat component** a live session uses, in an earlier state — not a separate "ghost" surface. The prompt box carries flat controls in its bottom row (agent-input pattern, `model · repository · machine`): the model control (modal picker fed by daemon `list_models`: throwaway `pi --no-session` answering `get_available_models`, fetched when the chat opens; thinking-level picker deferred), a read-only repository label (the workspace's folder name), and a machine control — live only while ≥2 stored workspaces share the path, listing exactly those machines; switching retargets the chat (lazy bridge connect) and refetches models. On first prompt: generate uuidv7 sid → `create_session` on the picked machine's bridge (with `name` derived from the prompt itself: first line, truncated) → attach → `prompt`. No readiness probe — pi's stdin queue drains in order (verified, `HARNESS_RESEARCH.md` §Round 4), so commands chained right after `create_session` are safe.

The three states are `unstarted → starting → live`, and they are one-way. What differs between them is small and worth naming: whether the machine can still be changed (only unstarted — a running session never moves), and what a model pick *means* (a launch-line argument, versus a `set_model` the running pi must confirm). Everything else — the pane, the prompt box, the input, the model control — is shared, which is why they are one component. The user's first prompt renders immediately and the chat is promoted in place when `Attached` lands; it is never rebuilt, for the same reason a resync does not rebuild it (C7).

**Switch model (live session)** — the attached chat's prompt box shows the session's current model (passthrough `get_state`, fetched at open together with passthrough `get_available_models` — both are milliseconds against the running pi, no throwaway process). Picking from the same modal list sends passthrough `set_model {provider, modelId}`; on success pi returns the Model now in effect and the label updates (no optimistic update), on failure the statusbar shows pi's error and the session keeps its model. The live picker has no "default" row — a running session always has a concrete model.

**Rename session** — right-click → Rename → inline input → Enter → passthrough `set_session_name`.

**Resume session** — click an `exited` session → `create_session` with the same sid → attach → `get_entries` renders history.

**Reopen client** — machines with stored workspaces connect in the background at startup (each: connect, `ping` — re-bootstrap harvesterd on version mismatch — then `list_sessions`); every other machine connects lazily on first touch, and a failed or lost bridge respawns on the next touch. Re-attach to whatever the user opens. Nothing was lost because nothing lived in the client.

## Auth & model routing (machine-side)

- Pi resolves credentials as: `--api-key` > `~/.pi/agent/auth.json` > env vars > `models.json` keys. auth.json outranks env vars — beware stale baked-in files.
- **exe.dev VMs: keyless, VERIFIED end-to-end (smoke test 2026-08-01).** The image ships a pi extension (`~/.pi/agent/extensions/exe-dev`) that discovers the VM's LLM-gateway models and registers them in pi (full Claude/GPT/grok/deepseek/kimi catalog, `*@llm` ids, cost metadata included). No keys, no `models.json`. Enabling it takes a pre-seeded `~/.pi/agent/exe-dev-llm-integration.json` = `{"version":1,"useExeIntegration":true}` (otherwise the extension pops a startup `select` dialog and, unanswered, silently skips the integration). **Implemented in the `exe-dev` provider's `create`** (`seed_llm_integration`) — the provider is the one citizen that knows a machine is an exe.dev VM; the daemon bootstrap must never do it, since its start script also runs for `local` and would write into the user's own `~/.pi`. Idempotent and non-destructive: an existing file is left exactly as it is (and logged when it does not enable the integration), and a seed that cannot reach a still-booting VM is a stderr diagnostic, not a `create` failure. **Scope**: it runs when Harvester creates the VM, so VMs created outside Harvester (including every VM that predates this) still need the file seeded by hand. **UNVERIFIED against a live VM.**
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

## Decisions on record

Settled in the 2026-08-04 backend review, plus the hardening pass that followed it. Each says whether it is built; they are here so the next round starts from the verdict, not from the argument.

- **C2 — worker replies correlate by a monotonic token** (`RequestToken`, minted from one process-wide `RequestTokens`), carried on every command that has an answer and echoed on that answer, replacing the per-command echo-key staleness guards (`cwd`/`path`): those were ad hoc per command and could not tell two in-flight requests with the same key apart. *Built.* Scope as built: commands with no reply (`Open`, `StartSession`, `Prompt`) carry no token — what they produce is the session's own event stream, keyed by sid — and a session-scoped answer still carries its `sid` as *routing* (which chat renders it) alongside the token that correlates it.
- **C3 — the full `Fleet` extraction out of `workbench.rs`**: bridge pool, machine table, probe verdicts, connect policy and machine identity move into client-core behind one owner. The gpui crate is headed for dumb, stateless components, so decomposing `workbench.rs` is a prerequisite, not speculation. *Built* (`client-core/src/fleet.rs`): one `MachineState` per machine replaces the `connections`/`probes` pair, so a probe verdict can no longer contradict a daemon link; `FleetError` carries the human-readable failure reason the statusbar shows, verbatim; every method is synchronous, so gpui's reactor-free executor is never asked to await a tokio primitive.
- **C7 — a repair is not an open.** `bridge::Event::Attached` meant both "you opened this session" and "here is the snapshot repairing what you missed", and the gpui frontend rebuilds the chat entity on every one — so a resync landing after the user had scrolled or typed clobbered the chat on screen. The two are separate events now: `Attached` (create the chat for `sid`; `entries` is pi's list verbatim, empty for a brand-new session) and `HistoryReplaced` (replace an existing chat's entries; ignore it if there is no chat for `sid`). Both carry the same `get_entries` payload — the difference is entirely what a frontend is licensed to do with it. *Built, both halves.* C6 added a third, `HistoryExtended`, under the same rule — a cheaper repair, licensed the same way.
- **C8 — a resync is scoped and deduplicated.** Every daemon `event_gap` used to spawn a full `get_entries`, with `attached` never consulted and no in-flight check: a gap for a session nobody was watching cost a refetch, and N gaps cost N concurrent refetches of one history. The bridge now repairs only the session it is attached to, and only one repair per session at a time (the attach-window overflow retry is a loop under the same claim). *Built.*
- **C9 — an attachment does not outlive its pi.** `BridgeContext.attached` was cleared only by `detach_previous`, so a session whose pi exited stayed recorded and the next `Open` sent a `Detach` for a dead sid — surfacing to the user as a statusbar error for an action they never took. `SessionExit` forgets it, scoped to the sid that exited. *Built.*
- **C6 — clients may keep a repairable transcript replica** (the amendment in Design invariants above), and `get_entries {since}` is implemented properly instead of an always-full refetch. *Built.* The cursor and the replica are owned separately on purpose: the bridge makes the calls, so it holds pi's `leafId`; the frontend holds the items, so it holds the boundary between what pi confirmed and what live events guessed. Neither needs the other's half, and the pairing of cursor-to-sid makes a cursor from a replaced session unusable rather than merely unlikely.

- **C10 — the ghost chat and the live chat are one component.** They were two — `workbench/ghost.rs` and `chat.rs` — each owning an input, a subscription, a focus flag, a model control and a copy of the prompt box, with the workbench arbitrating which one's modal was open in three separate places. Everything the user touches is the same on both sides of the line; what differs is named in the state (`unstarted / starting / live`) and is two facts wide. Folding them removed the duplicate prompt box, the duplicate model-control plumbing, the `GhostRequest` enum and its hand-carried `Command`, and the "whose picker is this?" precedence — and made the first prompt render immediately instead of blanking the pane until `Attached`. *Built.* The workbench now holds one `Option<Entity<Chat>>` and routes to it; `ghost.rs` is deleted.

Protocol-level decisions from the same review — the session-swapping denylist (C1), splitting protocol version from build identity (C4), and the dialog obligation plus interim auto-cancel (C5) — are recorded in `DAEMON.md` §Decisions on record.

## Open questions

Consolidated from the contract docs; all non-blocking, burn down while building:

1. ~~PI_OFFLINE vs --approve installs~~, ~~gateway~~, ~~VM username~~, ~~host keys~~, ~~stdout purity~~, ~~image pi version~~ — **resolved by the 2026-08-01 smoke test** (`HARNESS_RESEARCH.md` §Smoke-test log). Notable: all exe.dev hosts share ONE RSA host key (same as the control-plane pin) — no per-VM key management.
2. `--session-id` and stdin-close shutdown: verified live on 0.83.0, but undocumented pi API — re-verify per pi upgrade.
3. exe.dev: `--setup-script` execution context; full `ls --json` status value set — still open (`HARNESS_RESEARCH.md` §Open questions 4 and 6; only `running` has been observed).
4. Providers: Windows resolution and `secret` field handling are still open; ~~`list` cache tuning~~ — **resolved in the client** (`list` polls every 300s, the SSH probe every 30s, last listing displayed between polls; `PROVIDERS.md` §Open decisions 2). Not contract, by design.

## Tech stack

- **Client**: Rust + gpui (crates.io `gpui = "0.2"`), crate `crates/harvester` — gpui enters no other crate, same quarantine rule as clap-in-`harvester-cli`. Layout: `main.rs` (argv, keybindings, embedded musl daemon via build.rs), `workbench.rs` (top-level state: sidebar, content, statusbar) with `workbench/picker.rs` (the add-workspace folder picker: its modal state, host ops and rendering), `chat.rs` (one session's conversation, from before it exists until after its pi exits — see C10), `input.rs` (prompt input vendored from gpui's canonical example), `models.rs`/`model_control.rs` (the model domain types and the prompt box's model control), `workspaces.rs` (the workspace store and what a workspace is called on screen), `theme.rs` (palette), `ui.rs` (design-system primitives — every reusable visual component lives here), `kitchen.rs` (the component catalog behind `harvester kitchen`: each primitive's variants rendered live, no bridge/daemon). The backbone workers live in `harvester-client-core`, not here: `bridge` (one tokio-runtime thread per machine owning its daemon connection; UI ↔ bridge over reactor-independent tokio mpsc channels; command flows are straight-line futures on a `DaemonClient` pending-map — split into `bridge/mod.rs` for the Command/Event contract and the select loop, `flows.rs`, `attach_window.rs` and the interim `dialog.rs`) and `machines` (provider list, probe worker, and the daemon-free host ops the folder picker runs). Both workers get their runtime from one place, `tokio_thread.rs`, whose whole reason to exist is that the two disagree only about how a failure to build one is reported. `transcript.rs` is the client's model of a conversation — pi's wire shapes pinned to 0.83.0 and testable without a window. Machine identity is client-core's too: `MachineRef::{Local, Provider, AdHoc}` is the one place that decides what a stored machine string means. Above both sits `fleet` — the bridge pool, the machine table, the probe verdicts and the connect policy behind one owner (C3, built). A machine's reachability is one `MachineState`, not a link state beside a probe verdict, so the two cannot disagree; `workbench.rs`'s `machine_dot` is now a single match over that one value. A second frontend gets all of it and would still write for itself: the newest-first session list merged across machines, and every rendering decision.
- **harvesterd**: Rust, static musl binary (`x86_64-unknown-linux-musl`), no runtime deps, uploaded over SSH. Layout: `main.rs` (CLI + socket lock + accept + this binary's build fingerprint), `paths.rs` (every path derived from `$HOME`, one owner), `daemon.rs` (pid table — the only state), `server.rs` (controller: frame/decode/route/encode), `ops.rs` (one function per command), `passthrough.rs` (pi relay + id correlation), `session.rs` (spawn/pump) with `session/dialog.rs` (the empty-room dialog policy) and `session/launch.rs` (the pi launch line — the one place to re-verify on a pi upgrade), `fslist.rs` (session listing).
- **Shared**: `harvester-proto` — serde types for the envelope only (pi payloads stay `serde_json::Value` — pass-through by design), plus the canonical framing helpers (`read_frame`/`encode_frame` — LF-only, strip one CR) next to the `LF`/`CR` constants; every frame in daemon and client goes through them.
- **Providers**: any language, one-shot JSON-stdio executables.
