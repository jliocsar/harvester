# harvesterd — implementation notes

This codebase carries no code comments (see AGENTS.md). Everything a comment
would have said about `harvesterd` lives here instead, one section per module.
Update the relevant section in the same change that touches the code.

## The `daemon-release` cargo profile

harvesterd ships as a static musl binary that the client uploads over SSH on
every fingerprint mismatch, so size is the metric that matters for it:
`opt-level = "z"`, full LTO, `strip`, and `panic = "abort"` (a daemon that
panics has nothing to unwind for — the session state is on disk, and the
client re-bootstraps).

Those settings live in `[profile.daemon-release]` (root `Cargo.toml`,
`inherits = "release"`) and **not** in `[profile.release]`, because cargo
profiles are workspace-wide: as release overrides they also applied to the
gpui desktop client, where every one of them is wrong — `panic = "abort"`
kills the window on any UI panic instead of unwinding, and `opt-level = "z"`
plus full LTO trades frame time for bytes nobody ships. `[profile.release]`
is therefore stock, and the daemon build line is

```sh
cargo build --profile daemon-release --target x86_64-unknown-linux-musl -p harvesterd
```

which lands the binary in `target/x86_64-unknown-linux-musl/daemon-release/`.
That directory name is load-bearing in three places — `crates/harvester/build.rs`
(what it embeds), `crates/harvester/src/main.rs` (`DEV_DAEMON_PATH`, the dev
fallback) and `harvester-cli`'s `--daemon` default — so a profile rename is a
four-file change, not a one-line one.

## crates/harvesterd/src/daemon.rs

Shared daemon state: the pid table, and nothing else. Per docs/DAEMON.md,
harvesterd is stateless — pi's own session `.jsonl` files are the only source
of truth. This module is the in-memory exception: pids, not persisted state.

- `PASSTHROUGH_ID_PREFIX` (`"hv-"`) namespaces request ids the daemon injects
  into pi commands so the stdout pump can route a response back to its waiter
  by prefix match. The mint (`Daemon::next_passthrough_id`) and the match
  (in `session.rs`) must agree on the exact string, hence one shared const
  rather than a literal duplicated at each site.
- `SpawnOutcome::AlreadyRunning` vs `::Spawned`: both arms return the live
  session so callers never need to reconstruct the sid they just moved in.
  "AlreadyRunning" exists because the pid table is the per-sid single-instance
  guard — a second `create_session` for a live sid loses at the lock, and this
  is a domain outcome, not an error. It is deliberately *not* smuggled through
  `io::ErrorKind::AlreadyExists` for the caller to sniff; `io::Result::Err` is
  reserved for genuine spawn failures.
- `RunningSession` carries `cwd` and `spawned_at_ms` because the pid table is
  the *only* record of a session pi hasn't written a file for yet — a listing
  built from disk alone cannot see it at all (see fslist.rs).
- `DaemonInner.build_fingerprint`: this process's own build fingerprint,
  computed once at startup and reported in `ping` (docs/DAEMON.md §ping). Not
  persisted — it's the identity of the running image and dies with the
  process.
- `DaemonInner.sessions` uses `std::sync::Mutex` deliberately (not tokio's):
  no guard is ever held across an `.await`, and lookups happen on every
  passthrough, so a blocking mutex is fine and simpler.
- `Daemon::running_sessions()` copies every live pi child out from under the
  lock because no guard may cross the `spawn_blocking` boundary the caller
  (fslist) is headed for. Keyed by sid because `list_sessions` asks "is this
  sid running?" once per file on disk, and `Sid: Borrow<str>` makes that a
  borrowed lookup.
- `Daemon::spawn_session`: check-and-spawn under one lock. The table itself is
  the single-instance guard for a sid — this is what makes "one pi process =
  one session file, always" (CLAUDE.md) actually hold.
- **Generation identity**: a session's identity is its `Arc` pointer, not its
  sid. Stopping and resuming a sid spawns a *new generation* — a different pi
  child, a different `Arc<Session>` — under the same key. A sid match alone
  only proves "some pi for this sid is alive," which is the wrong question for
  a caller holding an older handle. `Daemon::is_current_generation` uses
  `Arc::ptr_eq` to ask the right question, and every "is my session still the
  live one?" check should go through it so the rule is stated once. It takes
  the sid *from* the session (not as a separate argument) because a
  caller-supplied sid could disagree with the handle — that pairing is exactly
  the invariant being enforced.
- `Daemon::remove_session_if_same`: vacates a session's table entry only if it
  is still the current generation; a resumed successor is never evicted.
  Called by the stdout pump *before* it broadcasts Exit. Tests-and-removes
  under one lock (rather than calling `is_current_generation` then
  re-locking) because a resume landing between the two calls would otherwise
  make this evict the successor it just checked was different.

## crates/harvesterd/src/fslist.rs

Implements `list_sessions`: a readdir-based scan of pi's session directories.
No daemon state is involved in the scan itself — the file's first line
(header) gives sid/cwd, and the last `session_info` entry in the file gives
the display name. Runs as **blocking std IO on purpose**: the caller invokes
this on tokio's blocking pool (`spawn_blocking`) so the daemon's single
async-runtime thread never waits on disk.

- `SESSION_INFO_MARKER` (`br#""session_info""#`): matched against raw bytes
  before anything is UTF-8-validated or JSON-parsed. Cheap prefilter — the
  overwhelming majority of lines in a conversation log are ordinary messages
  and never match, so most lines never pay for parsing.
- **Why `running` sessions are unioned into the listing, not just consulted
  for status**: pi writes a session's file *lazily*, on the first entry — so a
  session that has been spawned but never prompted has a live pi process and
  literally nothing on disk yet. A listing built purely from the filesystem
  scan would hide it entirely; and a client that cannot see a session cannot
  stop it, so that pi would outlive every client that could have reaped it.
  This is why `append_fileless_running` exists and why the union is by sid
  (not a blind append — a session already found on disk must not be listed
  twice). This behavior was found on a live VM, not derived from reading
  pi's source.
- Sessions are sorted newest-first by `mtime_ms` — this is the sidebar order
  clients expect.
- `read_session_file` streams the file **line by line** rather than slurping
  it whole: session files are full conversation logs that can run to many MB,
  and only the header line and the last `session_info` entry actually matter.
  A file whose header does not parse then costs exactly one line read.
- Read as **bytes** (`read_until(LF, …)`, matching pi's LF-only framing rule —
  see CLAUDE.md), never via `read_line`/text APIs: `read_line` would
  UTF-8-validate every byte of the *entire* log just to find two lines of
  interest, and critically its `Err(InvalidData)` on a single non-UTF-8 byte
  is indistinguishable from EOF at the call site — which used to silently
  truncate the name scan partway through a file. Reading raw bytes instead
  makes a corrupt/non-UTF-8 line just another line that fails the marker
  prefilter, so scanning continues past it (`invalid_utf8_line_does_not_truncate_the_name_scan`
  test documents this regression and its fix).
- `scan_for_last_name`: display name resolution rule is "the **last**
  `session_info` entry wins" because rename history in the file is
  append-only. Every line is first rejected by the byte-level substring
  prefilter, so only a real candidate line is ever UTF-8-validated and
  JSON-parsed.
- `entry_display_name` follows **pi's own rule** (source-verified at
  `session-manager.ts:713-714`, "use latest, including explicit clears"): the
  name is trimmed, and an empty or whitespace-only name is treated as an
  *explicit clear*, not as a blank/missing title. A later nameless
  `session_info` entry therefore clears an earlier name — "last entry wins"
  includes losing the name, by design.
- `append_fileless_running`: a session with no file yet has no header line, so
  everything reported about it (`cwd`, `mtime_ms`) comes from what the pid
  table recorded at spawn.
- The scan takes **no cwd filter**. `list_sessions` used to accept an optional
  `cwd` and thread it through all three functions here; no client ever sent
  one (the gpui client renders every machine's sessions in one sidebar and
  needs the whole list), so it was a filter that only ever matched everything.
  Filtering belongs to the caller that knows which workspaces it is showing —
  the daemon's job is the scan (docs/DAEMON.md §list_sessions).

## crates/harvesterd/src/main.rs

harvesterd is the per-machine daemon owning `pi --mode rpc` children. Contract
is docs/DAEMON.md (protocol v1). Stateless by design.

- `LOCK_FILE_NAME` (`harvesterd.lock`): the single-instance lock lives next to
  the socket in `~/.harvester`, as its own file rather than locking the socket
  file itself. Reason: the socket cannot answer "is a daemon already running"
  — nothing unlinks it on exit, so a stale socket is the *normal* case after a
  crash or `pkill`, and "connect refused, therefore unlink" would race a
  second daemon doing the exact same thing concurrently.
- `RUNNING_EXECUTABLE` (`/proc/self/exe`): read instead of
  `std::env::current_exe()`'s path so the fingerprint stays correct after an
  in-place upgrade replaces the file underneath the running process — this is
  the identity of the image that is *running*, which is exactly what `ping`
  claims to report (docs/DAEMON.md §ping). For `harvesterd build`, the process
  was just launched from the on-disk file, so the same read answers the
  on-disk question too.
- `UNAVAILABLE_FINGERPRINT` (`"unavailable"`): returned when the executable
  can't be read at all. Never equal to a real fingerprint, so a client
  comparing for equality always concludes "different" and re-installs — the
  safe failure direction is a needless upload, not a machine silently pinned
  to a stale daemon forever.
- `acquire_single_instance_lock`: the lock handle is deliberately leaked
  (`std::mem::forget`). The kernel drops an `flock` automatically whenever the
  process dies, SIGKILL included, so there's no stale state to clean up and
  nothing in the daemon should ever be tempted to release it early.
- `remove_stale_socket` is only ever safe to call with the single-instance
  lock already held — the one process that could still be listening on that
  path would have had to take the lock first.
- Startup ordering matters: lock first, then clear the rendezvous socket path,
  then bind. With the lock held, no second daemon can land between those two
  steps — this is exactly what the old connect-then-probe approach could not
  guarantee.
- `print_build_fingerprint` (`harvesterd build`) fails loudly on error rather
  than printing the daemon's degraded sentinel: a client comparing an
  unreadable answer against a real fingerprint would otherwise reinstall on
  every single connect, forever.
- `startup_build_fingerprint` degrades instead of exiting: an unreadable
  `/proc/self/exe` (e.g. an exotic mount namespace) must not stop the daemon
  from serving sessions.
- CLI surface is deliberately just a one-argument match: `harvesterd` (run),
  `harvesterd version` (the protocol handshake the bootstrap probe reads), and
  `harvesterd build` (this binary's fingerprint, for install-iff-it-differs).
  No `clap` — earn every dependency (CLAUDE.md); an arg parser would be bought
  for nothing here.
- `~/.harvester` permissions (`0o700`) are enforced in the process that owns
  the socket itself so they hold regardless of how the daemon was started or
  what the machine's umask is.
- Test `the_running_executable_fingerprints`: the hash algorithm's own
  behavior is pinned in `harvester-proto`; what's ours to prove here is that
  we read the *right file* and get a real answer — the test binary is itself
  readable via `/proc/self/exe`.

## crates/harvesterd/src/models.rs

Implements `list_models`: the throwaway-pi query behind the unstarted chat's
model picker, plus the stderr forensics that make a pi which dies on startup
diagnosable. Split out of `ops.rs` so the dispatch layer stays a list of
one-command handlers; `ops::spawn_list_models` is the only caller.

- **`list_models` / `query_available_models`**: the models pi would
  offer in a given `cwd` are fetched fresh on every call (DAEMON.md requires
  this to be deterministic and untouching of any session — no caching). A
  throwaway `PI_OFFLINE=1 pi --mode rpc --no-session` process asks
  `get_available_models` and pi's `data` is returned verbatim. Stdin must stay
  open until the response line is read, because pi treats EOF as shutdown and
  will die before answering otherwise (verified live on pi 0.83.0). The child
  is `kill_on_drop`, so on every exit path it's either already exiting (stdin
  already dropped) or gets SIGKILLed — no pi ever outlives the function. This
  work rides its own task since even the ~1s pi startup (bounded by
  `LIST_MODELS_TIMEOUT`) must not block sibling requests on the same
  connection.
- **`capture_pi_stderr` / `explain_pi_failure` / `pi_stderr_summary`**: pi's
  stderr used to be `Stdio::null()`, so a pi that died on startup produced
  only "pi exited before answering" and the reason was gone. It is piped now.
  The drain runs as its own task and keeps reading *after* the
  `PI_STDERR_CAPTURE_BYTES` cap is reached, appending nothing — stopping the
  reads instead would fill the pipe and block pi, turning a crash report into
  a hang. Both failure arms (clean EOF and a read error) route through
  `explain_pi_failure`, which writes the whole capture to the daemon's own
  stderr — `~/.harvester/log/harvesterd.log` when started by `bootstrap.rs` —
  and appends a one-line summary to the error the client shows.
  `pi_stderr_summary` prefers the first non-empty line *containing
  whitespace*, falling back to the first non-empty line: a message has words,
  whereas a bare token is a path or a source location. That rule is what turns
  node's first line (`.../cachestorage.js:20`) into the line that actually
  names the fault (`TypeError: webidl.util.markAsUncloneable is not a
  function`) — the real failure when pi runs on a Node older than 22, which
  is the case this was written for. A capture that summarizes to nothing
  leaves the caller's own reason untouched.

Named constants and why their values are what they are:
- `LIST_MODELS_TIMEOUT` (15s): ceiling on the whole throwaway `list_models`
  run (spawn → answer → exit; measured ~1s on exe.dev). Same role as
  `STOP_KILL_BACKSTOP` — only a wedged pi hits it, and `kill_on_drop` SIGKILLs
  on expiry, which is safe here because a `--no-session` pi has nothing
  durable to truncate.
- `MODELS_REQUEST_ID` ("hv-models"): deliberately inside the `hv-` namespace
  the passthrough pump routes on, even though this throwaway pi has no pump —
  the reply filter matches it directly.
- `PI_STDERR_CAPTURE_BYTES` (8 KiB) / `PI_STDERR_CHUNK_BYTES` (1 KiB): a
  startup crash report is a few hundred bytes and a node stack trace is a
  couple of KiB, so 8 KiB holds a whole one with room spare while bounding
  what a chatty pi can make the daemon hold.
- `PI_STDERR_SUMMARY_CHARS` (200): the summary rides in an error string the
  client renders in a toast, so it is capped to a readable length; the log
  keeps the untruncated text.
- `PI_STDERR_DRAIN_TIMEOUT` (2s): the drain task ends when pi's stderr closes,
  which a dead pi has already done, so this only bounds the pathological case
  where the pipe is held open by something we no longer control. Expiry costs
  the summary, never the error itself.
- `PI_EXITED_EARLY`: the one wording for "pi exited before answering", shared
  so the message the client matches on cannot drift between call sites.

## crates/harvesterd/src/ops.rs

`ops.rs` holds one named function per harvester command; `server.rs` only
frames/decodes/routes/encodes (the controller-layer house rule).

- **`dispatch`** returns `Option<Outgoing>`: `None` means a spawned task owns
  the eventual response. Slow commands (pi passthroughs, `list_models`,
  `list_sessions`) must never head-of-line block the connection loop, and the
  envelope `id` — not arrival order — is what correlates their answers back
  to the right request.
- **`with_checked_sid`** is the single gate every sid-carrying command passes
  through. A sid off the wire is an unvalidated remote string that goes on to
  be a pid-table key, a path component, and an argv element; rejecting here
  means no handler downstream ever sees an unchecked one.
- **`ping`** reports two independent numbers deliberately: `version` is
  envelope/protocol compatibility, `build` identifies the exact running
  daemon image (docs/DAEMON.md §ping). The daemon never acts on a version
  mismatch itself — that's the client's call, since only the client can
  reinstall — it just logs what it was told and reports its own, which is
  what makes a bootstrapped machine's log diagnosable.
- **`Attachment`** bundles a forwarder task handle with the exact `Arc<Session>`
  it incremented `attached` against. Holding the `Arc` (not just the sid)
  matters: a sid is ambiguous across resume generations, and releasing against
  the wrong generation would underflow the new session's counter and wedge
  the empty-room dialog policy (this was found live in an e2e run).
- **`Attachment`'s `Drop`** is the release: abort the forwarder, then leave the
  dialog room on the same session object `attach` joined, cancelling any
  dialogs that come back from leaving (non-empty only when this was the last
  watcher). Making release a `Drop` impl rather than a function every caller
  must remember means dropping an `Attachment` anywhere (explicit `remove`, a
  `clear` on disconnect, an overwriting `insert`) *is* the release — forgetting
  it is unrepresentable, and no dialog is ever left unanswered (docs/DAEMON.md).
- **`create_session`** is resume-or-create *and* idempotent: a sid that is
  already running succeeds with `already: true` instead of erroring. The
  daemon owns this truth because a client cannot know whether a session died
  while it was detached, so "make it running" is the only honest contract; it
  lets every open be the same unconditional create → attach chain. No second
  pi is ever spawned — the pid-table check inside `Daemon::spawn_session`
  answers this, preserving "one pi = one session file".
- **`spawn_list_sessions`** runs the disk scan on the blocking pool: session
  files can be large, and a synchronous read on the daemon's single runtime
  thread would stall every pump and connection meanwhile.
- **`spawn_list_models`** is the whole of `list_models` that belongs to the
  dispatch layer: hand the `cwd` to `models::list_models` on its own task and
  let it answer later. The throwaway-pi machinery and its constants live in
  `models.rs` (below) — same split as `list_sessions`, whose scan lives in
  `fslist.rs`.
- **`detach` / `stop` payloads** are built with `json!` rather than through a
  typed struct: `{}`, `{"stopping": true}` and `{"stopping": false}` are
  write-only shapes nobody decodes, and a `DetachData {}`/`StopData` pair in
  `harvester-proto` was two types whose entire content was the literal they
  produced. The shapes stay pinned by docs/DAEMON.md, by
  `detach_answers_the_documented_empty_payload` and by
  `stopping_a_session_that_is_no_longer_running_succeeds`.
  The `already` flags on `attach`/`create_session` keep their typed payloads —
  those are a documented contract the client reads.
- **`attach`**: subscribes to the session's broadcast channel *before* joining
  the dialog room, never the reverse. A dialog arriving between the two steps
  is auto-cancelled (room still reads as empty) — one spurious "Escape" — but
  joining the room first would let a dialog be remembered and broadcast before
  this subscriber exists, so nobody would ever see it and pi would wait
  forever. This is "safe direction over convenient direction". Attaching twice
  to the same generation is a no-op (checked via `Arc::ptr_eq` against the
  attached-map entry, independent from `Daemon::is_current_generation`'s pid
  table check because it must be evaluated against the *attached* map, not the
  pid table).
- **`spawn_forwarder`**: on `RecvError::Lagged`, the ring buffer dropped
  events for this subscriber. This is recoverable but never silent — an
  `EventGap` is sent so the client can `get_entries` to resync from its last
  seen event, then streaming continues from the oldest retained event.
- **`stop`** is non-blocking: it starts a polite escalation ladder and returns
  immediately. `session_exit` is the authoritative "it's done" signal; this
  response only means "shutdown initiated". A sid the pid table doesn't know
  takes the `already_stopped` arm — success with `{"stopping": false}`, not
  `no_such_session` — because the requested end state already holds and the
  caller is a UI whose picture of "still running" is always one event behind
  this process: the client's Stop button lives beside a busy indicator that a
  `session_exit` in flight has not yet cleared, and turning that race into an
  error banner would blame the user for the daemon's own timing. `attach`
  still errors on an unknown sid — that one *is* a lookup, and a client
  attaching to nothing has no stream to read. The escalation:
  1. *Rung 1* — write an `abort` command so pi records the in-flight turn
     cleanly in the session file instead of dying mid-stream. Failure here is
     fine, since a dead/deaf pi is exactly what the later rungs handle.
  2. *Rung 2* — close stdin (EOF), pi's documented shutdown path, after the
     queued abort drains.
  3. *Rung 3* (`kill_backstop`) — SIGKILL after `STOP_KILL_BACKSTOP` if the
     pid table still shows the *same generation* running. The "still current
     generation" liveness check matters because the pump vacates the pid-table
     entry before broadcasting `Exit`, so this stops the backstop from killing
     a pi that was resumed during the wait.

  Rung 3 is armed *first* in the code, on purpose: the ladder exists for a
  wedged pi, so no rung may depend on that pi cooperating, and ordering the
  backstop first keeps that guarantee independent of the stdin queue's
  behavior (rungs 1–2 only touch the queue, never the pipe, so neither can
  block on their own).

  Rung 1 registers a response waiter for the injected `abort` and drops the
  receiver on the spot (`drop(session.register_response_waiter(…))`). That
  looks pointless and is not: the pump swallows every `hv-`-prefixed response
  as a passthrough answer instead of forwarding it as an event, so pi's answer
  to *our* abort has to land somewhere. With no waiter registered it lands in
  `resolve_response`'s miss branch, which logs "dropped response for hv-N" to
  the daemon's stderr on every single stop — a false alarm in the one log a
  wedged session is diagnosed from. Registering gives the answer a home;
  dropping the receiver says nobody is waiting for it, because `stop` already
  answered `{"stopping": true}` and `session_exit` is the authoritative
  completion signal. A `oneshot::Sender` whose receiver is gone fails its
  `send` harmlessly, and the map entry is cleared either by that delivery or
  by `fail_pending_responses` when the session exits.

Named constants and why their values are what they are:
- `STOP_KILL_BACKSTOP` (30s): not a correctness knob — EOF triggers pi's
  immediate shutdown path (verified in rpc-mode.js; it never waits for an
  in-flight turn). This only fires for a genuinely wedged process (a hung
  extension `session_shutdown` handler, a stuck runtime).

The test module documents that payload wire shapes are pinned in
`harvester-proto`; what belongs to `ops.rs` is only the *routing* —
`data_response` re-serializes through a `serde_json::Map`, landing the payload
under `data`. Comparisons must be by value, not by byte string: that `Map` is
a sorted `BTreeMap` only while `serde_json`'s `preserve_order` feature is off,
and gpui turns it on for the whole workspace, so key order flips depending on
what else is built. Nothing may depend on JSON object key order.

## crates/harvesterd/src/passthrough.rs

Implements verbatim pi RPC passthrough (docs/DAEMON.md §The `pi` passthrough).

- `SESSION_SWAPPING_COMMANDS` = `["new_session", "switch_session", "fork",
  "clone"]`: the pi commands that would swap the session file underneath a
  live process. Refused by the daemon itself rather than left to client
  politeness, because "one pi process = one session file, always" is
  architectural (CLAUDE.md, docs/DAEMON.md), multiple clients are a
  first-class guarantee (the trusted client is not the only client), and the
  failure this prevents — a session file swapped under a running pi — is
  unrecoverable.
  - These are pi command `type` values, *not* `method` values like the dialog
    list used in `session.rs`.
  - **Pi-version-coupled**: verified against pi 0.83.0's
    `modes/rpc/rpc-types.ts:26,60,61,62` (the `RpcCommand` union) and its
    dispatch arms in `modes/rpc/rpc-mode.ts:432,600,608,616`. This is the one
    place the daemon deliberately knows pi's own vocabulary — re-check these
    exact strings on every pi upgrade (CLAUDE.md §Pi version discipline). A
    renamed command here silently disables the guard.
- `pi_passthrough` correlation flow:
  1. Validate the sid before anything else touches it — it's a remote string
     that becomes both a pid-table key and (elsewhere) a path component. The
     rejected sid is echoed back verbatim so the client can see what it sent.
  2. Session-swapping commands are refused *before* an id is injected and
     *before* anything is registered — such a command must never reach pi's
     stdin at all.
  3. A non-object command is rejected before registering a waiter: it can't
     carry the correlation id, so its response would never route back and the
     waiter would pend forever until the session exits.
  4. The response waiter is registered *before* the write, so the response
     can never race the waiter's registration. If the write then fails, the
     stale waiter entry is harmless: stdin being gone means pi is exiting
     anyway, and the pump drains the whole waiter map shortly after.
  5. The connection loop hands the wait off to its own spawned task
     (`answer_later`) rather than blocking — it must never head-of-line-block
     behind a pi command, since `bash`/`compact` are legitimately unbounded
     and `stop` has to stay reachable regardless.
  - Deliberately **no timeout**: pi's contract is "every command gets a
    response line or the process dies," and the pump fails every registered
    waiter on exit (mirroring pi's own RPC client's reject-pending-on-exit
    shape). A daemon-invented timeout would fabricate failures for
    slow-but-fine commands and could silently drop pi's real answer.
- `forward_dialog_answer`: `extension_ui_response` is fire-and-forget on pi's
  side — it resolves immediately with `response: null`. First answer wins:
  only the dialog's claim holder's answer is forwarded to pi; any other
  client's answer is dropped (docs/DAEMON.md dialog policy). A write failure
  here is logged loudly (not swallowed) because it means the winning answer
  never reached pi — the dialog policy's central invariant is directly at
  stake.
- `session_swap_refusal_message` names the invariant explicitly in the error
  text rather than just saying no, so the client author reading the error
  understands it's a hard rule, not a bug to work around.
- `pi_failure_payload` is the one place a *daemon-invented* pi answer is
  built. Every failure the daemon has to answer on pi's behalf — a rejected
  sid, an unknown session, a refused session-swap, a failed stdin write, a
  session that exited before responding — must be shaped like a pi response
  (`{"type": "response", "success": false, "error": …}`), because the client
  speaks Pi and does not know these came from us. It was written out twice
  before, once inside the `answer_later` future and once in `error_response`;
  one builder means the two cannot drift. There is no `command` field: pi
  echoes the command it answered, and the daemon has no command to echo on
  the paths where nothing reached pi.
- `SESSION_EXITED_BEFORE_RESPONDING`: the wording a client sees when the pump
  drops its waiter map on exit, named because it is the only pi "answer" the
  daemon authors for a command it did successfully write.

## crates/harvesterd/src/paths.rs

Every filesystem path the daemon touches is derived from `$HOME` in exactly
one place in this file (single source of truth for path construction).

- `socket()`: the client-daemon rendezvous point, spelled using the same
  constant (`harvester_proto::DAEMON_SOCKET_PATH`) the client itself reads, so
  the two sides structurally cannot disagree about where to meet.
- `harvester_dir()` (`~/.harvester`): the socket's parent directory. Its
  `0700` permission mode (enforced in `main.rs`) is the daemon's *entire*
  authentication story (docs/DAEMON.md §Transport) — anyone who can open the
  socket is trusted.
- `log_dir()`: where each session's `<sid>.stderr.log` lands. This is the only
  exit-diagnosis channel the client-daemon contract provides.
- `pi_sessions_root()` (`~/.pi/agent/sessions`): pi's own per-cwd session
  directories. Read-only from harvesterd's perspective — they are the actual
  source of truth (see fslist.rs and CLAUDE.md's disk-is-truth invariant).

## crates/harvesterd/src/server.rs

The controller layer: this module only frames, decodes, routes, and encodes
(house style, CLAUDE.md §Code style — "controller layer pattern"). Command
logic itself lives in `ops.rs` (harvester-native commands) and
`passthrough.rs` (pi passthrough).

- `OUTGOING_QUEUE_DEPTH` (256): outgoing lines queued per connection before
  backpressure kicks in.
- `Outbound::Typed` vs `::Raw`: everything flows through a single writer task
  per connection so responses and broadcast events never interleave
  mid-line — this is also what makes it safe for spawned handlers to answer
  out of request order, since the envelope's `id` is the correlation
  mechanism, not arrival order. `Raw` carries a pre-serialized wire line (sans
  trailing LF) shared straight from a session's event fan-out, written as-is
  and never re-serialized (avoids reserializing the same broadcast event once
  per subscriber).
- Connection teardown order in `serve_connection` matters: detach first
  (`attached.clear()`, which drops the forwarders' senders), *then* drop the
  connection's own `out_tx`. The writer task ends once the last sender is
  gone, having flushed everything already queued — aborting instead would
  discard responses a client that's half-closed its stdin is still waiting to
  read. Clearing the attached map *is* the detach: each dropped `Attachment`
  leaves its session's dialog "room," which is what triggers the empty-room
  auto-cancel dialog policy (docs/DAEMON.md).
- `answer_later`: lets a spawned task answer instead of the connection loop
  itself; the future yields a whole `Outgoing` because the answer's *variant*
  differs per caller (`HarvesterResponse` vs `PiResponse`).
- `decode_line` returning `None` means "nothing to dispatch" — either a blank
  line, or a parse error that has already been reported to the client inline.
- `RequestIdProbe` / `request_id_from_raw_line`: a line that fails to decode as
  a `Request` still has to be answered *with its `id`*. `serde_json` gives us no
  id from a failed decode (the whole value is discarded), so the raw bytes are
  re-parsed a second time through a struct that has exactly one optional field.
  This is the version-skew path — an unknown `cmd.type` from a newer client, a
  field an older daemon does not know — and the client's response router drops
  id-less responses while its pending waiters have no timeout, so answering
  `id: None` would hang that request forever rather than fail it. The probe
  keeps the id as a `Value`, not a `u64`: the daemon echoes what it was sent
  and never re-types it (only the *envelope* is typed). Yielding `None` for a
  line that is not JSON, is not a JSON object, or carries `"id": null` is
  correct — there is no id to correlate, and the error still reaches the
  client's stderr note. Documented in docs/DAEMON.md §Envelope.

## crates/harvesterd/src/session.rs

One pi child process per `Session`: owns spawn, stdin ownership, and the
stdout pump.

- **`SessionOutput::Event`** carries the complete pre-serialized wire envelope
  (`{"type":"event","sid":…,"event":…}`, no trailing LF) built once in the
  pump and shared by refcount (`Arc<[u8]>`) with every subscriber — never
  cloned or re-serialized per client.
- **`LineProbe`**: the only fields the pump routes on, borrowed straight out
  of the read buffer via `Cow`. Pi's `message_update` deltas each carry the
  *whole* partial message, so they're the largest and by far the most
  frequent lines; building an owned `Value` for every one of them just to
  read `type` would make the throwaway DOM the pump's dominant cost. The full
  tree is parsed in exactly one arm (`route_line`'s passthrough-response
  branch), where a client is actually waiting for it. `LineProbe::default()`
  is the "none of the routing fields apply" probe used as the fallback for a
  line that is valid JSON but doesn't fit the shape — so the line is still
  forwarded verbatim instead of vanishing.
  - `LineProbe.timeout` is milliseconds, present only on a dialog request: pi
    puts the extension's `opts.timeout` straight on the wire for `select`,
    `confirm`, and `input` (pi 0.83.0 `modes/rpc/rpc-mode.ts:137,142,147`,
    typed at `modes/rpc/rpc-types.ts:239-241,247`). `editor` never carries a
    timeout (`rpc-mode.ts:253-269`). Its presence means pi will resolve the
    dialog on its own — see `dialog::spawn_eviction`.
- **`Session` field notes**:
  - `cwd`: the pid table is the only record of a session's working directory
    until pi writes its first entry to the session file, so listing a
    fileless session requires it.
  - `spawned_at_ms`: stands in for the session file's mtime while there is no
    file yet — for a session that has done nothing, being spawned *is* its
    last activity.
  - `event_prefix`: the constant head of every event envelope this session
    emits (`{"type":"event","sid":<escaped sid>,"event":`), built once at
    spawn so `encode_event` is a byte concatenation instead of a second full
    JSON scan. **Soundness invariant**: `route_line` must only ever hand
    `encode_event` a line that already parsed as JSON, because the pi line is
    spliced in as raw bytes — a non-JSON line would corrupt the client's
    frame. `pump_stdout`/`probe_line` guarantee this by re-parsing as
    `&RawValue` before falling back to a default probe; do not weaken that
    without revisiting this invariant.
  - `stdin_queue`: private on purpose. Every write goes through
    `write_line`, and `pump_stdin` is the only holder of the actual pipe
    handle, so no caller ever awaits pi's stdin directly, and writes keep
    their arrival order across every connection. It becomes `None` after
    `close_stdin`: dropping the sender lets the pump drain what's queued and
    *then* drop the handle — that drop of the handle is what actually
    delivers EOF (`AsyncWriteExt::shutdown` is a mere flush for pipes; this
    was verified live — pi never saw EOF that way and had to be SIGKILLed).
  - `kill_switch`: fires the stop ladder's rung 3; `reap_child` listens on it.
  - `room`: who is watching and what is waiting on them, under one lock,
    because the dialog policy is a decision about both facts at once (see
    dialog.rs notes below).
  - `pending_responses`: in-flight passthrough waiters keyed by injected
    `hv-<n>` id. The pump resolves them; dropping the map wholesale on exit
    fails every waiter, mirroring pi's own reject-pending-requests-on-exit
    pattern.
- **`write_line`** is deliberately synchronous at the call site: the queue
  absorbs the write so no caller — least of all the connection loop — can be
  parked by a pi that stopped reading. A full queue reports "would block"
  immediately instead of parking the caller.
- **`close_stdin`** is the graceful path: drop the queue's sender, let the pump
  drain whatever is already queued (so a just-written `abort` still reaches
  pi), then drop the pipe handle — that's what delivers EOF, pi's
  documented-by-source shutdown path. The stdout pump observes the exit and
  broadcasts `Exit`.
- **`kill`** is the last resort after the stop grace period; only `reap_child`
  actually owns the `Child` and performs the SIGKILL.
- **`register_response_waiter`** must be called *before* the corresponding
  command is written to stdin, so the eventual response cannot race the
  registration.
- **`pump_stdin`** is the sole holder of pi's stdin pipe: it drains queued
  frames strictly in order, one flush per frame, so every writer's lines stay
  whole and arrive in order. It ends when the last sender is dropped
  (`close_stdin`) or the pipe breaks; dropping the handle on the way out is
  what delivers EOF.
- **`reap_child`** is the sole owner of the `Child`, so `Session::kill` needs
  no pid and no `kill(1)` binary — it races the child's own exit against the
  kill switch notification, because the stdout pump is parked on a read that
  only a dead pi will ever end.
- **`pump_stdout`**'s shutdown order is deliberate: collect the exit code,
  vacate the pid table, fail in-flight passthroughs, *then* broadcast `Exit`
  — in that order, so a client reacting to `session_exit` with an immediate
  resume can never race the pid table ("already running" becomes structurally
  impossible).
- **`probe_line`**: a line that fails to fit `LineProbe`'s shape (e.g. a
  non-string `type`/`id`/`method`, which pi 0.83.0 never emits but a later
  pi might) is not something the daemon can make a routing decision about —
  yet docs/DAEMON.md is unconditional that every stdout line pi emits is
  forwarded verbatim. So such a line is forwarded under the default probe
  rather than dropped; silently swallowing e.g. `agent_settled` would spin a
  client's turn forever. The `&RawValue` re-parse that distinguishes "not
  JSON" from "JSON but odd shape" only runs on the error path, so the pump's
  hot loop is unaffected.
- **`route_line`**'s passthrough-response arm is the one place that needs the
  whole parsed tree — it hands pi's full response object to the client
  waiting on it via `resolve_response`.
- **`passthrough_response_key`**: recognizes the `hv-<n>` id pattern minted by
  `Daemon::next_passthrough_id`, marking a line as an answer to an injected
  passthrough request rather than an ordinary session event.
- **`create_event_prefix`**: escapes the sid exactly as serde would, so the
  hand-built prefix is byte-identical to serializing `Outgoing::Event` (whose
  field order this matches) — verified by a dedicated test.
- **`encode_event`**: builds the wire envelope exactly once; every subscriber
  shares the resulting bytes, and a pending dialog keeps the same `Arc` for
  later replay. The pi line is spliced in verbatim (see the `event_prefix`
  soundness note above for what protects this).

Constants:
- `SESSION_EVENT_BUFFER` (1024): events buffered per session for slow
  subscribers before `RecvError::Lagged` kicks in.
- `STDIN_QUEUE_DEPTH` (64): frames queued for pi's stdin before writers are
  told to back off. Only a pi that stopped draining its stdin can fill this;
  failing fast beats queueing without bound, since the caller learns
  immediately instead of parking.

## crates/harvesterd/src/session/dialog.rs

Implements the dialog policy from docs/DAEMON.md §Dialogs: who is watching a
session, what dialogs are waiting on an answer, and what happens to a dialog
nobody can answer.

- `DIALOG_METHODS` = `select`, `confirm`, `input`, `editor` — the
  `extension_ui_request` methods that actually block pi until answered.
  Fire-and-forget kinds (`notify`, `setStatus`, …) are ordinary events, not
  dialogs.
- `TIMEOUT_EVICTION_SLACK` (5s): added to a dialog's own `timeout` before the
  daemon gives up on it. Pi's timer and the daemon's timer start at different
  moments — pi arms its timer before writing the request; the daemon's starts
  only when the line is read — so this slack keeps the daemon from forgetting
  a dialog pi is still genuinely waiting on (an answer arriving after
  eviction would never be forwarded). It's generous on purpose: being late
  costs nothing since the entry is already dead by then, but being early
  costs a real answer.
- **`DialogRoom`** keeps `attached` (a count) and `pending` (dialog id →
  pre-serialized envelope) under one mutex on purpose: "is anybody attached,
  and if so remember this dialog" must be one atomic decision. This was once
  split across a separate atomic and lock; a detach landing between the load
  and the insert could strand a dialog with nobody left to answer it and
  nothing left to trigger the cancel.
  - `attach()` replays every still-pending dialog's envelope to a newly
    attached watcher, so a client attaching mid-dialog can still answer it.
  - `detach()`: when the *last* watcher leaves, every still-pending dialog is
    returned for cancellation, since nothing could answer them anymore and pi
    would otherwise wait forever.
  - `remove_pending` is the one removal path shared by "answered" and
    "evicted" outcomes, so the two cannot drift apart in what they do to the
    room — only in what the caller does afterward (an answer is written to
    pi; an eviction never is). Returning `true` only when the dialog was
    still pending is what makes "first answer wins" hold.
  - `remember` refuses to record a dialog when the room is empty (`attached
    == 0`), signalling the caller to cancel instead — decided in the same
    critical section as the insert so the two outcomes are mutually
    exclusive.
- **`apply_policy`** returns `false` for exactly the case the caller cannot
  detect on its own: the dialog was auto-cancelled into an empty room, so the
  event must *not* also be broadcast (there's nobody to broadcast it to, and
  a later attacher would be shown a dialog that's already been answered).
  Note pi treats `timeout: 0` as "no timeout at all" (`if (opts?.timeout)` in
  `rpc-mode.ts:114`), so the daemon mirrors that — arming an eviction timer pi
  has no matching timer for would forget a live dialog.
- **`cancel`** writes an `extension_ui_response` with `cancelled: true`, which
  to the extension is indistinguishable from a human pressing Escape. Used
  both when a dialog arrives into an empty room and when the last watcher
  leaves.
- **`spawn_eviction`**: pi resolves a `timeout`-carrying dialog *itself* and
  emits **nothing** on stdout when it does (pi 0.83.0
  `modes/rpc/rpc-mode.ts:114-119`: the `setTimeout` callback runs `cleanup()`
  and `resolve(defaultValue)` with no `output(...)` call), and afterwards
  silently drops any later response for that id (`:768-776`). Nothing on the
  wire announces this, so a timer is the *only* way the daemon can learn a
  dialog resolved itself. The entry must be *evicted*, never cancelled: an
  `extension_ui_response` would be answering a dialog that no longer exists
  from pi's point of view. Without the timer the entry would be stale for the
  rest of the process's life — replayed to every later attacher, unanswerable,
  and cancelled for nothing at the eventual last detach.
  - The eviction timer holds only a **`Weak<Session>`**, never an `Arc`: a
    dialog may publish a timeout of hours, and the timer must not keep the
    `Session` (and with it the broadcast channel every subscriber holds)
    alive past the lifetime of the pi process it belongs to.

## crates/harvesterd/src/session/launch.rs

Builds the one pi launch line (docs/DAEMON.md §The pi launch line). Pinned to
pi 0.83.0 — `--session-id` (resume-or-create) is source-verified but
*undocumented* API, so re-verify this module on every pi upgrade (CLAUDE.md
§Pi version discipline).

- `create_pi_command` sets `PI_OFFLINE=1` to disable update checks and
  telemetry, plus two git guards (`GIT_TERMINAL_PROMPT=0` and a
  `BatchMode=yes` `GIT_SSH_COMMAND`) so a package install that wants
  credentials fails fast instead of hanging the spawn — this is the risk that
  passing `--approve` buys back.
- `open_stderr_log` **appends, never truncates**: since spawn is also resume,
  truncating would destroy the record of why the previous run died — exactly
  what a user is looking for when they click resume, and the sole
  exit-diagnosis channel the contract gives clients (docs/DAEMON.md §Events).
  The banner line written at open time is what makes appended runs separable
  when tailing the log.
- `MILLIS_PER_SECOND`: the spawn banner is stamped in whole seconds — the
  unit a `tail` of the log is read in, and the unit the contract names
  (docs/DAEMON.md §The pi launch line).

## crates/harvesterd/src/sid.rs

`Sid` is a session id that has been checked against pi's charset. A sid
arrives as an unvalidated remote string and is then used in five roles:
pid-table key, path component (`<sid>.stderr.log`), argv element, envelope
field, and wire field. Validating once, at the edge, is what keeps every one
of those roles honest — the type itself is the proof validation happened.

- `is_valid_session_id` is pi's own session-id rule, verbatim from
  `assertValidSessionId` (`session-manager.ts:212`, pi 0.83.0):
  `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$`. It's deliberately pi's
  charset and not "uuidv7", even though Harvester's own clients mint uuidv7:
  a narrower rule here would make every non-uuid session already on disk
  (pi's own naming is wider) unresumable through Harvester. Hand-rolled
  rather than pulled from a regex crate, since three character classes don't
  earn a dependency in a binary that ships as a static musl blob.
- `Sid` is `#[serde(transparent)]`: the wire representation is the plain
  string it always was, so the protocol is unchanged by the newtype's
  existence. The newtype exists purely so an unchecked string can't reach a
  path, an argv, or the pid table.
- `Sid::parse` takes `&str` rather than an owned `String` off the wire so a
  caller that must echo the *rejected* sid back in an error still owns it.
- `Borrow<str>` lets a `HashMap<Sid, _>` (e.g. `AttachedMap`) be looked up by
  `&str` without allocating a new `Sid`.
