# Harvester — agent instructions

Rust + gpui orchestrator for Pi coding-agent sessions on remote VMs. Read
`docs/SPEC.md` first; it is the umbrella spec and points at the normative
contract docs. Do not re-research what the docs already answer.

## Source-of-truth docs (this order)

1. `docs/SPEC.md` — umbrella: architecture, citizens, flows, non-goals, open questions.
2. `docs/DAEMON.md` — client ↔ harvesterd contract (envelope, launch line, dialog policy).
3. `docs/PROVIDERS.md` — machine provider contract (one-shot JSON-stdio executables).
4. `docs/HARNESS_RESEARCH.md` — verified Pi/exe.dev facts; every claim sourced or marked UNVERIFIED.
5. `DESIGN.md` (repo root) — the design system as shipped: the rem axis, type
   scale, colour roles, density tiers, component-choice and accessibility rules.
   Normative for anything that renders. Read it before adding UI.
6. `docs/internals/<crate>.md` — per-crate implementation notes: invariants, rationale,
   gotchas, const meanings. This is where code comments would go — see below.

When a decision changes, update the doc in the same change. `SPEC.md` defers to
the contract docs on conflicts; `DESIGN.md` is the authority on visual and
interaction decisions, and defers to the code when they disagree.

## No comments in code — ever

This is a **zero-comment codebase**: no `//`, `///`, `//!`, or `/* */` anywhere
in `crates/`. Everything a comment would say lives in `docs/internals/<crate>.md`
instead, organized by module. When you write code that needs explaining, explain
it there in the same change; when you change documented behavior, update the
notes. History and rationale persist in `docs/`, never inline.

Enforced by `crates/harvester/tests/no_comments.rs` (a string-literal-aware
lexer over every `.rs` file; runs in `cargo test --workspace`). `src/bin/`
spikes are exempt — throwaway code was always exempt from house rules.

The ban covers Rust tokens under `crates/` and nothing else: `#` prose in Cargo
manifests and in `providers/` scripts is allowed, and is where a build-time or
provider-side decision is explained, because neither file has a
`docs/internals/<crate>.md` counterpart a reader would think to open.

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

Pi's version is deliberately **invisible to the user** — no pin, no version
management, on any machine. The client depends only on wire surface verified
across the supported range (currently **0.83.0 through 0.84.3**, both
verified live on 2026-08-27), and every version-sensitive read degrades
gracefully: streaming accepts both 0.83's cumulative `partial` and 0.84's
bare `text_delta`s, and an unknown future shape falls back to `message_end` —
text arrives late, never lost, never hung. This policy exists because a
version drift (0.84.3 via a `latest` mise pin) once broke streaming
*silently*; graceful degradation plus per-release verification replaced
pinning as the answer.

Two behaviors Harvester stands on are *undocumented* pi API and cannot be
tolerated client-side: `--session-id` resume-or-create and stdin-close
shutdown. When a new pi version ships, run `scripts/pi_version_matrix.py` on
a throwaway VM (~2 minutes): it verifies both, plus the dialog channel, tool
events, `get_state`/`set_model`/`get_entries` shapes and models availability.
The verified matrix and the protocol-v2 watch item live in
`HARNESS_RESEARCH.md`. When docs are ambiguous, read the shipped source under
`~/.local/share/mise/installs/pi/<version>/`.

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

## One-way rules (there is exactly one way to do each of these)

Landed by the 2026-08-29 simplification round, which found the same job done two
or three ways in about thirty places. Each rule below is written so a reviewer can
check it with a grep. All violation counts are currently zero — keep them there.

- **A constant both ends of the wire must agree on lives in `harvester-proto`,
  with a test pinning its contents.** `DIALOG_METHODS` was duplicated
  byte-for-byte in the daemon and the client, with nothing relating them; drift
  would have parked a pi dialog nobody could answer.
- **One implementation per computation, in the lowest crate both callers already
  depend on.** `unix_millis_now`, `$HOME` resolution, stage-then-rename,
  `message_text`, fixture loading and "last path component" were each written
  twice; three had already diverged, two of those silently changing behaviour.
  Before writing a helper, grep for it.
- **A pi wire token is named in `harvester-proto` only when more than one crate
  reads it.** A token confined to one match arm in one file stays a literal —
  naming it buys indirection, not safety.
- **Every struct literal and every constructor return type is written `Self`.**
- **Every match arm calls one named function unless its body is a single
  expression.** Value construction and `Err`-log-and-return arms are exempt.
- **No error type exists unless something matches on its variants.** Three error
  vocabularies are permanent and correct: pi's JSON failure payload (an
  invariant), `std::io::Error`, and a display `String` at the client-core → app
  seam. A fourth must justify itself with a match site. `FleetError` could not.
- **`pub` means "re-exported from `lib.rs`"; everything else is `pub(crate)`.**
  `token.rs` is the reference implementation.
- **A window-free `present_*` function returns one presentation value; the view
  only reads entities and renders it.** `workbench/machine_presentation.rs` and
  `models::present_prompt_strip` are the pattern.
- **One truncation helper, and the budget includes the ellipsis.**
  `ui::core::truncated_to_max_chars`. Extractors that never ellipsise are a
  different operation and stay separate.
- **Outside `ui/`, `cx.theme()` is for structural slots only; every text colour
  comes from a `Tone`.** Greppable: `cx.theme()` outside `ui/` should name only
  `sidebar`, `background`, `foreground`, `border`, `radius`, `font_size`.
- **The gallery constructs no component the app constructs itself; both call one
  `ui::` factory.** The gallery drifting from the app has now cost us twice.
- **A module with a non-trivial pure function carries an inline
  `#[cfg(test)] mod tests`.** `tests/` is reserved for checks that walk the
  filesystem (`no_comments.rs`, `fixtures.rs`).
- **Every `.rs` file under `crates/` is covered by a `##` section in
  `docs/internals/<crate>.md`** naming either the file or its directory (a
  directory section is only for a set of files that are one repeated shape, like
  `gallery/src/stories/`). And a doc sentence claiming "the one place that X"
  must name the grep that proves it — three such claims were false when checked.

Two clippy opinions this codebase deliberately rejects, both from the `pedantic`
group and neither on by default: `map_unwrap_or` (42 sites use
`.map(f).unwrap_or(d)`; 2 used `map_or` and were converged onto the majority) and
`unnecessary_wraps` (test helpers return `Option<T>` on purpose, to compare
against the function under test).

## Working with the user

- **Co-design, never fait accompli**: present judgment calls as numbered options
  with a recommendation and trade-offs, get verdicts, then write it down.
- **plannotator** is the canonical review tool for plans and diffs; expect
  annotation rounds and respond with per-finding verdicts
  (Confirmed / Partly / Not a bug / Intended) before changing code.
- Verify claims against docs/source before asserting; label anything unverified.
- **exe.dev testing**: the plan is **prepaid** (~$20/mo, roughly 50 VMs, otherwise
  idle), so throwaway VMs are *encouraged* — go wide whenever a real VM answers a
  question faster than reasoning about one. The standing rule is create-your-own:
  spin up your own (`ssh exe.dev new --json --name <label>`), tear down everything
  you created (`ssh exe.dev rm <name>`), and NEVER touch, rename, or delete a VM you
  did not create this session. VM user is `exedev`; keyless LLM gateway is enabled by
  pre-seeding `~/.pi/agent/exe-dev-llm-integration.json` =
  `{"version":1,"useExeIntegration":true}`. The image tracks pi upstream
  (0.84.3 as of 2026-08-27); VMs are not pinned — record the VM's
  `pi --version` alongside anything captured as a fixture or a documented
  fact, and use `exeuntu update pi --version <v>` only when a comparison
  needs both sides.

## Build & test

```sh
cargo fmt && cargo check --workspace     # gate before handing anything over
cargo clippy --workspace --all-targets   # lint policy: docs/internals/lints.md
ast-grep scan                            # house rules clippy cannot express
cargo build --profile daemon-release --target x86_64-unknown-linux-musl -p harvesterd  # the shippable daemon
```

Lint config lives in `[workspace.lints]` (Cargo.toml), `clippy.toml` and
`.ast-grep/rules/`; every verdict and every deliberate `allow` is recorded in
`docs/internals/lints.md`. Two of the one-way rules below are now compiler-
enforced rather than grep-checked: `unreachable_pub` is the `pub`/`pub(crate)`
rule and `clippy::use_self` is the `Self` rule.

Protocol smoke test: run `./target/debug/harvesterd`, connect to
`~/.harvester/harvesterd.sock` with a short python/socat script, drive
`ping` / `list_sessions` / error paths. Kill the daemon and remove the socket after.
