# harvester-proto — implementation notes

This codebase carries no code comments (see AGENTS.md). Everything a comment
would have said about `harvester-proto` lives here instead, one section per module.
Update the relevant section in the same change that touches the code.

## crates/harvester-proto/src/lib.rs

Wire types for the client ↔ harvesterd protocol (docs/DAEMON.md, protocol v1).

### Framing (JSONL, pi's rules)

- `LF` is the *only* record delimiter, everywhere on the wire (matches pi's own
  RPC framing). A trailing `CR` is tolerated and stripped, but never emitted.
  Never use a generic Unicode line reader — U+2028/U+2029 are line breaks to
  such a reader but ordinary characters inside a JSON string, and `serde_json`
  emits them unescaped, so a generic reader would corrupt frames that contain
  them. `read_frame` is the only correct way to read the protocol for this
  reason.
- `read_frame` borrows from the caller's reused buffer (cleared each call), so
  callers that need to retain a frame (or read the next one) must copy it out
  first. A final unterminated line is still delivered as a frame (`None` only
  means EOF) — pi flushes one of these on shutdown, and dropping it would lose
  the last response of every session.
- `encode_frame` serializes one value as a complete `LF`-terminated line.
- `FRAME_BUFFER_START_BYTES` (64 KiB) is the starting size of a reused frame
  buffer shared by every read/write side of this framing (a connection's
  writer, a session's stdout pump, the throwaway `list_models` pi). The buffer
  grows to the largest line it ever sees, so this constant only decides how
  often it has to grow; 64 KiB was chosen because it clears pi's
  `message_update` deltas, which re-send a whole partial message and are the
  largest routine line on the wire.

### Pi payloads stay `Value`

Pi payloads are deliberately typed as `serde_json::Value` — the client speaks
Pi and harvesterd passes them through verbatim, never normalizing or
translating. Only the envelope (the harvester-native wrapper) is typed. This
shows up in `Request::Pi`, `Outgoing::PiResponse`, and `Outgoing::Event`.

### `PROTOCOL_VERSION`

Envelope compatibility, and nothing else: bumped **only** when the wire shape
changes in a way that makes an older peer wrong. It is what `ping` exchanges
and what a version-mismatch check tests.

Deliberately *not* the crate version (it used to be, via
`env!("CARGO_PKG_VERSION")`, shared across all five workspace crates). That
conflated two unrelated questions and failed in both directions: rebuilding
harvesterd without bumping the workspace left every bootstrapped machine on
the stale binary forever (the probe saw a version match and skipped the
upload), while a UI-only version bump force-upgraded every daemon — killing
every running pi child — for a change the wire never saw.

"Is the daemon out there the binary I ship?" is a *different* question,
answered by the build fingerprint (`PingData::build`), not by this constant.

### `DAEMON_SOCKET_PATH`

The daemon's socket path, relative to `$HOME`. Spelled once here (rather than
duplicated in both binaries) because it is the rendezvous point between them;
drift between two copies would surface only as an unexplained connect
timeout.

### `DEV_DAEMON_PATH`

The workspace-relative path of the shippable daemon binary
(`target/x86_64-unknown-linux-musl/daemon-release/harvesterd`). It was spelled
three times — `harvester/build.rs`, `harvester/src/main.rs`, and
`harvester-cli/src/main.rs` — so a profile or target rename was a three-file
hunt; now it is spelled once. It lives in this crate for the same reason
`DAEMON_SOCKET_PATH` does: the binaries that need it already depend on the
protocol crate.

The const sits in its own file (`src/dev_daemon_path.rs`, re-exported from
`lib.rs`) rather than inline in `lib.rs`, because `harvester/build.rs` shares
it by `include!`-ing that file: a build script importing the crate normally
would pull the whole proto dependency tree (tokio included) into the
build-dependency graph for one string. The file must therefore stay
free-standing — one `pub const`, no `use`, nothing else.

Produces `fnv1a64-<byte count>-<hash>`: FNV-1a-64 over the file's bytes, hash
in lowercase 16-digit hex. `FNV_OFFSET_BASIS` / `FNV_PRIME` are the reference
FNV-1a-64 constants.

This function lives in the *protocol* crate (not the daemon or the client)
because the resulting string is wire-visible — it travels in `PingData::build`
and the client computes the identical value over the binary it is about to
ship, to decide "install iff mine differs from yours" (docs/DAEMON.md
§Bootstrap). Two independent implementations of a value that must compare
equal across a network is a drift bug with a vicious failure mode: every
machine either never upgrades, or upgrades on every connect. Keeping exactly
one implementation, next to the framing code it is peer to, avoids that.

The byte count prefix is free to carry and makes two deploys eyeball-
comparable; the `fnv1a64` tag names the algorithm so a future change to it is
visible in the value rather than silently breaking comparisons.

**Not a security boundary.** The hash is non-cryptographic on purpose — a
static musl binary shipped over SSH on every deploy doesn't earn a crypto
dependency for a staleness check, and anything that can answer on the socket
at all can answer with any string it likes. It exists to detect the failure
mode that actually happens (a rebuild that never reached the machine), not a
hostile one.

`FINGERPRINT_CHUNK_BYTES` (64 KiB) is how many bytes `fingerprint_reader`
hashes per read. It's a tunable, hence named, but cannot change the result
(verified by the chunk-independence test — hashing in different chunk sizes
must produce the same fingerprint).

`fingerprint_bytes` is `fingerprint_reader` specialized for a binary already
in memory (how the client holds the daemon binary it ships, via
`bootstrap_and_connect`'s `daemon_binary`); infallible because reading from a
slice cannot fail.

### Envelope types

- `Request::Harvester` — a daemon-native operation.
- `Request::Pi` — verbatim pi RPC passthrough for one session.
- `HarvesterCmd` — daemon-native commands, i.e. only what pi cannot do itself:
  - `Ping` — first command on every connection; protocol-version handshake.
    Client sends its `PROTOCOL_VERSION`, daemon answers with its own.
  - `ListSessions` — sessions from pi's session dirs plus the daemon's pid
    table.
  - `CreateSession` — spawn (or resume — same thing) a pi process for `sid` in
    `cwd`.
  - `ListModels` — a throwaway `pi --no-session` in `cwd` answering
    `get_available_models`.
  - `Attach` / `Detach` — subscribe/unsubscribe this connection to a session's
    event stream.
  - `Stop` — close pi's stdin; 30s grace, then SIGKILL. Session stays
    resumable (see docs/SPEC.md / CLAUDE.md: terminate pi by closing stdin,
    never SIGTERM, because SIGTERM truncates the last stdout line).
- `Outgoing` — a daemon → client line: either a response to a request, or an
  unsolicited event.
  - `Outgoing::PiResponse.response` uses `#[serde(default)]` (not
    `skip_serializing_if`) because pi's fire-and-forget commands (e.g.
    `extension_ui_response`) answer with an explicit `response: null`, and
    serde cannot otherwise distinguish an explicit null from an absent key —
    both must mean the same thing here.
  - `Outgoing::Event` — one pi stdout line, verbatim.
  - `Outgoing::EventGap { sid, skipped }` — the daemon dropped `skipped`
    events for this connection because it was reading too slowly. This is
    daemon-originated, never a fabricated pi event: the client has an
    authoritative recovery path (passthrough `get_entries` from its last seen
    entry), but only works if it's told the gap happened.
  - `Outgoing::SessionExit` — the pi process for `sid` exited; cause lives in
    its stderr log (SSH tail), not on the wire. `exit_code` is `null` (kept
    present-and-null, not elided) when pi was killed by a signal.

### Response payload types

Typing the daemon-native payloads (as opposed to pi's, which stay verbatim
`Value`) means each wire field is modelled once instead of being hand-built
with `json!` on one side and hand-dug with `.get()` on the other.

- `HarvesterResponse::into_result` — `data` on success (`Null` when absent),
  the error string otherwise.
- `PingData`:
  - `version` — the daemon's `PROTOCOL_VERSION`, compared for envelope
    compatibility.
  - `build` — a fingerprint of the **running daemon's own executable image**,
    computed by harvesterd at startup. Answers "is the daemon running over
    there the binary I ship?", which the protocol version cannot answer since
    two builds of the same protocol are interchangeable on the wire but
    completely different code. Opaque to everyone but the producer: a client
    compares it against the fingerprint of the binary it's about to install
    (via `harvesterd build`) and installs iff they differ.
- `CreateSessionData.already` — true when the sid was already in the pid
  table, so no second pi was spawned. This is what keeps "one pi process = one
  session file" true. Not an error condition — it's what lets a client open
  any session with an unconditional `create_session` → `attach` chain.
- `AttachData.already` — true when this connection was already attached to
  *this generation* of the session object. A session that exited and
  respawned is a new generation, so the attach is real (not a no-op) even for
  a sid the client believes it already holds.
- The `detach` and `stop` payloads have no proto type: they are write-only
  shapes nobody decodes, built with `json!` at their ops.rs sites and pinned
  by docs/DAEMON.md (see docs/internals/harvesterd.md for the reasoning).
- `SessionInfo.file` — the session file on the machine, absent while pi has
  not written one yet. Pi creates the file lazily on the first entry, so a
  session that was spawned but never prompted is genuinely fileless; a client
  that fabricates a blank path here would be claiming a file that does not
  exist.
- `SessionInfo.mtime_ms` — milliseconds since epoch of the session file's last
  modification, or of the spawn time for a session that has no file yet.
  Either way, it means "last activity".

### Test-suite notes worth keeping in mind

- Round-trip tests assert wire shape directly against docs/DAEMON.md sections
  (Envelope, Operations, Events, ping, list_sessions, etc.) — the tests are a
  live check that the doc and the code have not drifted apart.
- The Unicode-line-separator test is the concrete regression test for the
  framing gotcha described above (U+2028/U+2029 inside a JSON string must not
  be treated as line breaks).
- `an_absent_already_decodes_as_false` / the elided-vs-explicit-false pattern:
  an elided key and an explicit `false` must decode identically, or a client
  reading a field the daemon skipped would fail the whole response instead of
  correctly reading "no, this was a fresh spawn".
- `session_exit_keeps_a_null_exit_code_for_signal_kills` — the `exit_code` key
  must be present-and-null for a signal kill, not elided; this is a
  deliberate encoding choice, not an oversight.
- `a_line_missing_a_required_field_is_an_error_not_a_default` — decoding is
  strict on purpose: leniency here would turn a daemon bug into a session
  that silently renders wrong, which is far harder to diagnose than a failed
  read erroring loudly.
- The FNV-1a-64 test vectors are the published reference vectors, pinning the
  exact output of a function the daemon and the client independently compute
  and must agree on byte-for-byte; an unpinned rewrite could silently break
  every machine's upgrade detection (never upgrade, or upgrade every
  connect).
