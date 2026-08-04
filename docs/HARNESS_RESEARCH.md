# Harness & Infra Research — Source of Truth

> **Scope (revised 2026-07-31).** Harvester targets exactly one harness — **Pi** — and one infra platform — **exe.dev**. Claude Code / Codex research was cut (recoverable from the session transcript if ever needed).
> Every claim is either (a) quoted from official docs with a URL, (b) verified against source at a pinned commit, or (c) explicitly labelled `UNVERIFIED` / `NOT DOCUMENTED`.
>
> Pins: Pi source `github.com/earendil-works/pi` @ `977ec833bbb86e245057e9162dbc1443c7b6e707` (2026-07-31, `@earendil-works/pi-coding-agent@0.83.0`; dev mirror `badlogic/pi-mono`). exe.dev docs fetched live 2026-07-31.

---

## Pi (`pi`)

Docs: <https://pi.dev/docs/latest/rpc> · <https://pi.dev/docs/latest/json> · <https://pi.dev/docs/latest/usage> · <https://pi.dev/docs/latest/session-format> · <https://pi.dev/docs/latest/extensions> · <https://pi.dev/docs/latest/providers> · <https://pi.dev/docs/latest/models>

### Launch line

```bash
pi --mode rpc \
  --session-id "$HARVESTER_SESSION_ID" \
  --session-dir /work/repo/.harvester/sessions \
  --approve \
  --provider anthropic --model sonnet \
  1> /var/harvester/$SID/events.jsonl \
  2> /var/harvester/$SID/stderr.log
# stdin MUST stay open for the process lifetime
```

Four modes (`main.ts:109-120`, verified):

```js
function resolveAppMode(parsed, stdinIsTTY, stdoutIsTTY) {
  if (parsed.mode === "rpc")  return "rpc";
  if (parsed.mode === "json") return "json";
  if (parsed.print || !stdinIsTTY || !stdoutIsTTY) return "print";
  return "interactive";
}
```

**With no TTY on either side, `pi "prompt"` silently becomes a one-shot print run.** Always pass `--mode json` or `--mode rpc` explicitly.

`--mode json` and `-p` are **single-shot**; only `--mode rpc` accepts follow-ups. (Multiple positional messages run sequentially in one process/session: `print-mode.ts:121-127`.)

### stdout purity and framing

`main.ts:593`: `shouldTakeOverStdout = appMode !== "interactive" && !isPlainRuntimeMetadataCommand(parsed)` → `takeOverStdout()` rebinds `process.stdout.write` to write to **stderr**; only the protocol serializer uses the saved raw handle. Stray `console.log` from an extension cannot corrupt the stream. **Tee stdout to the JSONL file; capture stderr separately** (diagnostics, `Extension error (<path>): …` in print/JSON mode, trust warnings, the `--session-id` "creating a new session" warning).

Framing rule, documented verbatim: *"RPC mode uses strict JSONL semantics with LF (`\n`) as the only record delimiter. … Node `readline` is not protocol-compliant for RPC mode because it also splits on `U+2028` and `U+2029`, which are valid inside JSON strings."* A Rust splitter on `b'\n'` (strip trailing `\r`) is correct by construction. RPC applies stdout backpressure between records (`waitForRawStdoutBackpressure`), so a slow reader throttles the agent rather than losing data.

### Session identity

- `--mode json`: first stdout line is `JSON.stringify(sessionManager.getHeader())` (guarded by `if (header)`), emitted before any agent event:
  ```json
  {"type":"session","version":3,"id":"uuid","timestamp":"…","cwd":"/path"}
  ```
  On `pi -c --mode json` this is the **resumed** session's header. It does **not** contain the file path.
- `--mode rpc`: **no header line is emitted.** Call `{"type":"get_state"}`:
  ```json
  {"type":"response","command":"get_state","success":true,
   "data":{"model":{},"thinkingLevel":"medium","isStreaming":false,"isCompacting":false,
           "steeringMode":"all","followUpMode":"one-at-a-time",
           "sessionFile":"/path/to/session.jsonl","sessionId":"abc123","sessionName":"my-feature-work",
           "autoCompactionEnabled":true,"messageCount":5,"pendingMessageCount":0}}
  ```
- Best option: **pre-assign with `--session-id <id>`** (`args.ts:267`: "Use exact project session ID, creating it if missing"; validated `/^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$/`). Conflicts (hard `exit(1)`) with `--session`, `--continue`, `--resume`. Documented only in `pi --help` and the CHANGELOG, **not** on pi.dev — treat as semi-public API.

### Resume

`pi --mode rpc --session-id <id>` reopens the exact session (creating if missing). Alternatives: `-c`/`--continue` (most recent for cwd), `--session <path|partial-uuid>`, `--fork`. `-r`/`--resume` is an interactive picker — **never headless**.

### Event stream

Command → `{"id":"req-1","type":"response","command":"prompt","success":true}` means **accepted/queued, not completed** — "Failures after acceptance are reported through the normal event and message stream, not as a second `response` for the same request id."

Events (`AgentEvent` + `AgentSessionEvent`): `agent_start`, `agent_end` (`{messages, willRetry}`), `agent_settled`, `turn_start`, `turn_end` (`{message, toolResults}`), `message_start`, `message_update`, `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `queue_update`, `compaction_start`, `compaction_end`, `auto_retry_start`, `auto_retry_end`, `summarization_retry_scheduled`, `summarization_retry_attempt_start`, `summarization_retry_finished`, `bash_execution_update`, `extension_error`.

**Use `agent_settled`, not `agent_end`, as the "turn finished" signal** (no retry pending, no compaction retry, no queued follow-up). `agent_end.willRetry` and `auto_retry_*` / `compaction_*` drive "still working" spinner states.

Undocumented-but-emitted on the same stream (verified in `agent-session.ts`): `agent_settled` (line 146, emitted 584-585), `session_info_changed` `{name}` (154, emitted 2875), `thinking_level_changed` `{level}` (155, emitted 1692), `bash_execution_update` (181, emitted 2786 — RPC `bash` command only, not LLM tool calls). None of these are in `json.md`/`rpc.md`'s tables.

**`entry_appended` is NOT a general entry feed**: its only emission site is `agent-session.ts:2381`, inside the extension API `appendEntry: (customType, data) => …` at 2377. It fires only when an extension calls `pi.appendEntry()`. Do not use it as a live cursor into the session tree.

Assistant deltas via `message_update.assistantMessageEvent`: `start`, `text_start|text_delta|text_end`, `thinking_start|thinking_delta|thinking_end`, `toolcall_start|toolcall_delta|toolcall_end`, `done` (`reason: "stop"|"length"|"toolUse"`), `error` (`reason: "aborted"|"error"`).

```json
{"type":"message_update","message":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello","partial":{}}}
```

⚠️ **Every delta re-sends the whole `partial` AssistantMessage *and* the whole `message`.** The stream is very verbose — budget disk and parse cost accordingly.

### Tool calls

```json
{"type":"tool_execution_start","toolCallId":"call_abc123","toolName":"bash","args":{"command":"ls -la"}}
{"type":"tool_execution_update","toolCallId":"call_abc123","toolName":"bash","args":{"command":"ls -la"},
 "partialResult":{"content":[{"type":"text","text":"partial output so far..."}],"details":{"truncation":null,"fullOutputPath":null}}}
{"type":"tool_execution_end","toolCallId":"call_abc123","toolName":"bash",
 "result":{"content":[{"type":"text","text":"total 48\n..."}],"details":{}},"isError":false}
```

`partialResult` is **accumulated, not a delta** — "allowing clients to simply replace their display on each update."

**Ordering caveat (parallel tools, the default):** `tool_execution_start` in assistant source order; `tool_execution_update` may **interleave across tools**; `tool_execution_end` in **completion** order; final `toolResult` message events in **source** order. Key UI components by `toolCallId`, never by arrival order.

Built-in tools and their arg/detail shapes (source-verified, useful for typed Rust components):

| tool | args | result `details` |
|---|---|---|
| `bash` | `{command, timeout?}` (seconds) | `{truncation?, fullOutputPath?}` |
| `read` | `{path, offset?, limit?}` | `{truncation?}` |
| `write` | `{path, content}` | — |
| `edit` | `{path, edits:[{oldText,newText}]}` | `{diff, patch, firstChangedLine?}` — **`patch` is a unified diff, render straight from it** |
| `grep` | `{pattern, path?, glob?, ignoreCase?, literal?, context?, limit?}` | — |
| `find` | `{pattern, path?, limit?}` | `{truncation?, resultLimitReached?}` |
| `ls` | `{path?, limit?}` | `{truncation?, entryLimitReached?}` |

Default active set is `read`, `write`, `edit`, `bash`; `grep`/`find`/`ls` are opt-in via tool options. **Tool names are not a closed set** — extensions can `pi.registerTool` and can override built-ins by re-registering the same name. Render unknown tools generically.

Streamed tool input: `toolcall_delta.delta` is a raw JSON fragment, but the accumulated block is **incrementally parsed** (`anthropic-messages.ts:653-665`: `block.partialJson += …; block.arguments = parseStreamingJson(block.partialJson)`), so `partial.content[contentIndex].arguments` is a best-effort object usable for optimistic UI. `partialJson` leaks into the serialized JSON but is not in the public `ToolCall` interface — **do not depend on it.**

### Approval

There is none, by design: *"It intentionally does not include built-in MCP, sub-agents, permission popups, plan mode, to-dos, or background bash"* and *"Built-in tools can read files, write files, edit files, and run shell commands with the permissions of the pi process."* Isolation is the VM's job — which matches exe.dev exactly (their agent-security editorial is literally about VM containment).

The only stdin obligation beyond commands is the extension-UI channel in RPC mode:

```json
{"type":"extension_ui_request","id":"uuid-1","method":"select","title":"Allow dangerous command?","options":["Allow","Block"],"timeout":10000}
{"type":"extension_ui_response","id":"uuid-1","value":"Allow"}
```
```json
{"type":"extension_ui_request","id":"uuid-2","method":"confirm","title":"Clear session?","message":"All messages will be lost.","timeout":5000}
{"type":"extension_ui_response","id":"uuid-2","confirmed":true}
{"type":"extension_ui_response","id":"uuid-3","cancelled":true}
```

Dialog methods (`select`, `confirm`, `input`, `editor`) **block until the client replies**. With `timeout` (ms) the agent auto-resolves; **with no `timeout` the promise waits indefinitely** (`rpc-mode.ts:89-119`) — Harvester MUST answer or `cancelled: true` every non-fire-and-forget request or the agent hangs. Fire-and-forget: `notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`.

In `--mode json`/`-p`, `ctx.hasUI = false` and all UI methods are no-ops (`runner.ts:235-264` `noOpUIContext`: `confirm → false`, `select/input/editor → undefined`) — an approval extension is effectively auto-denied without blocking.

**This is also the cleanest lever for Harvester's "render a dedicated UI component" requirement**: a `tool_call` extension can `ctx.ui.select(...)` and gpui renders it natively.

### Project trust — the silent headless footgun

> "Non-interactive modes (`-p`, `--mode json`, and `--mode rpc`) do not show a trust prompt. Without an applicable saved trust decision, they use `defaultProjectTrust` from global settings: `ask` (default) and `never` ignore those project resources, while `always` trusts them."

**By default a headless run silently ignores `.pi/settings.json`, `.pi/extensions/`, project packages, and project skills.** Fix per-run with `--approve`/`-a`, or persist via `~/.pi/agent/trust.json` / `"defaultProjectTrust": "always"`.

### Session storage

`~/.pi/agent/sessions/--<enc-cwd>--/<ISO-ts-with-:.→->_<uuidv7>.jsonl`, where the encoding is (`session-manager.ts:479`, verified):

```js
const safePath = `--${resolvedCwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`;
```

Note it strips the leading separator and also replaces `:` — the prose doc ("working directory with `/` replaced by `-`") is imprecise. Session IDs are uuidv7, so filenames sort chronologically.

Format is a **tree**, not a linear log: line 1 is the header (no `id`/`parentId`); every subsequent entry has `{type, id (8-char hex), parentId, timestamp}` + payload. Entry types: `message`, `model_change`, `thinking_level_change`, `compaction`, `branch_summary`, `custom`, `custom_message`, `label`, `session_info`.

**The session `.jsonl` is a first-class, documented, versioned format (v3).** Unlike other harnesses, Harvester may read it directly — it can even *be* the persistence layer instead of a tee'd wire log.

Enumeration: `readdir` the per-cwd dir, sorted by file mtime desc. **If you set a custom `--session-dir`, all projects share one flat dir** and you must filter on the header's `cwd` field yourself. Overrides precedence: `--session-dir` > `PI_CODING_AGENT_SESSION_DIR` > `settings.json sessionDir`.

Durable replay cursor (RPC only): `{"type":"get_entries","since":"<entryId>"}` → `{entries[], leafId}`. Documented: *"an entry id works as a durable cursor … even across client restarts,"* and unlike `get_messages` it includes pre-compaction history and abandoned branches. `success:false` if `since` matches no entry. `{"type":"get_tree"}` returns the full tree with labels. **`--mode json` has no replay** — read the `.jsonl` directly.

The LLM-callable bash tool receives `PI_SESSION_ID`, `PI_SESSION_FILE` (absolute, unset for ephemeral), `PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL`. Every pi process exports `PI_CODING_AGENT=true`.

### Auth

Provider env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …) or `~/.pi/agent/auth.json` (0600). **`auth.json` outranks env vars** — a stale baked-in `auth.json` shadows an injected key. OAuth/subscription creds land in the same file and auto-refresh, so copying it is the documented cross-machine path. `/login` is TUI-only; **no documented headless login**.

Custom providers / gateway routing via `~/.pi/agent/models.json` (hot-reloaded):

```json
{"providers": {"anthropic": {"baseUrl": "https://my-proxy.example.com/v1"}}}
```

*"All built-in Anthropic models remain available. Existing OAuth or API key auth continues to work."* Provider fields: `baseUrl`, `api` (`anthropic-messages`, `openai-completions`, …), `apiKey` (literal, `$ENV_VAR`, or `!command`), `headers`, `authHeader`, `models[]`. → This is the hook for exe.dev's LLM gateway (below).

### Lifecycle

- **RPC lifetime is bound to stdin.** `process.stdin.on("end", () => shutdown())` (`rpc-mode.ts:802`); the mode otherwise returns `new Promise(() => {})`. Redirecting stdin from `/dev/null` makes pi exit immediately. Keep a live pipe (or a held-open FIFO).
- Signals: `SIGTERM` → exit **143**, `SIGHUP` → exit **129** (`rpc-mode.ts:366-374`, `print-mode.ts:47-63`). RPC's shutdown **skips the final stdout flush when the signal is SIGTERM** (`rpc-mode.ts:736`) — expect possible truncation of the last line. Prefer closing stdin.
- Print/JSON mode merges piped stdin into the prompt — feed `< /dev/null` if you don't want that.
- No documented RPC `quit`/`shutdown` command.
- `@file` args are rejected in RPC mode (`main.ts:598`, exit 1).
- `PI_OFFLINE=1` / `--offline` disables all startup network ops (update checks, package updates, telemetry).

### Healthcheck

```bash
pi --version
pi auth print-bearer-token --model <model> [--provider <p>] [--min-expiry 30m]   # exit 1 + stderr if no usable credential
pi --mode json -p "ping"
```

**Do not use `pi --list-models` as a pass/fail probe** — it prints a friendly "no models available" message and still exits **0** (`cli/list-models.ts:36-39`). In every non-interactive mode, if no model resolves, pi prints to stderr and `exit(1)` (`main.ts:852`).

---

## exe.dev

Docs: <https://exe.dev/llms.txt> · <https://exe.dev/docs/api.md> · <https://exe.dev/docs/cli-new.md> · <https://exe.dev/docs/customization.md> · <https://exe.dev/docs/use-case-agent.md> · <https://exe.dev/docs/integrations-llm.md> · <https://exe.dev/docs/shelley/llm-gateway.md> · <https://exe.dev/docs/upgrade-codex.md> · <https://exe.dev/docs/faq/how-exedev-works.md>

### What it is

Persistent Linux VMs ("Build apps or SSH into a persistent Linux VM"). Cloud Hypervisor on bare metal; VMs boot from a container image (default **`exeuntu`**) — *"creating a new VM takes about two seconds."* Persistent disks. No public IPs: HTTPS is terminated and proxied by the platform; SSH routes through `vmname.exe.xyz`. Trade-off: *"you don't get to choose which kernel you're using."*

### Control plane = SSH. There is no separate API server to integrate.

Every management command is `ssh exe.dev <cmd>`, and `--json` gives structured output:

```bash
ssh exe.dev ls --json         # list VMs
ssh exe.dev new --json        # create VM (~2s)
ssh exe.dev rm / restart / rename / tag / cp / resize / stat ...
```

Observed `ls --json` / `new --json` fields per VM:

```json
{"https_url":"https://bloggy.exe.xyz","region":"lon","region_display":"London, UK",
 "ssh_dest":"bloggy.exe.xyz","status":"running","vm_name":"bloggy"}
```

Full `status` value set is **NOT DOCUMENTED** (only `"running"` observed in examples). An HTTPS API also exists (`https-api.md`, SSH-signed local key creation) if we ever want machine CRUD without shelling out — not needed for v1.

**This maps 1:1 onto Harvester's Machine flows**: add machine = `ssh exe.dev new --json --name <label>`; machines list + healthcheck dot = `ssh exe.dev ls --json` (+ a direct `ssh <vm> true` probe); rename = `ssh exe.dev rename`.

### VM creation (`new`) — the provisioning surface

Flags: `--name`, `--cpu` (default 2), `--memory`, `--disk`, `--tag` (repeatable), `--comment` (≤200 bytes), `--image` (custom container image; `LABEL exe.dev/install-shelley=true` controls agent install), `--registry-auth`, `--env KEY=VALUE` (repeatable), `--setup-script` (**runs on first boot, max 10KiB**; pipe with `cat setup.sh | ssh exe.dev new --setup-script /dev/stdin`), `--integration`, `--prompt`, `--no-email`, `--json`.

A default setup script for **all** future VMs can be persisted client-side: `defaults write dev.exe new.setup-script` / `defaults delete dev.exe new.setup-script`.

### Coding agents are pre-installed — the provisioning rule is already satisfied

> "claude, codex, and pi are pre-installed, as well as the Shelley agent" — `use-case-agent.md`

Updating on long-lived VMs (`upgrade-codex.md`):

```bash
exeuntu update pi                         # pi lives in ~/.local — run as the pi user, NO sudo
exeuntu update pi --version <x.y.z>       # pin
sudo -u exedev exeuntu update pi --home /home/exedev   # for another user
```

(claude/codex live in `/usr/local/bin` and need `sudo` — irrelevant now, noted for completeness.) The doc's `sudo -u exedev … --home /home/exedev` example implies **`exedev` is the standard VM user** — verify.

### SSH access to VMs

- Syntax: `ssh [-l user] [user@]vmname [command...]` — one-off non-interactive commands are supported.
- `ssh_dest` from `ls --json` is `vmname.exe.xyz`.
- Auth: SSH keys, one keypair per account. Recommended client config:
  ```
  Host exe.dev *.exe.xyz
    IdentitiesOnly yes
    IdentityFile ~/.ssh/id_ed25519_exe
  ```
- Host key pinning for the control plane: `SHA256:JJOP/lwiBGOMilfONPWZCXUrfK154cnJFXcqlsi6lPo`. **Per-VM host key story is NOT DOCUMENTED** — verify what `vmname.exe.xyz` presents.
- `scp` works for file transfer (`faq/copy-files.md`).

### LLM Gateway — Pi auth without putting keys on VMs

- New accounts get a default **`llm` integration attached to `auto:all`** (all VMs).
- Inside an attached VM the gateway is at **`https://llm.int.exe.xyz`** (low-level: `http://169.254.169.254/gateway/llm/<provider>`). *"No API keys are necessary; the gateway authenticates the VM."*
- Managed access to **Anthropic, OpenAI, and Fireworks** models; billed against the subscription's monthly token allocation (top-ups purchasable). Alternatively the integration can hold **your own provider API key** (stored integration-side, *"inaccessible to the VM"*) — keys never touch VM disk either way.
- Documented recipes exist for Claude Code (`ANTHROPIC_BASE_URL=https://llm.int.exe.xyz` + placeholder `ANTHROPIC_API_KEY=implicit`) and Codex; **pi is not mentioned**. Pi's `models.json` `baseUrl` override (see Pi §Auth) should slot in identically:
  ```json
  {"providers": {"anthropic": {"baseUrl": "https://llm.int.exe.xyz", "apiKey": "implicit"}}}
  ```
  **UNVERIFIED — needs a hands-on test.** Fallback that definitely works: `ssh exe.dev new --env ANTHROPIC_API_KEY=…` or write `~/.pi/agent/auth.json`.
- Attach syntax: `ssh exe.dev integrations attach <name> vm:<vm>|tag:<tag>|auto:all`.

### Gaps in exe.dev docs (verify empirically)

The docs nowhere state whether VMs **sleep/suspend when idle** or whether **detached processes keep running with no SSH session connected**. The marketing framing ("persistent VM", VPS alternative, serverful editorial, GitHub Actions runner use case) strongly implies always-on, but it is not written down. **This is the single most important thing to test before freezing the architecture** — Harvester's whole detached-session model depends on it.

---

## Design implications for Harvester (Pi + exe.dev only)

Cutting to one harness and one platform deletes most of the earlier abstraction burden: no `HarnessAdapter` trait over contradictory stdin policies, no normalized event model, no approval-transport enum, no per-platform transport plugins. What remains:

1. **One spawn contract.** `pi --mode rpc --session-id <id>` per session, stdin held open for its lifetime, stdout tee'd to a JSONL (or rely on pi's own session file). Terminate by closing stdin (SIGTERM may truncate the last stdout line).
2. **DECIDED: `harvesterd`** (tiny daemon, bootstrapped over SSH). Pi RPC dies when stdin closes, so a session cannot live inside an SSH channel owned by the client — the daemon owns pi children's stdin pipes, broadcasts events to attached clients, passes `get_state`/`get_entries` through to pi, answers/forwards `extension_ui_request`, and serves reconnecting clients. (This point is a dated snapshot of the decision; the shipped daemon journals nothing — pi's session files are the only durable state, see `DAEMON.md` §What is deliberately NOT in the contract.) One static musl binary uploaded over SSH on first connect.
3. **Persistence layer may come for free.** Pi's session `.jsonl` is documented, versioned (v3), and has a durable cursor API (`get_entries {since}`). Harvester's "source of truth on disk" can literally be pi's own session dir + a small `meta.json` per session for Harvester-level metadata (title, workspace ref). The tee'd `events.jsonl` is then just a raw wire log for live replay, or dropped entirely.
4. **Machine CRUD goes through the provider contract** (see `PROVIDERS.md`): one-shot JSON-stdio executables in `~/.harvester/providers/`. The built-in `exe-dev` provider wraps `ssh exe.dev … --json`; session traffic stays on plain SSH (`vmname.exe.xyz`) regardless of provider. "local" machine remains a hardcoded direct-spawn special case, not a provider.
5. **Workspace flow**: folder picker = `ssh vmname ls`-style execs (or SFTP); git validation = `ssh vmname 'git -C <path> rev-parse --git-dir'`.
6. **Approvals/interactivity**: none natively; if/when Harvester wants tool gating or richer UI hooks, it ships a Pi **extension** (TypeScript) and answers `extension_ui_request` over the RPC channel — every non-fire-and-forget request MUST be answered or the agent hangs. Extension loading in headless mode requires `--approve` / trust config (project-trust footgun above).
7. **Provisioning rule**: nothing to install — `pi` ships on `exeuntu`. Machine-create just runs `exeuntu update pi` (optionally `--version` pinned) in `--setup-script`, plus dropping `models.json`/auth material.
8. **Healthchecks**: machine dot = `ssh exe.dev ls --json` status + `ssh vmname true`; harness readiness = `pi --version` + `pi auth print-bearer-token --model <m>`.

---

## Open questions — needs hands-on testing

### exe.dev

1. ~~Do detached processes survive with no SSH session connected?~~ **RESOLVED (operator knowledge, 2026-08-01): exe.dev VMs are always up; there is no idle sleep/suspend.** Detached processes survive.
2. ~~Default VM username and sudo policy~~ **RESOLVED (smoke test): user `exedev`, passwordless sudo.**
3. ~~Per-VM SSH host keys~~ **RESOLVED (smoke test): there are no per-VM keys — every VM presents the SAME shared RSA key as the control plane (`SHA256:JJOP/lwiBGOMilfONPWZCXUrfK154cnJFXcqlsi6lPo`), stable across `restart`. SSH terminates at exe.dev's edge; one pin covers everything.**
4. Full `status` value set in `ls --json`. **PARTIAL: `running` observed; `new --json`/`rm` output shapes captured in smoke test.**
5. ~~Gateway + models.json~~ **RESOLVED (smoke test), better than hoped: no `models.json` needed. The image ships a pi extension (`~/.pi/agent/extensions/exe-dev`) that discovers gateway models from `https://reflection.int.exe.xyz/integrations` and registers providers (`exe-dev-openai` etc., baseUrl `https://llm.int.exe.xyz/...`). Keyless end-to-end prompt verified (PONG via `gpt-5.6-sol@llm`; catalog includes full Claude/GPT/grok/deepseek/kimi lines). Enablement: pre-seed `~/.pi/agent/exe-dev-llm-integration.json` = `{"version":1,"useExeIntegration":true}` — otherwise the extension pops a `select` dialog at startup (observed live in RPC mode; unanswered → integration silently skipped).**
6. `--setup-script` execution context: which user, env, cwd; is failure surfaced in `new --json`?
7. ~~Which pi version ships on the image~~ **RESOLVED (smoke test): pi 0.83.0 at `/usr/local/bin/pi` (standalone binary — no node/npm on the image), x86_64 Ubuntu 24.04. Image also pre-installs `~/.pi/agent/bin/fd` and the `exe-dev` extension.**

### Pi

8. Does `pi --mode json` accept `-c` / `--session` / `--session-id`? Source says yes (session resolution precedes mode dispatch); docs' option lists omit them. Smoke-test.
9. Stability of `--session-id` as public API — in `--help` and CHANGELOG, not on pi.dev. **Behavior VERIFIED live on 0.83.0 (smoke test): create emits a stderr warning + creates with that id; re-launch with same id resumes silently (no warning), `get_state.sessionId` matches, filename is `<ts>_<custom-id>.jsonl`. Re-verify per pi upgrade.**
10. `--session <partial-id>` matching a different project may hang headless (source shows a `promptConfirm` with no answer path). **Always pass an absolute path or `--session-id`.**
11. Is the `--mode json` header line emitted under `--no-session`?
12. Are `agent_settled`, `session_info_changed`, `thinking_level_changed`, `bash_execution_update` intentionally emitted in `--mode json`? (In the source union, not in `json.md`.)
13. File-locking/concurrency on the session `.jsonl`: Harvester tailing while pi appends; two resumes of the same `--session-id`. NOT DOCUMENTED.
14. ~~Stdout purity on a fresh machine~~ **RESOLVED (smoke test): on a factory-fresh VM, every RPC stdout line was clean JSON (including with the exe-dev extension loaded and fetching its catalog). Warnings go to stderr.**
15. Is `tool_execution_update` emitted for all built-in tools or only streaming ones (docs only exemplify `bash`)?
16. `toolcall_start` per provider: are tool `id`/`name` populated at start or only at end? Read from `partial.content[contentIndex]`.
17. Copying `~/.pi/agent/auth.json` between machines for OAuth creds: device-binding? refresh races across N VMs? NOT DOCUMENTED. (Moot if the LLM gateway works.)
18. ~~Version pinning~~ **RESOLVED (smoke test): image ships pi 0.83.0 — exactly the version this research is pinned to.**

## Watch: pi protocol v2 (upstream, experimental)

`packages/protocol` on pi `main` defines an experimental **protocol v2**: length-prefixed CBOR frames, first message `hello {version, bearer token}`, correlated request/response + server event envelopes, "session and server snapshots are authoritative; progress events are transient UI hints". Explicitly experimental, no compatibility guarantees, NOT in released 0.83.0.

Implications: this smells like a future pi server/daemon mode. If it ships, harvesterd may shrink to a bootstrapper and the Harvester envelope migrates to it — the client's pi-event rendering survives unchanged (we pass pi payloads through verbatim by design). Re-check on every pi release; do not adopt while experimental.

## Watch: pi main wire changes (unreleased, WILL land in the next release)

Verified 2026-08-04 against the pi checkout at `main` (= the 0.83.0 release plus ~212 unreleased commits; `packages/coding-agent/package.json` still reads 0.83.0):

- **`message_update` loses its cumulative `partial`.** New `packages/coding-agent/src/modes/json-event.ts` strips the cumulative AssistantMessage from every streaming event (`const { partial: _partial, ...deltaEvent }`), and `rpc-mode.ts` now routes all stdout through `toJsonEvent(event)`. Our render contract ("each delta carries the whole partial message — replace, don't append") breaks the moment exeuntu ships the next pi: the chat must learn to accumulate from `message_start` + deltas (or treat `message_end` as authoritative) **before** any pi bump.
- **`get_available_models`/`set_model` became snapshot-based.** `rpc-mode.ts` switched `await session.modelRuntime.getAvailable()` → `getAvailableSnapshot()`; early in process life the snapshot may be incomplete. Affects the throwaway `list_models` flow (it asks immediately after spawn) — likely needs a retry-until-nonempty guard after the bump.

## Smoke-test log — 2026-08-01, throwaway VM `harvester-smoke` (created and deleted for this)

Additional verified facts beyond the resolutions above:

- **stdin-close shutdown: exit 0, clean** — `{ cmds; sleep; } | pi --mode rpc ...` terminated with exit 0 when the pipe closed. Design assumption confirmed live.
- **`PI_OFFLINE=1` + `--approve` + project `.pi/settings.json` with `packages`**: no install occurred in offline OR online runs (no node/npm on the image), no hang, exit 0 both times. Practical answer for exeuntu: project npm packages are inert; startup never blocks. Keep the `GIT_TERMINAL_PROMPT=0`/BatchMode guards anyway (cheap insurance for git-source packages if node ever appears).
- **A real `extension_ui_request` exists in the wild**: with no preference file, the exe-dev extension emits `{"type":"extension_ui_request","method":"select","title":"Use exe.dev LLM integrations?...","options":[...]}` at RPC startup (no `timeout` field). Unanswered: pi keeps serving RPC (loop not blocked), the integration is silently skipped, `get_available_models` returns no `@llm` models. Harvester policy: pre-seed the preference file on exe-dev machines; the daemon dialog policy covers the rest.
- **Keyless auth end-to-end**: prompt → `agent_start` → deltas → `message_end` (`"PONG"`, usage+cost populated) → `agent_settled`, zero credentials on the VM.
- Full `new --json` response shape captured (ssh_dest, urls, proxy_port, ...); `rm`/`restart` are plain-text confirmations.

## E2E log — 2026-08-01, harvesterd on throwaway VM `dressage-but-python` (created and deleted for this)

First full-stack run: local client → SSH unix-socket forward (`ssh -N -L local.sock:~/.harvester/harvesterd.sock`) → static musl harvesterd → pi 0.83.0, keyless gateway. Both phases green: create → attach → `get_state` probe → prompt → stream → `agent_settled` → `get_entries` → stop (exit 0), then on a fresh connection: `list_sessions` shows `exited` + persisted name → resume same sid → `messageCount` intact → `get_entries {since}` delta replay → second turn → clean stop.

- **Bug found & fixed: closing pi's stdin requires *dropping* the `ChildStdin`.** `AsyncWriteExt::shutdown()` on a pipe is a mere flush — pi never saw EOF, out-waited the 5s grace, got SIGKILLed (`exit_code: null`). After switching `close_stdin` to drop the handle: exit 0, observed through the whole stack, twice.
- **Ordering fact**: `session_exit` can reach an attached client *before* the `stop` response (broadcast vs request path race). Both orders are valid; clients must not assume a sequence. Recorded in `DAEMON.md`.
- Pi response envelope on stdout: `{"type":"response","command":<cmd>,"id":...,"success":true,"data":{...}}` — command payloads live under `data` (`get_state.data.sessionId`, `get_entries.data.entries/leafId`). Matches `rpc.md`; confirmed through passthrough.
- `--model gpt-5.6-sol@llm` alone is enough on exe.dev VMs — the extension-registered provider (`exe-dev-openai`) resolves from the model id; no `--provider` needed.
- Static-pie musl binary (849 KB) runs on the stock exeuntu image with zero setup; socket + log dirs self-created on first run.

### Round 2 — stop escalation ladder (same day, throwaway VM `axolotl-kayak`, created and deleted)

The 5s-grace stop was redesigned (abort → stdin EOF → 30s SIGKILL backstop, non-blocking) and re-verified end-to-end, including a new mid-turn phase:

- **Stop mid-turn: exit 0 in 0.2s.** `abort` + EOF while the model was streaming a long answer — no SIGKILL, session resumable afterwards with history intact. The fixed 5s grace this replaced was a blind timer with SIGKILL as its normal outcome (see the `ChildStdin` finding above) — the redesign makes SIGKILL a never-expected last resort.
- **Source facts behind the design** (rpc-mode.js, agent-session.d.ts): stdin EOF calls `shutdown()` immediately — it never waits for an in-flight turn; `session.dispose()` is synchronous; `flushRawStdout()` is skipped only for SIGTERM (the truncation source). The 30s backstop exists solely for a wedged runtime or a hung extension `session_shutdown` handler.
- **Bug found & fixed: attachments must be generation-aware.** A connection's attach entry held only sid → forwarder; after stop+resume the same sid names a *new* pi process, so re-attach was a silent no-op and connection cleanup would underflow the new session's `attached` counter (wedging the empty-room dialog policy). Fix: the attachment holds the `Arc<Session>` it incremented; staleness is `Arc::ptr_eq` against the current pid-table entry; forwarders end after delivering `session_exit` (exit is terminal).
- **Gateway cold-start flake (first boot)**: the exe-dev extension's discovery fetch can time out right after VM creation — pi then exits 1 with "Model … not found" and the stderr log states the cause plainly. Design held up: client sees `session_exit`, cause is one SSH `tail` away. Same fetch succeeded (200, 0.15s) seconds later; a client retry of `create_session` is the correct response.

### Round 3 — bootstrap-over-SSH (2026-08-02, throwaway VM `tare-or-ethernet`, created and deleted)

`harvester-client-core::bootstrap_and_connect` verified live: fresh install (`Installed`), idempotent reconnect (`AlreadyCurrent`), on-disk downgrade → `Upgraded` (probe path), running-0.0.9-daemon-with-current-disk-binary → `Upgraded` (ping-mismatch forced-reinstall path), and the terminal `VersionMismatch` error when the shipped binary itself is stale. Then a full protocol smoke through `harvester-cli raw`: create → attach → prompt → `HARVEST_OK` → `agent_settled` → stop → `exit_code: 0`.

- **Bug found & fixed: a trailing `&` backgrounds the entire `&&` list** — and POSIX gives async lists stdin from `/dev/null` in non-interactive shells. With install (`cat > binary`) and start chained by `&&`, the whole chain ran async, `cat` read `/dev/null`, and a 0-byte binary "installed" with exit 0. Fix: parenthesize the start — `(setsid harvesterd … &)` — so only the daemon is backgrounded. The manual e2e rounds never hit this because their start command was standalone.
- **Host keys**: `BatchMode=yes` alone cannot reach a *fresh* VM (unknown host key → hard fail). `StrictHostKeyChecking=accept-new` fixes first contact without prompts and still hard-fails if a known key changes; adequate given exe.dev's fleet-wide shared host key.
- `ssh -L` does **not** expand `~` in the remote socket path — the probe fetches the remote `$HOME` (same round trip as the version check) and the forward uses the absolute path.
- Upgrade kill: remote `pkill -x harvesterd` + a `pgrep` wait-loop before starting the replacement, so the new daemon cannot lose the socket-bind race to a dying predecessor. SIGTERM to harvesterd closes its pi children's stdin — their clean shutdown path, per contract.

### Round 4 — gpui client vertical slice (2026-08-02, throwaway VM `egg-by-penguin`)

`crates/harvester` (gpui 0.2 from crates.io) verified live against a session seeded via `harvester-cli raw` (`SLICE4_OK`, stopped clean): the app bootstrapped the VM by itself, listed the exited session, resumed it on click (`create_session` same sid → attach → `get_entries`), rendered history including tool-call placeholders and tool-result messages, and streamed fresh turns. Confirms `get_entries` with no `since` returns full history, and that chaining `get_entries` right after `create_session` needs no readiness probe — pi answers once its stdin queue drains.

- gpui facts: published on crates.io since this round (`0.2.x`, out of the Zed monorepo); smol-based executor, **no tokio interop** — the client runs tokio on a dedicated bridge thread, tokio mpsc channels are reactor-independent and awaitable from gpui tasks; no built-in text-input widget (`examples/input.rs` is the canonical IME-correct implementation, vendored); `tokio::select!` + client-core's `DaemonReader::next` is unsafe under cancellation (partial line dropped) — reads live on their own task.
- Build facts: needs `libxkbcommon-x11-dev` on Linux (X11 feature links it even under Wayland); full-DWARF dev builds of the gpui tree are multi-GB — workspace `[profile.dev] debug = "line-tables-only"`.

### Round 5 — ghost chat / `list_models` (2026-08-03, same VM `egg-by-penguin`)

- **Stdin EOF races the response on a throwaway pi**: `echo '{"type":"get_available_models",...}' | pi --mode rpc --no-session` returns *nothing* — pi sees EOF and takes its shutdown path before answering the queued command. Holding stdin open until the response line is read (then closing) works every time. Consequence for harvesterd's `list_models`: write request → read response → close stdin, never write-and-close. This does not contradict the Round-4 "stdin queue drains in order" fact — *commands* queue in order; EOF is not a command, it preempts.
- `get_available_models` on an exe.dev VM: 50 models, each with `id`/`name`/`provider` (`provider` present on gateway models too, e.g. `exe-dev-openai`), whole run ~0.9–1.4s including pi startup.
- Full ghost chain verified over the daemon socket (protocol e2e, no UI): `create_session` (fresh sid + `--provider`/`--model` from the models list + `name`) → `attach` → passthrough `prompt`, no probe between — prompt answered, `get_entries` showed both messages, `list_sessions` carried the name, `stop` clean.

### Round 6 — mid-session model switching (2026-08-03, pi 0.83.0 `docs/rpc.md` + live e2e on `egg-by-penguin`)

Source-verified in `~/.local/pi/docs/rpc.md` (§Model, §get_state) and confirmed live over the daemon socket (create → attach → `get_state` → `get_available_models` (50) → `set_model` gpt-5.6-sol→terra confirmed by a second `get_state` → bad model rejected `"Model not found"` → stop):

- `set_model {"provider": "...", "modelId": "..."}` switches the **running** session's model; response `data` is the full Model object now in effect. Errors are clean (`"Model not found: ..."`).
- `get_state` returns `data.model` — the current Model object or `null` — plus `thinkingLevel`, `isStreaming`, `sessionName`, etc.
- `get_available_models` works on a running session too — no throwaway pi needed when one is attached; harvesterd's generic passthrough carries all three (no daemon changes).
- `cycle_model` / `set_thinking_level` / `cycle_thinking_level` exist (thinking levels still deferred client-side).
- UNVERIFIED: `set_model` behavior mid-turn (verified idle-session only; docs state no restriction — the client allows it and surfaces any pi error in the statusbar).
