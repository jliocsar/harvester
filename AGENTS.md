# Harvester — agent instructions

Rust + gpui orchestrator for Pi coding-agent sessions on remote VMs. Read
`docs/SPEC.md` first; it is the umbrella spec and points at the normative
contract docs. Do not re-research what the docs already answer.

## Source-of-truth docs (in `docs/`, this order)

1. `SPEC.md` — umbrella: architecture, citizens, flows, non-goals, open questions.
2. `DAEMON.md` — client ↔ harvesterd contract (envelope, launch line, dialog policy).
3. `PROVIDERS.md` — machine provider contract (one-shot JSON-stdio executables).
4. `HARNESS_RESEARCH.md` — verified Pi/exe.dev facts; every claim sourced or marked UNVERIFIED.
5. `internals/<crate>.md` — per-crate implementation notes: invariants, rationale,
   gotchas, const meanings. This is where code comments would go — see below.

When a decision changes, update the doc in the same change. `SPEC.md` defers to
the contract docs on conflicts.

## No comments in code — ever

This is a **zero-comment codebase**: no `//`, `///`, `//!`, or `/* */` anywhere
in `crates/`. Everything a comment would say lives in `docs/internals/<crate>.md`
instead, organized by module. When you write code that needs explaining, explain
it there in the same change; when you change documented behavior, update the
notes. History and rationale persist in `docs/`, never inline.

Enforced by `crates/harvester/tests/no_comments.rs` (a string-literal-aware
lexer over every `.rs` file; runs in `cargo test --workspace`). `src/bin/`
spikes are exempt — throwaway code was always exempt from house rules.

## Architecture invariants (do not violate)

- **Disk on the VM is the only source of truth.** Pi's session `.jsonl` files are
  the persistence layer. harvesterd is stateless (pid table + pipes + subscribers,
  all in-memory). No daemon journals, no metadata files — deletion was the design.
- **The client speaks Pi.** Pi payloads cross every layer as `serde_json::Value`,
  verbatim. Never normalize, never translate pi events. Only the envelope is typed.
- **One pi process = one session file, always.** Never send `new_session`,
  `switch_session`, `fork`, `clone` to a running pi.
- **Terminate pi by closing stdin, never SIGTERM** (truncates the last stdout line).
  Pi RPC's lifetime is bound to stdin — that fact is *why* harvesterd exists.
- **LF-only JSONL framing** everywhere (pi's rule, adopted for our protocol too):
  split on `LF`, strip a trailing `CR`, never a generic Unicode line reader.
  Use the `LF`/`CR` constants from `harvester-proto`.
- **Sessions resume with the same call that creates them** (`--session-id` is
  resume-or-create). Don't build separate resume paths.
- Dialogs: forward when a client is attached; auto-cancel (`cancelled: true`)
  when the room is empty. Never leave a pi dialog unanswered.

## Pi version discipline

Everything is pinned to **pi 0.83.0** (what exeuntu ships). `--session-id` and
stdin-close shutdown are source-verified but *undocumented* API — re-verify on
every pi upgrade (see `HARNESS_RESEARCH.md`, incl. the protocol-v2 watch item).
When docs are ambiguous, read the shipped source at
`/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/` (local pi binary
itself is broken — node version mismatch — but the source is readable).

## Code style (extracted from review rounds — treat as house rules)

- **Controller layer pattern**: `server.rs` only frames/decodes/routes/encodes.
  Command logic lives in `ops.rs` (one named function per command) and
  `passthrough.rs`. Keep it that way as the surface grows.
- **No magic values**: any tunable number or protocol byte becomes a named
  `const` (`OUTGOING_QUEUE_DEPTH`, `SESSION_EVENT_BUFFER`, `LF`), with its
  meaning recorded in `docs/internals/<crate>.md`.
- **Factor out named handlers**: prefer `handle_x`/`create_x` extractions over
  inline branching (`handle_response_line`, `handle_dialog_request`,
  `create_pi_command` are the pattern).
- **Match arms delegate**: every enum match arm calls a dedicated handler
  function unless the arm is trivial (a one-line assignment). Dispatch
  functions like `handle_bridge_event`/`dispatch` are pure routers.
- **Group declarations before actions** inside a function body.
- **Blank-line padding around blocks** (before/after `if`/`loop`/`match`, before
  `return`) — rustfmt cannot enforce this; write it that way. A dylint custom
  lint is the recorded future fix.
- **Encapsulate invariants with privacy**: `Session.stdin` is private so all
  writes go through `write_line` (single-line framing + flush). Don't pub fields
  whose misuse breaks a protocol invariant.
- **Earn every dependency**: no clap until a third CLI form exists; shell out to
  `kill` rather than pull libc for one syscall. Static musl binary stays tiny.
- Handle all errors; log ignored-but-noteworthy failures to stderr rather than
  swallowing silently.
- Long boring descriptive names beat short clever ones.

## Working with the user

- **Co-design, never fait accompli**: present judgment calls as numbered options
  with a recommendation and trade-offs, get verdicts, then write it down.
- **plannotator** is the canonical review tool for plans and diffs; expect
  annotation rounds and respond with per-finding verdicts
  (Confirmed / Partly / Not a bug / Intended) before changing code.
- Verify claims against docs/source before asserting; label anything unverified.
- **exe.dev testing**: throwaway VMs are fine (`ssh exe.dev new/rm`), but NEVER
  delete a VM you didn't create this session. VM user is `exedev`;
  keyless LLM gateway is enabled by pre-seeding
  `~/.pi/agent/exe-dev-llm-integration.json` = `{"version":1,"useExeIntegration":true}`.

## Build & test

```sh
cargo fmt && cargo check --workspace     # gate before handing anything over
cargo build --release --target x86_64-unknown-linux-musl -p harvesterd  # the shippable daemon
```

Protocol smoke test: run `./target/debug/harvesterd`, connect to
`~/.harvester/harvesterd.sock` with a short python/socat script, drive
`ping` / `list_sessions` / error paths. Kill the daemon and remove the socket after.
