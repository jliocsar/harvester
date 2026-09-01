# harvester-client-core — implementation notes

This codebase carries no code comments (see AGENTS.md). Everything a comment
would have said about `harvester-client-core` lives here instead, one section per module.
Update the relevant section in the same change that touches the code.

## crates/harvester-client-core/src/bootstrap.rs

First-connect bootstrap, per docs/DAEMON.md §Bootstrap & upgrade: probe the
machine, install/upgrade the static harvesterd binary, start it detached, then
connect and handshake. Written once against `Host::exec` — the local machine
runs the exact same scripts through `sh -c`, so install/upgrade policy cannot
drift between machine kinds.

Two different questions are asked here, and conflating them was a live bug
(co-design C4):
- *Is the binary over there the one I ship?* — answered by the build
  fingerprint; install iff it differs. This is what makes a rebuilt daemon
  actually reach the machine (see `plan_install` below).
- *Can these two peers talk?* — answered only by `PROTOCOL_VERSION` at the
  ping, never by the fingerprint.

`BOOTSTRAP_EXEC_TIMEOUT` (30s) bounds the probe and start execs, each about
one round trip. `INSTALL_TIMEOUT` (120s) is larger because the install exec
carries the binary upload over a possibly slow link plus the bounded
old-daemon wait (10s worst case).

`DAEMON_BINARY_PATH` / `DAEMON_LOG_PATH` are relative to `$HOME`
(tilde-expanded by the shell); the log path is appended to, not truncated, so
logs survive upgrades.

`KillRunningDaemon` is a dedicated enum rather than a bool so call sites read
as what they do.

`BootstrapOutcome::Upgraded` closes the old daemon's pi children's stdin as a
side effect of restarting it — acceptable by contract (stdin-close is the
sanctioned pi shutdown path; see the top-level architecture invariant on
never SIGTERM-ing pi).

`bootstrap_and_connect` is both the first-connect *and* every-connect
contract: the binary on disk is fingerprint-checked on every connect, and a
daemon still running an older image gets upgraded in place (exactly once).
After connecting, if the running daemon's reported build differs from what
was just confirmed/installed on disk, that means the file on disk is ours but
the process running is not — it was started before an upgrade landed, or the
disk check raced another client. Only a restart fixes that, so the function
forces the upgrade path exactly once more. If a second kill-reinstall-restart
still doesn't produce a matching build, this is deliberately **not** treated
as an error: the fingerprint is only a staleness heuristic, while what
actually governs whether two peers can talk is the protocol version checked
in `connected_if_protocol_matches`. Refusing here would brick a machine over
a benign race (another client installing a different build) or over a daemon
that can't fingerprint its own image and reports a sentinel — both recover on
a later connect, and neither is worth bricking a machine over. It still logs
loudly to stderr because a *permanent* mismatch is worth investigating.

`plan_install` is pure so the install-path policy is unit-testable without a
machine. Comparing build fingerprints rather than crate versions is the fix
for a real production incident: with the crate version as identity,
rebuilding harvesterd without bumping the workspace version left every
bootstrapped machine on the stale binary forever, because the probe saw a
version match and skipped the upload.

`connected_if_protocol_matches` is the only hard failure in the whole
bootstrap: a daemon just confirmed (or reinstalled) speaking a protocol
envelope this client does not understand. Unlike a build mismatch this can't
be repaired by installing again — the binary we ship *is* the wrong one — so
it's a hard `Error::VersionMismatch`, not a warning. What it hands back is a
`Connected` carrying the connection and the install outcome, and nothing
else: the protocol version it just asserted equals `PROTOCOL_VERSION` is a
constant of this build, so re-publishing it as a "daemon version" only ever
produced the string "1" in a costume. The daemon's real identity, when one
is wanted, is `ping.build` (the fingerprint), which bootstrap already reads
and judges here.

`probe` is one round trip that answers both bootstrap questions at once:
where `$HOME` is (the socket forward needs an absolute remote path — ssh does
not expand `~` in `-L`, and the local connect must join paths the same way),
and which build of harvesterd is on disk, if any.

`probe_script` / `parse_probe`: the probe's answer is **positional** — `$HOME`
on line one, the build fingerprint on line two. It's parsed as pure,
independently-tested logic rather than read inline off `ExecOutput` because
one stray line of daemon output shifts the fingerprint out of place, and a
fingerprint that never matches makes every future connect reinstall forever.
`parse_probe` returning `None` means the exec itself failed; the home line
prints even when the binary is missing (the `exec ~/path build` tail fails
with 126/127 *after* the home line already printed), so a *missing* home line
is the only signal that means the exec truly failed. Anything short of a
clean two-line answer (non-zero exit, empty build line, a daemon too old to
know the `build` subcommand) is read as "nothing usable installed", which
triggers a reinstall — the self-healing direction, since a needless upload is
the cheap failure. This needs no special case for pre-`harvesterd build`
daemons: they exit non-zero on the unknown argument, which naturally lands
here as `None` and retires them.

`start_script`: harvesterd does **not** self-daemonize — `setsid` plus the
redirects in this script are the entire daemonization story, on the local
machine too (a plain child would share the client's terminal and die with
it). It's idempotent: a second daemon simply loses the socket-bind race and
exits. The subshell around the trailing `&` is load-bearing: a bare trailing
`&` would background the *entire* preceding `&&` list, and POSIX gives async
lists stdin from `/dev/null` — which silently truncated the `cat` binary
upload to zero bytes when this script was appended directly to the install
chain (found live in production). That same backgrounding also makes the
daemon launch itself unobservable — the subshell exits 0 whether or not the
daemon ever actually ran — so the preflight checks ahead of it (setsid
present, binary runnable) carry the whole diagnosis. Without them, a missing
`setsid`, a wrong-architecture binary, or a truncated upload all surface only
as a connect timeout ten seconds later, with the real error stranded in the
remote log. The script stays a single `&&` chain (each guard braces its own
`exit`) because `install_script` appends it with `&&` — a bare `;` would let
`start` run even after a failed install.

`install_script`: receives the binary on stdin. The staging-file + `mv` dance
swaps atomically and dodges `ETXTBSY` from a still-running daemon. On
upgrades, sending the kill to the old daemon closes its pi children's stdin
pipes — their clean shutdown path — and the bounded wait (100 × 0.1s, then a
loud failure) keeps the new daemon from losing the bind race to a dying
predecessor rather than spinning forever on an unkillable process. The
staging file name is per-shell (`$$`) because multiple concurrent clients are
first-class: two of them upgrading the same machine at once would otherwise
interleave their writes into one path and `mv` a corrupt binary into place;
with `$$`, whoever finishes last still wins, but with a complete binary. The
`wc -c` size assertion is what makes "complete" actually true: `cat` cannot
distinguish a finished upload from a broken stdin pipe (both are EOF, both
exit 0), and a failed stdin write is non-fatal on our side (`Host::exec`), so
without the size check a truncated binary would be `chmod +x`'d and `mv`'d
into place exactly like a whole one. The atomic swap only ever protected
against `ETXTBSY` — never against a short write; the size check covers the
latter. Only the bin dir is created here; the log dir and the `~/.harvester`
`chmod 700` both come from the appended `start_script`, which needs them
anyway because `start()` (the no-install path) runs `start_script` alone.

The bootstrap test module docstring: these scripts are the least verifiable
code in the client — they only ever run inside someone else's remote shell,
and a broken one bricks every machine at once. The tests here check what
*can* be checked locally: that a real `/bin/sh -n` parses the generated
script (same check `sh -n providers/exe-dev` performs, for a string built at
runtime), and that the guards that must be present in the `&&` chain actually
are, in the right order (e.g. the size assertion strictly precedes `chmod +x`
and `mv`, so a truncated upload is never made executable or installed).

## crates/harvester-client-core/src/bridge.rs

The bridge is one long-lived Tokio task and one daemon connection per machine. All bridge tasks share the client runtime's current-thread reactor and remain GPUI-independent. `Command` is the runtime-to-bridge vocabulary; `EventSender` tags every event with the machine's bridge generation before it reaches `Fleet`. Pi commands and payloads remain `serde_json::Value`.

`attachments` is keyed by sid. Each `Attachment` owns its attachment generation and `get_entries` cursor, so one machine may keep multiple Pi rooms attached concurrently. `Open` and `StartSession` install the runtime-supplied generation; prompt, stop, dialog and generic Pi commands check that generation before crossing the daemon connection. `RoomJoined`, `RoomLeft`, replay, live Pi, exit, operation and prompt outcomes echo it. The runtime performs the second generation check before mutating a replica.

Attachment transitions use the persistent `attachment_changes` map of sid to async mutex. Open/start/repair for the same sid serialize, preventing a repair detach from racing a replacement generation. Different sids use different mutexes and therefore do not make a slow history fetch on one session block another session on the machine. Entries intentionally remain in this lock map for the bridge lifetime: deleting a key while an old flow retains its `Arc` could create two locks for one sid.

An attach registers its `(sid, generation)` replay window before sending daemon `attach`, then fetches full history, emits `Attached`, records `leafId`, and flushes held live events. A malformed success without an `entries` array is a history failure, never an authoritative empty transcript. `Attached.history_available = false` establishes the live room while instructing the runtime to retain any reconnect replica. The bridge reports `OpenHistory` failure and schedules repair.

After a direct `agent_settled`, the bridge registers a replay window before forwarding the boundary to the runtime, then fetches `get_entries {since}`. This makes persisted history authoritative and bounds the raw live suffix without letting a queued next turn slip between settlement and window registration. A settlement already held by another replay window schedules one follow-up sync after that window flushes.

Daemon `event_gap` and attach-window overflow use the same repair. `ResyncGuard` admits one repair per `(sid, generation)`. Repair marks the lifecycle observable, takes the sid lock, verifies ownership on every retry, detaches, registers a fresh replay window, reattaches, tries `get_entries {since: leafId}`, and falls back to a full `get_entries` for any delta error. A successful delta emits `HistoryExtended`; a full fetch emits `HistoryReplaced`. Cursor reads and writes are generation-filtered. Live events held after the selected snapshot are then delivered in order. Dialog room leave/join events surround detach/reattach so obligations cannot outlive daemon room ownership.

Prompt delivery has its own exactly-once arbitration. `spawn_prompt_flow` registers `SubmissionId -> (sid, generation)` before spawning the Pi request. Pi success or failure removes that registration before emitting `PromptDelivered` or `PromptFailed`. Connection loss atomically drains the same map and emits failures before `Disconnected`; a response task that wakes later finds no registration and emits nothing. This ordering lets the runtime distinguish bridge queue acceptance from Pi acceptance and prevents disconnect/response races from restoring a prompt Pi accepted.

`StartSession` uses daemon `create_session` with a generated uuidv7 sid, attaches, publishes the empty optimistic replica without replacing its first prompt, then enters the same registered prompt flow. Create remains resume-or-create. The bridge never sends Pi session-switching commands. Stop delegates to the daemon stop ladder; no client layer signals Pi.

`fetch_entries` accepts a nullable or absent `leafId`, but requires `entries` to be an array. Unknown future entries and live events are not interpreted here. The only Pi response fields the bridge reads are `success`, `error`, `data`, and the `type` needed to echo a generic request method.

`DISCONNECTED_PROMPT_DETAIL` is the stable local diagnosis for a submission whose bridge died before one Pi outcome won. `UNKNOWN_PI_ERROR` is used only when a failed Pi response omits its error string.

Focused fake-daemon tests drive list/open/attach, failed history and repair, multiple attached sessions, independent sid locks, delta/full repair behavior, prompt acknowledgement and disconnect arbitration, generation-scoped removal, and stale cursor rejection.

## crates/harvester-client-core/src/bridge/attach_window.rs

This pure synchronous module closes the attach snapshot/live race. `AttachRegistration` is an RAII owner for one `(sid, generation)` buffer registration. Dropping a cancelled or failed flow removes only that owner; flushing removes the shared entry and gives exactly one concurrent attach flow the buffered result.

`ATTACH_EVENT_BUFFER_CAP = 1024` bounds only the temporary replay window, not the runtime's raw live-event retention. At the cap, the buffer becomes `Overflowed` and stops pretending it has a complete sequence. Flushing overflow schedules generation-correlated history repair. A custom `LF`-independent structure is unnecessary here because values have already crossed JSONL framing.

Owner ids and attachment generations use checked increment rather than wrapping. `BufferKey` includes generation, so a stale registration cannot flush or remove a replacement attachment's events. Unit tests cover ordering, overflow, duplicate owners, RAII cleanup and stale-generation isolation.

## crates/harvester-client-core/src/connection.rs

The client ↔ harvesterd link: a Unix socket, reached directly on the local
machine and through an `ssh -N -L` forward on a remote one, framed as
LF-terminated JSONL (docs/DAEMON.md §Protocol).

**Constants and their rationale**
- `DAEMON_SOCKET` (`= harvester_proto::DAEMON_SOCKET_PATH`) is the daemon's
  socket path relative to `$HOME` — identical on remote (forwarded) and local
  (direct) hosts; the shared constant with `harvester-proto` is the rendezvous
  contract between client and daemon.
- `CONNECT_TIMEOUT` (10s) / `CONNECT_RETRY_INTERVAL` (250ms): how long and how
  often to retry connect+ping while a freshly started daemon is still binding
  its socket. Covers slow VMs; the normal case succeeds on the first try.
- `HANDSHAKE_PING_ID = 0`: the handshake ping is always the first request on a
  connection, so id 0 is unambiguous; `DaemonClient` (below, same file) seeds
  its request counter from it, so handshake and requests share one monotonic
  id space. Private since the merge — no other module reads it.
- `FORWARD_SOCKET_PREFIX`/`FORWARD_SOCKET_SUFFIX` (`harvester-` / `.sock`):
  forward socket files are named `harvester-<pid>-<hash>.sock`, built by
  `forward_socket_name` and read back by `forward_socket_owner_pid` — one
  writer and one reader of the shape, so the sweep cannot drift from
  `local_socket_path`. The suffix also keeps the sweep from touching ssh's
  own control sockets (`harvester-ssh-%C`, no extension) that live in the
  same directory.

**`Forward`**: a live `ssh -N -L` process plus the local socket file it bound.
Dropping it kills the process (`kill_on_drop`) and unlinks the file. The local
machine has no `Forward` — the daemon owns its socket directly there. The
`child` field is behind a `Mutex` only so liveness (`try_wait`, which needs
`&mut Child`) can be checked through the shared handle both connection halves
hold; the alternative — dropping the forward on every failed handshake —
would kill ssh mid retry-loop.

`ForwardHandle = Option<Arc<Forward>>` is what both connection halves keep
alive and what every failed connect attempt shares; `None` on the local
machine, where there is no forward to keep alive. The `Option` is outside the
`Arc` on purpose: "no forward" then costs no allocation and reads as one
`None` instead of an `Arc` that has to be dereferenced twice before the
question is even asked.

**`DaemonConnection::over`**: frames the protocol over any duplex stream,
boxed rather than generic. The stream kind is an implementation detail of
*connecting*: a type parameter here would propagate through `DaemonClient`,
every bridge flow and `Fleet` for a per-line cost that's noise next to an SSH
round trip. In exchange, a `tokio::io::duplex` pair stands in for a daemon in
tests, making everything above this layer testable without a VM.

**`raw` module** (`#[cfg(feature = "raw-protocol")]`): byte access to a live
connection for `harvester-cli`'s protocol debugger only. Not a framing
bypass — reads still go through `read_frame` and writes still terminate with
exactly one LF. What it skips is *typing*: a line the client can't parse must
print rather than end the stream, and a malformed frame must still reach the
daemon so its documented parse-error response is observable — neither is
expressible through the typed calls. It's a separate module rather than extra
methods on `DaemonReader`/`DaemonWriter` because a cargo feature is not a
boundary: features unify across a workspace build, so gating methods still
leaves them reachable on the production types from the gpui client. Naming
this module is what the shipped client has no reason to do and no way to do
by accident.

**`connect_host`**: the one place the two host kinds actually differ — a
remote daemon's socket is reached through the ssh forward, the local one
directly. Both converge on the same retry-until-the-ping-lands loop
(`connect_socket`) and hand back what the daemon says about itself (protocol
version, build fingerprint); judging that is bootstrap's job, not this
module's.

`connect_socket` retries connect+ping until `CONNECT_TIMEOUT` elapses (a
just-started daemon needs a beat to bind its socket), giving up early if the
forward process has already died. `bounded_handshake` caps one handshake
attempt at the connect deadline — without the cap, a daemon that accepts a
connection but never answers the ping would hang the bridge forever, since
the retry loop only re-checks the deadline on the error path. The forward
itself outlives every individual attempt; the common retry shape is "ssh
accepted the socket locally, then closed it immediately because the remote
daemon hasn't bound its socket yet" — that's exactly the EOF branch handled
in `handshake`.

`start_forward` builds the `Forward` from the spawn, not from a later success
path: the socket file is bound the instant ssh comes up, so building the
guard any later would orphan one socket file per expired connect attempt. It
removes a stale local socket file first (ssh refuses to bind over one).
Keepalive and `StreamLocalBindUnlink` options live in `SSH_BASE_ARGS`
(host.rs) rather than on this invocation: the *master* connection owns the
transport and the forward's listener, so passing mux-client-only options here
would have no effect — this bit the project once when both landed on the
forward invocation and silently never fired.

`local_socket_path` hashes the *full* SSH destination string, not a
sanitized/readable tag — a readable tag proved collision-prone (`web_1` and
`web.1` sanitized identically), which would make a second bridge unlink and
rebind over the first one's live forward, cross-wiring machines. Per-process
(pid in the name) and short by construction, since Unix `sun_path` maxes out
around 108 bytes.

`sweep_leaked_forward_sockets` reclaims forward socket files a previous
client process left behind without running `Forward::drop` (SIGKILL, OOM, a
panic-abort). The same-pid `remove_file` preamble in `start_forward` never
catches these because the next process has a different pid — without the
sweep they'd accumulate in `~/.harvester/run` forever, one per destination
per crash. It is a two-stage test, cheapest and least invasive first:

1. *Whose is it?* `forward_socket_owner_pid` reads the pid out of the name
   and `process_is_alive` asks whether that process still exists. A socket
   whose owner is alive is skipped untouched — it belongs to another running
   client (or to this one), and connecting to it would push a spurious
   accept-then-EOF through a forward somebody is using. A name we cannot
   read an owner out of is left alone and noted on stderr: it is not one of
   ours in the shape we write today, and deleting other people's files is
   not the sweep's job.
2. *Is anyone listening?* `remove_if_refused` connects and treats only
   `ECONNREFUSED` as proof of a dead listener; every other error
   (permissions, a path that stopped being a socket) is a reason to leave
   the file alone.

Both stages are needed. The connect test alone is what makes the sweep
pid-reuse-proof — a recycled pid would otherwise strand a real leak — but on
its own it pokes every live client's forward on every startup. The pid gate
alone would be a filename believing itself. In the one case where they
disagree (a leaked socket whose pid has since been reused by an unrelated
process) the sweep does nothing, which is the conservative half of the
trade: a stale file survives until the next sweep after that pid dies.
`reclaim_if_leaked` is both stages for one candidate file, leaving the sweep
loop itself as only "is this one of ours? then consider reclaiming it".

`process_is_alive` shells out to `sh -c 'kill -0 <pid>'` (`pid_liveness_script`)
rather than pulling `libc` in for one syscall — the same trade the project
makes elsewhere, and `kill` is a shell builtin so nothing has to be
installed. A probe that fails to run at all is read as *alive*, because
"cannot tell" must not become "delete it".

**`DaemonClient` and `SessionEvent`** (merged in from the former
`client.rs` — one socket, one file).

Request/response correlation for one daemon connection. Per docs/DAEMON.md:
requests carry an `id`, the response echoes it, and arrival order on the wire
is not guaranteed. `DaemonClient` resolves requests at their `await` point via
a pending map — the same shape pi's own RPC client uses internally — so every
command flow in the client reads as straight-line async/await rather than a
hand-rolled continuation state machine. Unsolicited daemon lines (pi events,
`session_exit`, gap notices) flow out on a separate channel (`SessionEvent`)
instead of through the pending map, since nothing is awaiting them.

`SessionEvent::EventGap`: the daemon dropped events for this connection
because the client read too slowly. Whatever state the client holds for that
`sid` is now incomplete and must be refetched rather than trusted as
continuous.

`ClientInner.writer` is guarded by an **async** mutex because the guard is
held across the socket write itself (a sync mutex cannot be held across an
`.await`).

`ClientInner.pending` is a **std** (sync) mutex, deliberately never held
across an await — it's only touched for quick insert/remove operations. The
value stored is the whole daemon line/envelope, not a pre-classified payload:
the router only knows the request id, and the original requester already
knows which envelope shape it sent, so no double-parsing is needed.

`last_request_id` starts at the handshake ping's id (`HANDSHAKE_PING_ID`,
defined above), so the first real request issued is the one immediately after
it — request ids share one monotonic sequence with the handshake instead of
restarting. `a_client_request_resolves_when_the_daemon_echoes_its_id` pins
both halves: the sequencing and the resolve-at-the-await-point round trip.

`DaemonClient::start` splits a `DaemonConnection` into a background routing
task (owns the reader) and a client handle (writer + pending map). Dropping
both the client handle and the event receiver is what tears the connection
(and any ssh port forward backing it) down — there's no separate explicit
teardown call.

`resolve`: when a pending waiter for a response id can't be found, that means
the requester's task already gave up (e.g. it was dropped mid-await) — this is
logged as noteworthy but is not treated as fatal, since the daemon did nothing
wrong.

The `PendingRequestGuard` RAII guard (renamed from `Pending` in the merge,
because the crate publicly exports the unrelated `token::Pending<T>` and two
meanings of one name in one crate was a review finding waiting to happen)
exists because the ways a request's pending-map entry needs cleaning up outnumber the
ways it resolves normally: a failed write, a cancelled future (the caller's
task dropping mid-await — which the UI does routinely whenever a flow is
abandoned), or ordinary resolution. Without RAII, a leaked entry means a
waiter that can never be woken and a `HashMap` that only grows — so removal
cannot safely live at a single call site and must be tied to the guard's drop.

Every acquisition of the pending map — and of `Forward.child` — goes through
`harvester_proto::lock`, which panics with the field's own name and the
poison detail. This crate used to carry four three-line `lock_*` wrappers
(`lock_pending` here, `lock_cursor`/`lock_resyncing` in `bridge.rs`,
`lock_attach_buffers` in `bridge/attach_window.rs`) that were near-identical
to three more in the daemon; there is one now, in the crate both sides already
depend on. The convention is to pass the mutex field's own name, with no
`" mutex"` suffix — the helper adds it.

`envelope_mismatch`: the daemon echoing a request's id back on the *wrong*
envelope type (e.g. a `Harvester` request id showing up on a `PiResponse`) is
a daemon-contract violation — impossible under docs/DAEMON.md's contract, so
it's surfaced as an error to report rather than something the client tries to
recover from.

The `#[cfg(test)]` helpers `next_line_for_test`/`send_json_for_test` stay
`pub(crate)`: they are the fake-daemon vocabulary `bridge.rs`'s `Harness`
drives its tests with, so they cannot narrow to private.

**Cross-reference**: docs/DAEMON.md §Protocol, §ping.

## crates/harvester-client-core/src/dialog.rs

`DialogRequest` parses Pi's blocking `extension_ui_request` methods named by the shared protocol dialog-method set. The raw request id is retained; only the fields required to render and answer confirm/select/input/editor are extracted. Unknown or malformed requests are not invented into answerable dialogs.

`DialogObligations` is window-free runtime state. Active rooms are a `HashSet<SessionKey>`, so every attached session on every machine may hold its own queued obligations. The global `VecDeque` gives Root's one modal surface a deterministic FIFO. Identity is `(SessionKey, dialog id)`; duplicate replay is ignored.

`enter_room` and `leave_room` follow bridge `RoomJoined`/`RoomLeft`. Leaving one room removes only that session's obligations; `leave_machine` removes only rooms and obligations on that machine. `answer_front` requires the exact `DialogKey`. The runtime records one in-flight answer per key and retires the local front only after Pi acknowledges success; rejection preserves the obligation and emits `DialogAnswerFailed` so the presenting window can reopen it for retry.

The daemon remains responsible for auto-cancelling when no client room is attached. The client never leaves an obligation parked merely because another session is also attached.

## crates/harvester-client-core/src/fleet.rs

The fleet: every machine the client knows about, what is known about each
one, and the policy that decides when to connect one. This is the layer
above `bridge` and `machines` — those two each own one machine's worth of
work (a daemon connection, a provider listing, a probe), but nobody owned
the cross-machine questions: which machines exist, which of them have a
bridge, whether a bridge should be started at all, and what a machine's dot
means. Those questions used to live in the gpui crate's `workbench.rs`,
where a second frontend could not reach them.

### Reactor boundary

The fleet is owned by the runtime engine inside its current-thread Tokio reactor. Its methods stay synchronous and non-blocking: commands go through unbounded channels, while `FleetEvents` returns already-correlated bridge and machines events to the engine's select loop. Fleet starts async tasks but neither stores a runtime nor exposes Tokio primitives to the desktop. GPUI observes only `ClientRuntime` snapshots and runtime events.

### Probe verdicts and daemon links are one state, not two

They used to be two maps (`connections` and `probes`) that could disagree,
with a precedence rule written only in the renderer that read them. Now one
`Machine` row is either `Probed` or `Linked`, the `BridgeHandle` lives
*inside* the linked variant, and the precedence rule is enforced at the one
place the row is written (`Machine::record_probe`): a probe verdict never
overwrites a daemon link, because an actual connection outranks
`ssh <machine> true`. A frontend reads one `MachineState` and renders it —
there is no second field to keep in sync, and the type makes it impossible
to claim a link without holding the bridge that backs it. Absence from the
table is a third case (nothing known at all) and needs no variant.

`Link` (reachable only through `Machine::Linked`) models what a bridge has
said about its daemon: `Connecting` (up, bootstrapping or handshaking),
`Connected`, `Failed { detail }` (the bridge reported why it could not
connect — the detail is the user's *only* diagnosis, so it is carried, never
discarded), and `Lost` (connected once, then lost the daemon). Only `Failed`
carries a payload: `Connected` used to carry a `daemon_version` that was
always the string "1", because bootstrap asserts the daemon's protocol
version equals ours and then had nothing else to report. A machine that is
connected is connected; the statusbar names the machine, not a constant.

`Machine::Linked` keeps its `BridgeHandle` even after a failure, for two reasons: sending through the closed channel produces an explicit stopped-bridge error, and retouching the machine (`ensure_connection`) is the single retry path for failed or lost bridges.

### `MachineState`

The borrowed, render-ready view of one machine's row. Three sources speak
here, in ascending precedence: `Missing` is what the provider *listing*
says (deleted upstream — distinct from `Unreachable`, which is a machine
that still exists and simply didn't answer a probe); the next three
(`Unknown`, `Reachable`, `Unreachable`) are what a probe says; the last four
(`Connecting`, `Connected`, `ConnectFailed`, `ConnectionLost`) are what a
bridge says. A machine moves up into the bridge half once a bridge is
started for it and never falls back out — a probe is weaker evidence than an
actual connection, and a machine whose connect failed stays visibly failed
until something retries it. `is_live()` (`Connecting` or `Connected`) is the
one predicate that means "usable — commands queue on it."

### Fleet failures

`Fleet`'s fallible methods (`ensure_connection`, `host_for`, `send_host_command`) return `crate::Result`. Their failures — `NotConnectable`, `HostWorkerMissing` and `HostWorkerStopped` — are variants of `crate::Error`; bridge startup itself is infallible once the runtime reactor exists.

Every one of them renders the exact sentence the statusbar showed before any
of this was typed at all. The human-readable `detail` string *is* the payload
— a frontend that collapsed this down to "not connectable" would throw away
the only diagnosis a user gets for a machine that will not come up.
`HostWorkerMissing` vs `HostWorkerStopped` distinguishes an inert test fleet with no machines task from a runtime whose machines task has ended.

### `Unapplied<T>` — making a footgun unrepresentable

Bridge and machines events are wrapped in opaque `Unapplied<T>` values. The runtime engine can reach a value only through `Fleet::observe_bridge_event` or `observe_machines_event`, which updates fleet state first. This makes routing an event before recording its machine state a compile error instead of a convention.

### `Fleet` fields (why, not what)

- `machines: HashMap<String, Machine>` — every machine the client has an
  opinion about (probed, connected, or both). Absent means nothing is known
  yet.
- `provider_rows: Vec<MachineRow>` — the last listing cycle's rows, sorted
  by key.
- `listed_providers: HashSet<String>` — the providers whose listings can be
  believed (see `Fleet::is_missing`); empty until the first listing cycle
  reports.
- `tracked: Vec<String>` — the machines a frontend told the fleet to care
  about (`Fleet::track_machines`), deduplicated and sorted. The ad-hoc
  subset is *derived* from this on every read (`ad_hoc_machines()`), never
  stored separately — it used to be a second copy that only one call site
  recomputed, so any other way of mutating `tracked` silently desynced it.

### Policy methods

- `track_machines`: the machines the client cares about — every stored
  workspace's machine plus the argv destination. Call it again whenever
  that set changes. Two policies hang off this single list, both owned by
  the fleet: which ad-hoc destinations the worker should probe, and which
  machines get a background auto-connect so a reopened client shows its
  sessions again without being asked (docs/SPEC.md §Flows "Reopen client").
- `forget_unknown_machines`: drops every machine the client no longer knows
  about (neither tracked nor listed). Without this, a machine deleted
  upstream stayed `Connected` forever and kept being counted in
  `linked_machine_count`; dropping the row also drops its command handle,
  which lets the bridge task end.
- `auto_connect_tracked` / `connectable_tracked_machines`: connects every
  tracked machine that has no bridge yet *and* resolves to a host. Quiet by
  design — an unresolvable machine (a provider that hasn't listed yet) is
  skipped without an error, and the next listing cycle retries it. A
  machine whose bridge already *failed* is deliberately **not** retried
  here; retouching it via `ensure_connection` is the retry mechanism, and a
  5-minute listing cycle is not a retouch. Being quiet is correct here and
  wrong for the user at the same time — that split is exactly why
  `missing_tracked_machines()` exists as a separate, user-facing answer to
  "is this skip permanent?" `connectable_tracked_machines` is split out
  purely so the borrow of `self.tracked` ends before connecting, and because
  it is the entire policy in one place — testable without starting a
  network task.
- `ensure_connection` / `connect_decision` / `spawn_bridge`: lazily
  connects. Reuses a live bridge, respawns a failed-or-lost one (retouching
  a machine is the retry, by design), spawns a fresh one otherwise. The
  decision is computed in a pure `connect_decision` before anything is
  spawned, which is what makes the connect policy unit-testable without
  network tasks (see the `ConnectDecision` enum, tested directly in `tests`).
  `spawn_bridge` unconditionally replaces any previous handle in the table —
  that replacement is what makes "retouch a failed machine" equivalent to
  "retry it."
- `send` / `send_host_command`: `send` routes a command to a machine's
  bridge. Link *state* still comes from link events, never from the send's
  return value, but both ways of losing a command (no row/no bridge, or a
  bridge task whose command channel has closed) return an error rather than
  being swallowed. This is the "forget racing a send" path, and a command
  that evaporates in silence is indistinguishable from one the daemon ignored.
  The command's own payload is deliberately not logged — a dropped prompt's
  text is the user's, not the log's. Host ops
  (`$HOME`, `ls`, `git rev-parse`) go through `send_host_command` to the
  machines worker instead, because they never touch a daemon
  (docs/SPEC.md §Workspace, "no daemon or provider involvement") — so they
  bypass the bridge entirely and don't need one to exist.

### Event application

- `observe_bridge_event` / `observe_machines_event` are the only doors into
  the `Unapplied` wrapper — see above.
- `record_bridge_event` only reacts to the link-relevant subset of
  `bridge::Event` (`Connected`, `ConnectFailed`, `Disconnected`); everything
  else is session/request traffic the fleet has no opinion about and passes
  through untouched to the frontend's own routing.
- `set_link` is only ever called for a machine whose bridge the fleet itself
  started, so the row is expected to exist and be linked; the `else` branch
  (log to stderr) exists purely so a stray/out-of-order event can't panic a
  frontend.
- `record_machines_event` takes only `Listed` and `Probed`; `ProviderFailed`
  is statusbar material and the host-op answers belong to whoever asked —
  both are left for the frontend to handle directly.
- `handle_listed`: a machine that vanished from a fresh listing is gone; a
  provider-key tracked machine that just *appeared* in it becomes newly
  resolvable, so this also re-runs `forget_unknown_machines` and
  `auto_connect_tracked` — that's how a workspace's sessions "show up" the
  moment its machine's provider finally lists it.

### Read-side methods

- `state()`: a live bridge outranks the listing — "we are talking to the
  machine, whatever its provider currently says about it." Nothing weaker
  does: a probe verdict for a deleted machine is stale by construction, and
  reporting it would be exactly the silence `MachineState::Missing` exists
  to break.
- `is_missing()`: true only when the machine's provider has listed
  successfully *and* did not include it (deleted upstream). A provider that
  has never listed says nothing about its machines — an absent row is not
  evidence until the provider has actually listed at least once, otherwise
  one broken/never-run provider would report its entire fleet as deleted.
  Only provider machines can go missing: `local` always exists, and nothing
  "lists" an ad-hoc SSH destination in a way that could leave it out.
- `missing_tracked_machines()`: the tracked machines that no longer exist —
  a frontend's cue to still show them (rather than silently dropping the
  sidebar row) while their workspace goes on looking otherwise healthy;
  without this the client would sit there connecting to nothing with no way
  for the user to find out why.
- `host_for()`: the one place a machine string resolves to the `Host` its
  execs actually run on, routed through `MachineRef`. An error means either
  the provider key is unknown (not listed yet) or the machine uses the
  unsupported argv ssh form (no target).
- `label()`: provider machine's listed label when available, falling back
  to the key's id half before the provider has listed; the raw string for
  ad-hoc destinations and `local`.
- `machine_candidates()`: "connectable" is defined as *exactly*
  `Fleet::host_for` succeeding, and nothing else — this used to be spelled
  twice (once as "resolves to a host," once as a re-derived provider-target
  filter here), and the two implementations disagreed about ad-hoc
  destinations. Now there is one definition.
- `linked_machine_count()` / `connected_machine_count()`: `linked` counts
  any machine with a bridge at all (the denominator a frontend counts
  connections against); probe-only machines don't count because nobody
  asked them to connect. `connected` counts only `Link::Connected` — a probe
  is not a connection attempt.

### Recorded follow-up

None beyond what is captured above; unresolved behavior belongs in the contract docs.

### Machine identity

This was `machine.rs` until it folded in here — `MachineRef` never had a
caller outside the fleet, so it now lives beside its one consumer and is
private to this file.

Machine identity (docs/SPEC.md §Machine): the trichotomy that decides what a
stored machine string refers to, and therefore which host the client execs
on. This is decided in exactly one place because `Host::Local` is already a
client-core concept, and a frontend re-deriving these semantics from
`MachineKey` alone would drift from them.

- `LOCAL_MACHINE = "local"` is the hardcoded local machine's key. It is not a
  provider machine and is never deletable: harvesterd runs as a detached
  child on the local machine, with no SSH involved.
- `MachineRef` is what a stored machine string (workspaces.json, or the argv
  launch destination) refers to — total by construction. Every string is one
  of three kinds; an unresolvable *reference* is still well-formed, and
  whether it resolves to an actual host is the caller's separate concern:
  - `Local` — the hardcoded local machine.
  - `Provider(MachineKey)` — a `<provider>/<id>` key. Connection info comes
    from that provider's `list`, so such a machine is only resolvable once
    its provider has been listed. Parsing splits on the *first* `/`, so an
    id may itself contain slashes — the provider name is the one part
    Harvester owns the format of.
  - `AdHoc(String)` — a plain SSH destination used verbatim: the argv launch
    form and legacy keyless workspaces.json entries.
- Gotcha: a provider literally named `local` (`local/vm`) still parses as
  `Provider`, not `Local` — only the bare string `"local"` matches the
  local-machine case. `"localhost"` is `AdHoc`, not `Local`.

## crates/harvester-client-core/src/host.rs

The one exec boundary for both machine kinds — local and SSH
(docs/SPEC.md §Machine). Every control-plane script runs through `sh` either
way: `sh -c` locally, `exec sh -c ...` over ssh on the remote side. This
means bootstrap and workspace-browsing code is written once against
`Host::exec` and can't drift between the two arms, and doesn't depend on the
remote user's login shell. The socket-connect split (forward vs. direct)
lives in `connection.rs`, not here — this module is exec only.

`Host::exec` always carries an explicit `timeout`: ssh's own
`ConnectTimeout` bounds only the TCP connect step, so without a caller-side
deadline here a wedged transport or an unbounded remote loop would hang its
bridge forever. It applies that deadline by handing the built command to
`run_within`, which is where "every child process this crate spawns is killed
at a deadline" is actually written down. On the SSH arm, the remote command is re-executed through
`exec sh -c <quoted script>` rather than run directly, because sshd runs
commands through the user's *login* shell, which is only POSIX-compatible by
luck — fish and csh reject the arithmetic, async-subshell and `&&`-chain
constructs the project's scripts are written in. Re-execing through `sh`
guarantees both arms really get the POSIX shell this module promises.

`SshTarget.options` is the provider contract's `ssh.options` (PROVIDERS.md
§list), each entry emitted as one `-o` argument. They are appended *after*
`SSH_BASE_ARGS` and after `ControlPath`, and the order is the whole safety
argument: ssh uses the first value it obtains for a setting, so a provider can
add a `ProxyCommand` for a machine that is not directly routable, but cannot
turn off `BatchMode` (which is what stops a provider's machine hanging the app
on a password prompt) or redirect the control socket. Every other `SshTarget`
in the crate is built with an empty `options` — an ad-hoc `--machine` argument
and the CLI's `--dest` name a target the user's own ssh config already
resolves.

`HOME_SCRIPT` uses `printf '%s\n' "$HOME"` rather than `echo`, because
`echo`'s handling of backslashes is shell-dependent (differs across
`sh` implementations); `printf` is not.

`parse_home` trims the first line (a trailing `\r` is invisible in a path
but fatal when substituted into an `ssh -L` forward spec) and rejects
non-absolute results, because a relative `$HOME` would send the folder
picker walking from nowhere and make a forward spec meaningless.

`shell_quote` single-quotes a value for POSIX shells using the `'\''`
escape dance, so arbitrary characters survive a command-line round trip.
It lives next to the exec boundary because that's exactly what it protects:
both the scripts passed to `Host::exec` and any paths interpolated into
them.

`folder_name` is the client's one "last component of a path", re-exported from
`lib.rs` because both halves of the `harvester` crate call it and they cannot
see each other: `ui/transcript.rs` (a file's name in a tool header) is on that
crate's lib side, and its sidebar, chat and repo control are on the bin side.
This crate is the lowest one both already depend on. It trims a trailing slash
and answers `"/"` for the root, because the two implementations it replaces
disagreed on exactly those cases — the weaker one rendered a stored repo path
ending in `/` as an empty sidebar subtitle. The four pinned cases travel with
it as a unit test here. It takes a path as `&str` and returns a borrow, so it
is a display helper with no allocation and no `Path` semantics: nothing here
touches the filesystem or resolves `..`.

`ExecOutput::require_success` is the crate's single "did this exec work?"
check: `Ok(())` on exit 0, otherwise the crate's `Error::Exec` carrying a
caller-supplied `context` plus the child's own stdout/stderr diagnosis
(`describe_failure`). Every non-zero exit anywhere in the crate is reported
through this path, so the shape of an exec failure can't drift between call
sites.

`run_within` is the crate's one exec entry point: `tokio::time::timeout`
around `run`, turning an expired deadline into an `io::Error` of kind
`TimedOut`. Both callers reach it — `Host::exec`, which wraps a script in
`sh -c`, and `Provider::invoke`, which spawns a bare provider executable and
therefore bypasses the `sh -c` wrapping entirely. They used to add their own
`tokio::time::timeout` independently, each with its own error text, which made
the deadline a convention two call sites happened to follow rather than a
property of the boundary. `run` itself is private now; nothing outside this
module can spawn a child without a deadline.

`run` spawns the command with `kill_on_drop` (cancelling the returned future
kills the child; nothing lingers) and feeds `stdin_data`, if any, on its own
spawned task rather than inline before draining output. EOF is signaled by
dropping the stdin handle — a mere `shutdown()` never closes a pipe fd,
which is the same fact that once bit harvesterd's own stop path. Feeding
stdin concurrently with draining stdout/stderr matters because doing them
sequentially deadlocks whenever the child fills its output pipe before it
has consumed all of stdin: the child blocks writing its output, Harvester
blocks writing stdin, and nobody is reading — exactly the shape the
multi-megabyte daemon-upload exec hits.

The test module uses `Host::Local` as a real in-process test double for the
whole exec boundary: the scripts under test are the *same strings* the SSH
arm would send, and the only difference between the two arms is which shell
ultimately runs them. Notable coverage: the `shell_quote` round trip
(including an explicit shell-injection attempt via an unbalanced `'`), a
non-zero exit's diagnosis, concurrent stdin-feed/output-drain under a
multi-megabyte payload (the deadlock scenario above), and deadline
enforcement on a script that never terminates.

**Cross-reference**: docs/SPEC.md §Machine; docs/DAEMON.md (stdin-close
shutdown discipline, referenced by analogy in `run`'s EOF-via-drop
comment).

### SSH targets

This was `ssh.rs` until it folded in here — addressing and the exec boundary
that uses it are one module now.

Plain-SSH plumbing shared by this module's own one-shot execs and the
long-lived socket forward (`connection.rs`): target addressing plus the base
ssh invocation.

`SshTarget.dest` is handed to ssh verbatim, so both `user@host` forms and
ssh-config aliases work unmodified.

`SSH_BASE_ARGS` is non-interactive by contract — the client must fail fast,
never hang on a password or host-key prompt (`BatchMode=yes`).
`StrictHostKeyChecking=accept-new` trusts a first-seen host key without
prompting (otherwise fresh VMs would be unreachable under `BatchMode`) but
still hard-fails if a *known* key changes.

Multiplexing (`ControlMaster=auto` + `ControlPath`): the first connection to
a machine becomes the mux master and every later exec rides it, skipping the
SSH handshake — measured on exe.dev, a listing costs ~1.7s cold vs. ~0.35s
multiplexed, which is what makes the folder picker's per-click `ls` calls
affordable. `auto` degrades to a plain (non-muxed) connection whenever the
master is unusable, so multiplexing is pure speedup, never a new failure
mode. `ControlPersist=60`: the master lingers 60s past its last client, then
exits on its own — nothing for Harvester to manage or clean up.
`StreamLocalBindUnlink=yes`: unlinks a leftover forward socket file before
binding, so a respawn racing a dying forward's teardown doesn't fail with
"Address already in use."

`ServerAliveInterval=15` / `ServerAliveCountMax=3`: a dropped link that never
sends a TCP RST (laptop sleep, wifi switch, VM migration) is otherwise
invisible — requests carry no deadline of their own, so a pending one would
hang until TCP itself gave up, with the machine still showing as connected.
Three missed 15s keepalive probes end the connection, EOF-ing the socket and
surfacing as a real disconnect within ~45s.

Everything transport-level (keepalives, `StreamLocalBindUnlink`) belongs on
`SSH_BASE_ARGS` rather than on an individual invocation, because whichever
process becomes the mux *master* owns the connection — options about the
transport or about a forward's listener are the master's to apply and are
silently ignored on a mux client. (Both options landed on the forward
invocation once, where — since the forward is usually the client, not the
master — they could never fire; this is the reasoning that fixed that bug.)

`CONTROL_SOCKET_TEMPLATE = "harvester-ssh-%C"`: `%C` is ssh's own short hash
of (local host, remote host, port, user), giving one control socket per
distinct target, well under the ~108-byte `sun_path` limit.

`client_socket_dir` resolves and memoizes (`OnceLock`) `~/.harvester/run`
(mode 0700) once per process. Control sockets and forward sockets must never
live in world-writable `/tmp`: a predictable path there could be squatted
before Harvester binds it, and ssh's mux protocol is itself unauthenticated
— a squatted/fake mux server would MITM every exec. This mirrors the
daemon-side policy of using filesystem permissions for socket auth
(docs/DAEMON.md). Falls back to the OS temp dir (with a loud stderr note)
only when `$HOME` is unusable. It's memoized because it sits on the path of
every ssh invocation the client builds (every machine probe, every
folder-picker click), and re-resolving meant a `create_dir_all` +
`set_permissions` pair on each one.

`SshTarget::command` builds the actual ssh invocation; `extra_args` are inserted before the destination because ssh requires options before the target argument.

## crates/harvester-client-core/src/lib.rs

The crate is the GPUI-independent client boundary. `ClientRuntimeOwner::start` is the primary entry point. The public surface re-exports typed identities, snapshots, runtime events, dialog answers, model choices, raw/derived session replicas, transcript presentation data and workspace ids. Worker implementation types remain `pub(crate)`.

The runtime is process-owned; a frontend clones `ClientRuntime`, reads immutable `Arc<ClientSnapshot>` values, subscribes to runtime events, and submits typed intents. GPUI does not appear in this crate. Every I/O task that needs Tokio runs under the one runtime engine reactor; startup and terminal failures are surfaced in the snapshot rather than swallowed.

`LOCAL_MACHINE` is the one wire/storage key for the local host. `Error` remains only where callers match its variants; runtime-to-desktop failures cross as display strings and semantic events.

## crates/harvester-client-core/src/machines.rs

The machines worker is one long-lived task on the client runtime's Tokio reactor. It owns everything that execs on a machine *without* a daemon: one-shot provider executables, SSH probes, folder-picker path work and branch resolution.

It enumerates `~/.harvester/providers/` (docs/PROVIDERS.md), lists each
provider's machines on a slow cycle, and drives the sidebar dots with
Harvester's own `ssh <machine> true` probe on a faster cycle — never with
provider-reported status (docs/SPEC.md §Machine).

It also answers the host ops the repo picker needs (`$HOME`, `ls`,
`git rev-parse --git-dir`) and the one the chat's branch indicator needs
(`git rev-parse --abbrev-ref HEAD`). Those are plain `Host` execs —
docs/SPEC.md §Workspace: "no daemon or provider involvement" — so they are
keyed by `Host`, not by machine key, and correlate their replies with a
`RequestToken` rather than by echoing the path back. Routing them through a
machine's bridge is what used to make adding a workspace on a fresh machine
wait out a full harvesterd bootstrap.

Cadence constants (why these values, not what they are):
- `PROBE_INTERVAL` = 30s — sidebar-dot probe cadence. A warm multiplexed SSH
  probe is one network round trip, so this is cheap to run often.
- `PROBE_TIMEOUT` = 15s — ceiling on the liveness probe. ssh's
  `ConnectTimeout=10` bounds only the TCP connect, so auth and the exec
  itself need their own stop, hence a timeout longer than the connect bound.
- `LIST_INTERVAL` = 300s (5 min) — provider `list` cadence. Machine sets
  change rarely, and each cycle costs one control-plane exec per provider
  (e.g. exe-dev: an `ssh exe.dev ls`).

Both intervals fire immediately on their first tick, so startup does one
list cycle and one probe cycle for free, without extra bootstrap code.

`Command`/`Event` are the worker's runtime-facing protocol. Every `Command` that is answered carries the `RequestToken` its answer echoes; `SetAdHocMachines` has no answer, so it carries none. On the `Event` side, host-op answers carry only the requesting token, so the engine applies an answer only while that token is still current; stale in-flight requests are ignored. All four answers are shaped `result: Result<_, String>`;
`WorkspaceValidated` used to be `error: Option<String>` because
`validate_git_workspace` returns `crate::Result<()>` and `Option` was the
shorter way to spell "nothing to hand back on success". One sibling in four
spelling the same thing differently cost its spawner a `.err().map(...)`
where the other three use `.map_err(...)`, and its consumer a second shape to
learn.

`Event::Listed.listed_providers`: every provider whose `list` has succeeded
at least once, this cycle or an earlier one. A provider absent from this set
has never reported, so its machines' absence from `rows` proves nothing —
this is the whole difference between "that machine was deleted" and "that
provider is broken" (consumed by `Fleet::is_missing` in fleet.rs).

`MachineRow.target: Option<SshTarget>` is `None` for the argv ssh escape
hatch (unsupported yet) — such machines list, but stay unreachable/unprobed.
It is a straight copy of `ProviderMachine.target`, which now answers the same
question at parse time (see `provider.rs`).

Worker startup sequence and its rationale:
1. `install_bundled_providers()` — writes the bundled exe-dev provider into
   place whenever what is on disk is not byte-identical to what this build
   ships. A failure only costs the default provider; discovery still runs.
2. `sweep_leaked_forward_sockets()` — this worker is the client's one
   startup point inside client-core, running before any bridge binds a
   forward, so it's the right moment to reclaim socket files a previously
   killed client could not clean up itself.

Concurrency design of the main loop (`worker_main`):
- The command arm of the `select!` delegates to `handle_command`, a pure
  router whose arms call one named function per command (the house
  match-arms-delegate rule); the four host-op spawners differ only in which
  workspace op they run and which `Event` carries the answer.
- Listing runs off the `select!` loop on its own task and reports back
  through a channel, so a slow provider never delays a probe tick or a
  command being answered. The arm that receives that report delegates to
  `publish_listing` for the same reason the command arm delegates to
  `handle_command`: it was a 21-line body doing a null check, two field
  reassignments, a build-and-sort, an event construction, a send, a branch on
  the send failing, and a probe, inside a `select!` whose other arms are all
  one line. `publish_listing` answers `Listing::Published` or
  `Listing::NobodyListening`, and only the second ends the worker — a named
  answer rather than a bare `bool`, matching `ConnectDecision` and
  `WindowFlush` elsewhere in the crate.
- `spawn_host_op`: every host op rides its own task and reports as it
  lands — a `$HOME` lookup on a cold machine costs a full SSH handshake, and
  the loop must keep probing, listing, and answering other commands
  meanwhile. Concurrent ops are safe: they are one-shot execs sharing
  nothing, and each answer carries the token that tells them apart. A
  dropped receiver on send just means the window closed.
- `spawn_list_cycle`: a provider executable that hangs is bounded only by
  its own kill-timeout, far longer than a probe interval, so a listing cycle
  must not sit inside the `select!` loop itself.
- `ProviderRows` (`HashMap<provider, Vec<MachineRow>>`) keeps each
  provider's rows from its *last successful* listing. Keeping them per
  provider is what makes a failed cycle survivable: a single blip on one
  provider's control plane would otherwise unlist its machines for a full
  interval — and since workspaces resolve their machine key through this
  list, those workspaces would become unopenable meanwhile. The key set
  itself is equally load-bearing: a provider is keyed only once it has
  listed successfully, and drops out when discovery stops finding its
  executable at all, so "keyed here" is exactly "this provider's listing can
  be believed."
- `list_provider_machines`: discovers providers, then protocol-gates
  (`check_protocol`, i.e. `info`) and `list`s every provider concurrently
  via a `JoinSet`, so the cycle costs the slowest provider rather than the
  sum of all of them. A provider that fails reports `ProviderFailed` and
  keeps its previous rows (from `ProviderRows`); `None` overall means
  discovery itself failed, so the caller changes nothing at all.
  `check_protocol`/`info` runs on every cycle, not just the first, because
  providers can be swapped out on disk between cycles.
- Fresh listing rows get their probe dots right away instead of waiting out
  the full probe interval.

`probe_machine`/`probe_reachable`: every probe rides its own task and
streams its result as it lands, so one dead machine burning its
`ConnectTimeout` no longer stalls every later machine's dot, and the worker
loop keeps answering commands meanwhile. Overlap between probe cycles is
bounded because `PROBE_TIMEOUT < PROBE_INTERVAL`, and a duplicate probe is
idempotent (the dot just updates twice).

`probe_reachable` is Harvester's own machine-liveness probe (docs/SPEC.md
§Machine): a plain `ssh <machine> true`, reachable iff it round-trips with
exit 0. It rides the multiplexed SSH master when one is up, so a warm probe
is one round trip. Errors collapse to unreachable — that is the only answer
the sidebar dot renders. This logic lives here (not in `host`) because a
probe is a *machine* question answered with an exec: `host` owns SSH
addressing and the exec boundary, and having it reach back up into
machine-level policy would invert the dependency for exactly one caller (this
file).

`probe_provider_machines` skips machines with no `target` (the argv ssh
form) — no target means no probe, and the dot stays unknown rather than
being reported unreachable.

### Runtime ownership

`start` calls `tokio::spawn`; it is invoked only while the process-owned client runtime reactor is active. The task ends when its command and result channels close. Runtime startup builds the reactor before publishing an owner, so there is no half-started machines worker.

### Daemon-free host ops

This was `workspace.rs` until it folded in here — the four ops are private to
this module now, reached only through `machines::Command`, and only
`RemoteDirEntry` is still re-exported from `lib.rs`.

Workspace browsing (docs/SPEC.md §Workspace): directory listings for the
folder picker, and git validation. This is explicitly **client** business,
never the daemon's — docs/DAEMON.md keeps path/filesystem work out of the
envelope on purpose. Implemented as one-shot execs against `Host`, so the
local machine and SSH machines get identical semantics; remote execs
multiplex over the existing SSH ControlMaster connection, so a per-click
listing costs roughly one network round trip, not a new connection.

`WORKSPACE_EXEC_TIMEOUT` (30s) is a ceiling on one picker exec: multiplexed
listings normally take tens of milliseconds, so this exists purely so a
wedged transport can't hang the picker forever.

`exec_workspace_script` is the one exec-and-require-success step all four
ops share (run under `WORKSPACE_EXEC_TIMEOUT`, fail through
`require_success` with the op's own context word), leaving each op as its
script plus its parse.

`resolve_home`: finds the machine's `$HOME`, which is where the folder picker
starts its walk (SPEC §Workspace). Deliberately one plain exec, never a
daemon round trip/bootstrap — path work being client business means the
picker must not block on a daemon it doesn't otherwise need.

`list_directory` shells out to `ls -1Ap`: one name per line, hidden entries
included (dotdirs are legitimate workspace parents — must not be filtered
out), with directories marked by a trailing `/`. Known limitation: symlinked
directories are listed *un*dereferenced by `-p`, so they render as plain file
entries in the picker — considered acceptable for a picker's purposes.

`parse_ls_entry` drops any line carrying a control character, and any empty
one. Quoting a path *into* the remote shell is sound (`shell_quote`); parsing
`ls -1` output back *out* is not, because the format has no escaping — a
filename may legally contain anything but `/` and NUL. A name with an
embedded `\r`, tab or escape sequence would otherwise become a picker row
whose text is a lie and whose value is later quoted back into a shell
command. Dropping such an entry is the honest outcome: no such row appears,
rather than a row that points somewhere else. The one case this cannot
repair is a filename containing a literal newline — `lines()` has already
split it into two clean-looking halves by the time we see it, and `ls -1`
gives us nothing to tell that apart from two files. Both halves are dropped
only if they happen to carry other control characters; otherwise they render
as two entries that lead nowhere (validation fails when they are opened).
Fixing that class for real needs a NUL-delimited listing (`find -print0`),
which is a bigger change than the picker has earned.

`validate_git_workspace`: a workspace is defined as "a path that is a git
repository" (SPEC §Workspace) — validated via `git rev-parse --git-dir`. Git's
own failure text is threaded through to the UI as the error detail.

`resolve_git_branch`: powers the chat prompt strip's read-only branch
indicator (SPEC §Flows "Start session"). Uses the same client-side exec path
as the picker's listing/validation, since the daemon has no concept of git
branches and is deliberately never asked for one. A detached HEAD reports the
literal string `HEAD` verbatim — whatever git itself says the checkout is on,
unmassaged.

## crates/harvester-client-core/src/provider.rs

Implements the machine provider contract (docs/PROVIDERS.md): user-supplied
one-shot executables in `~/.harvester/providers/` that answer "what machines
exist and how do I SSH into them." Control plane only — session traffic
never touches a provider. Each operation is one local exec: an optional JSON
object on stdin, a JSON object result on stdout, diagnostics on stderr
(logged, never parsed). This file implements the mandatory core (`info`,
`list`); `create`/`delete` are a later slice.

`parse_ssh_target` reads the `{host, user, port, options}` form and answers
`None` for the `{argv}` form, which the client does not implement — such
machines list and never connect (PROVIDERS.md §list says so normatively).
`options` is `#[serde(default)]`, so a provider written before the field
existed keeps parsing and gets an empty list rather than an error.

`SUPPORTED_PROVIDER_PROTOCOL = 1`: providers reporting any other protocol
version are rejected at `check_info` (docs/PROVIDERS.md §info). It is the one
item in this file that is still `pub` — everything else is `pub(crate)`,
because `lib.rs` declares `mod provider;` privately and re-exports nothing
else from it. This const is re-exported from `lib.rs` on purpose:
PROVIDERS.md:65 treats the exact-equality gate as a public contract point, so
the number a provider author has to match is readable from outside the crate
rather than being a claim only this file could check.

`INVOKE_TIMEOUT` (30s): the timeout for one provider invocation, after which
it's killed. `create` gets a separate 120s timeout by contract, added with
the machine-CRUD slice.

`BUNDLED_EXE_DEV`: the default provider shipped with Harvester
(docs/PROVIDERS.md §Reference implementation), implemented in the exact same
format as a user-supplied provider — no privileged code paths.
`install_bundled_providers` writes it whenever the bytes on disk differ from
the bundle (`installed_matches_bundle`), which covers both "it is missing"
and "it is an older copy". Existence used to be the only trigger, which
meant a fixed provider script never reached anyone who had already run
Harvester once — the daemon got fingerprint discipline for exactly this
failure and the provider had none. The consequence is that Harvester **owns
this filename**: an in-place edit of `~/.harvester/providers/exe-dev` is
overwritten on the next launch, so local modifications belong in a
differently-named provider file (PROVIDERS.md §Discovery says this
normatively). Disabling it is still done by clearing its executable bit
(non-executable files are ignored by discovery), never by deleting it.

The write goes through `harvester_proto::replace_atomically`, which stages
`exe-dev.next.<pid>` and renames it into place, for the same two reasons the
daemon install does: a rename is atomic, so a reader never sees a half-written
script, and two clients installing at once cannot interleave their writes into
one path. It also dodges `ETXTBSY` from a copy another client is executing
right now. `BUNDLED_PROVIDER_MODE` (0o755) is passed as the helper's `mode`
and applied to the staged file *before* the rename, so the provider is never
momentarily present-but-unexecutable — discovery would silently skip it in
that window. A staging file left behind by a failed write is removed.

This used to be a local `write_bundled_provider` plus a staged-path dance in
`install_bundled_providers`, with the suffix inline as `"next."`, and — unlike
the daemon-side copy of the same idiom in `harvester/src/workspaces.rs` — it
did **not** `sync_all()` before renaming. The two copies disagreed on exactly
the property the idiom exists for: a crash could leave a renamed-but-empty
provider script, which `installed_matches_bundle` would then read as "not a
match" forever, reinstalling on every launch and never succeeding. There is
one implementation now, in the lowest crate both callers already depend on.
`create_dir_all` stays here — the helper does not create parents.

`Provider.path` is private: every invocation must go through `invoke`, which
is the single place enforcing the timeout, exit-code rules and stderr
logging.

`MachineKey` is how Harvester keys a machine — `<provider>/<id>`
(docs/SPEC.md §Machine). `MachineKey::parse` returns `None` when there's no
`/` separator, because such strings are ad-hoc SSH destinations, not provider
machines (legacy `workspaces.json` entries, the raw argv destination).

`ProviderMachine` deliberately has no status field: the availability dot
shown in the UI is Harvester's own SSH probe, never the provider's word
(docs/SPEC.md §Machine) — a reported `status` is validated (required by
contract) and then dropped.

`ProviderMachine.target: Option<SshTarget>` answers the only question anyone
ever asks about a machine's `ssh` block — "is there a target, and what is it"
— at parse time, and stores the answer. `parse_ssh_target` returns `None` the
moment it sees an `argv` key: the argv escape hatch is parsed (a provider
using it must still `list` cleanly) but not yet wired into an actual
connection, and PROVIDERS.md §list makes "such machines still list, with no
target" normative, so an argv entry must not fail to parse and silently
vanish from the sidebar. Not decoding the argv array at all is what makes
that normative sentence true for a *malformed* argv value too, where the old
code skipped the whole entry with a diagnostic.

This replaces an `SshInfo` enum — a `Host { host, user, port }` variant, an
`Argv(#[allow(dead_code)] Vec<String>)` variant whose payload nothing read, an
`ArgvSshEntry` deserialize struct that existed only to fill it, and a
`target()` method that flattened the whole thing to `Option<SshTarget>` one
call later at `machines::machine_rows`, its only caller. Nothing outside this
file ever named the enum. Deciding at the parse and storing the decision is
the same information in one field.

Such a machine surfaces as `Error::NotConnectable` at the moment someone
tries to use it, which is where a user can actually act on it.

`discover_providers` enumerates `~/.harvester/providers`: executable regular
files register, dotfiles and non-executables are ignored
(docs/PROVIDERS.md §Discovery); a missing directory is an empty list, not an
error (covers first run). One unreadable directory entry does not fail the
whole scan — the caller (`machines.rs`) treats a discovery *error* as "change
nothing," so a single dangling symlink would otherwise freeze the machine
list forever; such entries are skipped with a stderr note instead.
`std::fs::metadata` follows symlinks, so a symlink to an executable registers
like the executable itself, and a *dangling* symlink is just an
unclassifiable entry, not a discovery failure.

`Provider::check_protocol` ("`info`") is purely a protocol gate: nothing
consumes the reported provider identity (the UI labels machines by the
provider's file name, PROVIDERS.md §Discovery), so this only checks the
contract and returns nothing. A typed result belongs to whichever future
slice first needs `capabilities`/`create_fields`.

`Provider::invoke` is the one exec-per-operation contract point: exit 0 means
success and stdout must parse as the result object; non-zero means failure,
optionally with a `{"error"}` object surfaced verbatim, else falling back to
exit status + stderr. It's killed at `INVOKE_TIMEOUT` via `kill_on_drop` (the
timeout simply drops the child future). Spawning/piping/decoding the process
itself, *and the deadline*, are `host::run_within`'s job (the crate's one exec
helper) — this function keeps only what's provider-specific: the contract's
result classification. It passes `INVOKE_TIMEOUT` in rather than wrapping the
call in its own `tokio::time::timeout`, so "every child process this crate
spawns is killed at a deadline" is one function rather than a convention two
call sites happen to follow. The cost is that a provider timeout now reads
`` provider exe-dev: `list`: timed out after 30s `` instead of a
hand-written near-identical sentence.

`InfoResult._name`, `InfoResult._capabilities` and `MachineEntry._status` use `#[serde(rename = ...)]`. **The field is not the point; the deserialization is.** The underscore records that code does not read the value while the serde rename still requires the contract's original field. All three are required *by contract*
(docs/PROVIDERS.md §info, §list), so declaring them is what makes serde
reject a provider that omitted them — deleting the fields would silently
accept a non-conforming provider, and the `info_rejects_missing_required_fields`
test would stop testing anything. Nothing in this crate consumes their
*value*: `name` and `capabilities` because the UI labels machines by the
provider's file name and no capability gate exists yet, `status` because
Harvester's own SSH probe is authoritative (docs/SPEC.md §Machine). The
underscore says "read by the contract, not by the code" without a lint suppression.

`parse_ssh_target` deliberately is not an untagged enum over `{host,...}` vs
`{argv,...}`: untagged enums report "data did not match any variant" on
failure, which throws away the field-level diagnostic (e.g. "missing field
`host`") that callers log per-entry.

`parse_machine_list` / `parse_machine` decode entries individually so one
malformed *entry* is skipped with serde's own diagnostic instead of failing
the whole `list` result; only a malformed *result* (missing `machines`) is a
hard error.

**Cross-references**: docs/PROVIDERS.md §info, §list, §Discovery,
§Transport, §Reference implementation; docs/SPEC.md §Machine.

## crates/harvester-client-core/src/token.rs

`RequestToken` is the opaque equality key for worker and Pi answers. `RequestTokens::mint` uses a checked atomic increment: tokens are process-global to the runtime engine, never inferred from cwd/path/sid and never reused by wrapping. The numeric value has no meaning outside comparison.

The engine records the latest token per chat, session or browser and removes associated context on cancellation, disconnect, replacement generation or completion. A stale answer therefore cannot mutate current state even when its echoed path or sid is identical. Token context lives next to the state machine that consumes it rather than in a generic compatibility `Pending<T>` layer.

## crates/harvester-client-core/src/identity.rs

All runtime identities are value types rather than strings passed by convention. `MachineId` rejects empty keys. `SessionId` uses `harvester_proto::is_valid_session_id`, the same validation as the daemon's session-file boundary. `SessionKey` always pairs a sid with its machine; same-sid sessions on two machines are distinct map keys. `ChatId` separates draft and live-session namespaces. `DraftId`, `WorkspaceBrowserId` and `SubmissionId` are independently minted counters.

Workspace paths are opaque remote POSIX strings after the module rejects empty or non-absolute values. Equality and hashing include machine and path. Display conversions are for labels and wire fields only; routing retains the typed key.

## crates/harvester-client-core/src/model.rs

This module owns the window-free model vocabulary. `ModelChoice` preserves provider and id as the command identity and carries Pi's display name separately. `ModelsState` distinguishes not-requested, loading, loaded and failed. `ModelSelection` distinguishes launch default/choice from live pending/unavailable/current states so draft and attached controls cannot accidentally share transition rules.

Extractors read only the fields needed by pickers. The enclosing Pi payload remains raw at the bridge/replica boundary.

## crates/harvester-client-core/src/replica.rs

`SessionReplica` keeps three related but non-interchangeable views: raw persisted entries, the raw live-event suffix, and the derived `Transcript`. Disk remains authoritative; the replica is evictable and repairable.

Persisted entries use `Arc<Vec<Value>>`. Raw live events use a persistent reverse-linked log, making a cloned snapshot and one append O(1) while preserving every current-suffix `Value`. `live_events()` allocates only the reference index needed to iterate oldest-first. The log has no arbitrary retention cap: on each settled turn the bridge fetches an authoritative history delta and clears the covered live suffix, and repair does the same after a gap.

Full history replacement and delta extension clear the old live suffix because fetched persisted entries authoritatively cover that span. The bridge's replay window then delivers events after the selected cursor. Unknown custom entries and events remain available even when `Transcript::apply` returns false.

Every mutation advances a checked `revision`. The desktop uses this value to observe same-length stream replacements and tool changes without deep comparisons. Snapshot cloning is cheap: raw log nodes, history and transcript items are shared until the one replica being changed needs copy-on-write derived data. Runtime handlers transfer owned history vectors and live values into the replica, avoiding a second deep clone at the bridge-to-engine seam.

## crates/harvester-client-core/src/runtime.rs

`ClientRuntimeOwner` constructs a current-thread Tokio runtime first, then starts one dedicated process thread and seeds one `Engine`. If runtime construction fails, no owner or thread escapes. `ClientRuntime` is the cloneable frontend handle: latest snapshot, event subscription and typed intents only. Draft, browser and prompt ids use checked atomic increments; prompt submission is explicitly reserved before `submit_prompt`, allowing a view to register its input before a stopped runtime can synchronously reject it.

`RuntimeEventHub` gives each subscriber a private FIFO. `Changed` is the sole coalescible event because it only means “read the latest snapshot”. Semantic events are never dropped or translated into `Changed`: prompt accepted/rejected/completed, dialog-answer failure, draft promotion, workspace completion and machine-forget outcomes must drive view-local ownership transitions exactly once. Queues are intentionally unbounded for semantic correctness. Dropping or explicitly closing the hub wakes subscribers and makes `next()` return `None` after queued events drain. If the engine loop ends unexpectedly, the worker publishes a fatal runtime message before closing the hub, so the desktop cannot remain silently stale.

Prompt acceptance means Pi acknowledged the request, not merely that a bridge channel accepted a command. Rejection carries the original `SubmissionId`, `ChatId`, text and detail. Desktop adapters must reserve, record, then submit in that order. If scheduling delivers `agent_settled` before the prompt response task, the pending prompt records the boundary; a later success emits acceptance immediately followed by completion, while a later failure still rejects.

## crates/harvester-client-core/src/runtime/snapshot.rs

`ClientSnapshot` is the immutable observation model. Large collections are `Arc`-backed. The live-session map stores `Arc<LiveSession>` values as well as living behind an `Arc<HashMap<...>>`, so one streamed event clones map pointers and only the targeted live-session value. Old GPUI frames may safely retain older snapshots.

`LiveSession` owns lifecycle, attachment generation, replay cursor/repair state, model and branch controls, `SessionReplica`, queued dialog requests and the generation-local turn revision. `can_stop` is true only for an attached busy transcript. `DraftSession` owns target and launch controls before promotion. Browser state, machine presentation state and session summaries are read-only values to frontends.

Snapshot `revision` is checked-monotonic. `SessionReplica::revision` is the transcript-observation key; collection lengths are not a valid substitute because cumulative streaming and tool updates can change content without changing shape.

## crates/harvester-client-core/src/runtime/engine.rs

`Engine` is the only client orchestration owner. It routes intents and already-generation-filtered fleet events into workspace, index, draft, live-session, model, branch, browser, dialog and prompt state. The file follows named handlers; the top-level matches only dispatch.

Session attachment generations are monotonic per `SessionKey`. Opening an exited, failed or disconnected session retains its replica and controls until authoritative `Attached` history arrives. `Attached.history_available = false` changes lifecycle and room ownership without replacing that replica. History replacement/extension and live Pi events mutate only a matching generation. Turn boundaries advance `event_revision`; stale `get_state` may still provide model identity but cannot overwrite newer start or settlement evidence. Message and tool events mark the transcript busy without making every streaming delta invalidate the state request, covering attach in the middle of a turn when `agent_start` was missed while still allowing an initial `isStreaming` answer to settle.

`pending_prompts` records submission id, chat, session generation, prior busy state, submission event revision and whether Pi acknowledged delivery. The optimistic transcript row carries the same `SubmissionId`, so duplicate prompt text reconciles and fails independently. Queue acceptance creates the optimistic user item but emits no acceptance. `PromptDelivered` marks and emits acceptance once. `agent_settled` completes only delivered submissions, so a settlement racing ahead of the response cannot erase a later rejection. Failure removes the optimistic obligation, appends its diagnosis and preserves another/newer active turn. Exit, disconnect and forget complete acknowledged submissions but reject unacknowledged ones. Bridge-side arbitration guarantees one response/disconnect outcome.

Dialog obligations enter and leave exact session rooms. An answer remains in flight until the bridge returns Pi's success verdict; failure keeps the obligation at the FIFO front for retry. Model, state, set-model, branch and browser answers are token-correlated; displaced set-model responses are ignored. A latest set-model failure restores its prior presentation and requests fresh controls so an earlier successful request cannot leave the UI lying about Pi's actual model.

Every handled external event republishes the latest snapshot. Semantic events are emitted separately through the lossless hub. Pure helper tests and fake bridge tests cover keyed same-sid routing, generation rejection, multiple sessions, history preservation/repair, raw custom data, dialogs, mid-turn state, prompt races, stale model/branch/browser answers, persistence and forget cascades.

## crates/harvester-client-core/src/runtime/engine/browser.rs

Browser handlers own the daemon-free remote folder workflow. A browser id and latest request token correlate home resolution, directory listings and workspace validation. Navigation uses remote POSIX joins, including a single slash at root. Cancel or completion removes the run and invalidates all pending tokens.

Navigation accepts only directory names present in the current listing, preventing stale or forged path traversal. Validation stages the new workspace vector through `WorkspaceStore` before publishing it. Duplicate confirmation closes the browser without rewriting. `WorkspaceAdded` includes the originating browser id and is semantic because only that modal coordinator may release its matching picker if snapshot repaint events coalesce.

## crates/harvester-client-core/src/runtime/engine/tests.rs

These focused tests live beside the non-trivial engine rather than in `tests/`. They exercise real handlers and intents with an inert fleet, private event subscriber and scratch persistence paths. Coverage includes same-sid keys on different machines, monotonic reopen generations, stale attachment events, room/dialog replay, mid-turn `get_state`, model/branch/browser token retirement, arbitrary intent rejection, optimistic prompt failure and delivery/settlement races, semantic-event burst preservation, reconnect-history preservation, workspace persistence and machine-forget policy.

Bridge fake-daemon tests cover the transport half; engine tests do not duplicate JSONL framing.

## crates/harvester-client-core/src/session_index.rs

`SessionIndex` merges per-machine daemon listings with sessions synthesized immediately after a successful start command. Keys are full `SessionKey`s. Replacing one machine's listing cannot erase another machine's rows. Visibility is restricted to stored workspaces, sorted newest-first, and status updates affect only the exact key.

A later daemon listing supersedes synthesized metadata because disk/daemon listing is authoritative. Removing a machine clears only its rows.

## crates/harvester-client-core/src/workspace.rs

`WorkspaceStore` is the sole implementation of `~/.harvester/workspaces.json` persistence. It resolves `$HOME`, treats a missing file as empty, reports malformed/unreadable files to stderr and returns an empty observable list rather than inventing state. Saves serialize the typed `(machine, path)` records and use `harvester_proto::replace_atomically` after creating the parent directory.

Runtime mutations stage a new vector, save it, then publish it. A failed add or forget therefore leaves both memory and disk unchanged. Tests use `WorkspaceStore::at` with scratch paths.

## crates/harvester-client-core/src/transcript.rs

The client's model of one session's conversation: pi's history entries and its
live event stream, reduced to a list of items a frontend renders. This is
where the client "speaks Pi" (docs/SPEC.md §Rendering pi) — the shapes of
`message_end`, `tool_execution_*` and `get_entries` history, all pinned to pi
0.83.0. The module used to live inside the gpui chat pane, which made every
one of these facts reachable only through a window and testable only by hand;
the two behaviors that most needed a test — out-of-order tool correlation and
the output cap's UTF-8 boundary — are covered at the bottom of the file.

`Transcript` deliberately holds no view state. Whether a tool block is
expanded is the frontend's business, and a frontend keying that by
`tool_call_id` (rather than by position) keeps the state across a resync,
which replaces every item.

`TOOL_OUTPUT_MAX_BYTES` (64 KiB) caps kept tool output per block, tail-kept.
`tool_execution_update` snapshots are replaced wholesale, so the cap only
bounds client memory — the full output always lives in the session file on the
machine.

`Role` covers who said a rendered message; tool results are `ToolBlock`s, not
messages, and system entries are skipped entirely.

`ToolStatus`: a history `toolCall` without a matching `toolResult` stays
`Running` — either the tool is genuinely in flight (client attached mid-turn)
or the session died with it still outstanding.

`ToolBlock` is correlated by `toolCallId`. Pi runs tool executions in parallel
and their `tool_execution_end` events arrive out of order (verified live
against pi 0.83.0), so position in the list never identifies a block — only
the id does. Every block also keeps raw call arguments, the latest raw partial result and the raw final result; the built-in output, diff and line statistics are only a projection. `ToolBlock.diff` holds edit's pre-rendered numbered diff (`result.details.diff`).

`ToolBlock::running` is the only way a block is born, from either source: a
live `tool_execution_start` and a history `toolCall` block differ solely in
which JSON keys carry the id, name and arguments — everything else about a
just-started tool (empty output, not truncated, no diff, `Running`) is the
same fact stated twice, and stating it twice is how the two paths drift.
`UNNAMED_TOOL_NAME` (`"tool"`) is the fallback when pi names no tool: a block
with a blank verb reads as a rendering bug, so it renders as a generic one.

`ToolBlock::line_stats` produces the "+N −N" label: `write` counts the lines
in the `content` argument it was called with (known from the call, before any
result); `edit` counts its diff's +/− lines (only known once the result
lands). Pi's diff format is `" 1 ctx" / "-2 old" / "+2 new"`, so the leading
byte is the marker. Only these two tools report line changes in pi 0.83.0 —
counting a `+` in some future tool's output would fabricate a stat, so
everything else returns `None`.

`Item` is the conversation in render order: messages interleaved with boxed tool blocks. The indirection keeps ordinary message items small even though a tool block preserves several raw Pi values. Pi emits the assistant `message_end` before its tool executions start,
so plain push order is the true chronological order. `Item::Failure` is its
own variant (not rendered as an assistant message) because a failed turn is
not something pi *said* — rendering it as an assistant message would put words
in the agent's mouth.

`Transcript` is the **repairable replica** described in docs/SPEC.md §Design
invariants: the session file on the machine stays the only authority, and any
doubt is resolved by refetching rather than by reconciling. `history_len`
tracks how many leading items came from a `get_entries` snapshot; everything
past it was built from live events, which carry no entry ids (pi 0.83.0,
verified) and so cannot be matched against fetched entries one by one. That is
exactly why the boundary is worth recording: a delta is the authoritative
account of the span the live tail was guessing at, so the tail is replaced by
it rather than appended to.

- `Transcript::new` rebuilds from `get_entries` history: user/assistant text
  plus tool blocks rebuilt from assistant `toolCall` blocks matched to their
  `toolResult` messages — the same data the live events carry (verified
  against pi 0.83.0 session captures).
- `Transcript::streaming` exposes the in-flight assistant message, and
  `apply_message_update` speaks both wire shapes pi has shipped: pi 0.83
  re-sends the whole partial AssistantMessage on every update (replacement),
  pi 0.84 dropped `partial` and sends only bare `text_delta`s
  (accumulation — verified live 2026-08-27, raw captures of both versions in
  `tmp/pi-083-stream.jsonl` / `tmp/pi-084-stream.jsonl`). A `partial` field
  wins wherever it exists, because it is cumulative and therefore self-healing
  under event loss; bare deltas are appended. Any future shape that matches
  neither degrades to the `message_end` fallback — the full text appears when
  the message finishes, late but never lost and never hung. Thinking deltas
  and tool-call deltas never touch the streamed text.
- `Transcript::is_busy`: a turn is running until `agent_settled`, not
  `agent_end` — retries, compaction and queued follow-ups may still run after
  `agent_end`.
- `Transcript::replace_history` answers a resync (docs/DAEMON.md §Events): the
  fetched entries are authoritative and replace everything rendered,
  including the in-flight streamed text (on 0.83 the next `message_update`
  re-carries the whole partial; on 0.84 the preview restarts from the next
  delta and `message_end` still lands the full message). `busy` survives a
  replace: turn state belongs to the
  event stream, and a snapshot fetched mid-turn does not end the turn.
- `Transcript::extend_history` folds a delta from `get_entries {since}`
  (docs/SPEC.md C6) onto what is already held — the cheap repair, versus
  `replace_history`'s expensive full rebuild. The live tail is truncated
  first (see the `history_len` rationale above for why it cannot be
  reconciled entry by entry); what survives is the prefix pi already
  confirmed. A session with a thousand entries repairs by fetching only the
  handful it missed. `busy` survives here too, for the same reason as above.
- `Transcript::push_user_prompt` renders the prompt before pi echoes anything back, tags the row with its `SubmissionId`, and starts the turn immediately. The private tag lets identical prompt text reconcile independently and is cleared when authoritative history or Pi's user-message echo confirms the row.
- `Transcript::end_turn` fires on `agent_settled`, or on a `session_exit` that
  ended a turn the hard way; the streaming partial is dropped with it since
  whatever was streaming will never be completed now.
- `Transcript::fail_prompt` removes the matching optimistic obligation and
  appends the delivery diagnosis. It ends a turn only when that submission
  started from idle and no newer turn event has arrived. A rejected steering
  prompt must not clear the partial text or busy state of the turn it tried to
  steer. The rejected optimistic row is removed; `RuntimeEvent::PromptRejected` restores the exact input text in the originating view and the transcript keeps the delivery diagnosis.
- `Transcript::apply` advances by one recognized Pi event and returns `false` for a shape the current derived renderer does not cover. `SessionReplica` retains that raw value for the current live suffix and advances its revision so observers can refresh; after settlement, Pi's raw persisted entries become the authoritative extension surface.
- `apply_message_end`: text is pushed first, then a possible `Item::Failure`
  — a turn can stream a partial answer and *then* fail, and both halves are
  things the user needs to see.
- `apply_tool_start` runs after `apply_message_end` in event order because pi
  always emits the assistant `message_end` before that turn's tool executions
  start, so the tool block naturally lands after the text that announced it.
- `apply_tool_update`: `partialResult` is the accumulated snapshot, so it
  replaces the block's output rather than appending.
- `finish_tool_block` is the one place a running block becomes a settled one,
  shared by the live end event and the history fill. The two paths differ only
  in *where the error flag lives*: a live `tool_execution_end` carries
  `isError` on the event beside a separate `result`, while a history
  `toolResult` message carries `isError` on the result object itself
  (`fill_tool_block_from_result` is that one-line adaptation, and
  `is_error_flag` reads the key from whichever object owns it). Hence the
  `result: Option<&Value>` parameter — a live end event may legally carry no
  result at all, and it must still settle the status, or the card spins
  forever. Kept in one place because a rule about how a tool block settles
  that is written twice is a rule that only holds until one copy is edited.
- `extend_items` is one pass in entry order, with one output: user and
  assistant messages become items (a `toolCall` block becomes a `Running`
  tool block), and a `toolResult` entry finishes the block its
  `toolCallId` names, via `apply_tool_result`. Nothing is collected first
  and nothing is rendered finished at birth.

  This rests on one fact about pi's own storage: a `toolResult` entry can
  never precede its `toolCall` entry in `get_entries` output. `get_entries`
  returns entries "in append order" and the session file is a parent-linked
  chain, with the `toolResult` entry's `parentId` pointing at the assistant
  entry that carries the call (pi 0.84.3 `docs/rpc.md` §get_entries,
  `docs/session-format.md` §SessionMessageEntry). Re-verified over the three
  captured fixtures under `crates/harvester/fixtures/`: 26 `toolResult`
  entries, zero preceding their call.

  It used to be three passes with two borrowed intermediate collections and
  two `'a` lifetime parameters — a `tool_call_id → toolResult` map filled by a
  `record_tool_result`, an ordered user/assistant list, then a build pass
  threading the map through `push_message_items` into `tool_block_from_history`
  so a block could be rendered *already finished*. Every one of those pieces
  existed to have a call's result in hand at the moment the call was rendered.
  Rendering it `Running` and letting the result finish it is the mechanism
  that already had to exist for the cross-delta case, so the whole
  arrangement was a second implementation of `find_tool_block`.

  The cross-delta case is unchanged and is the reason `apply_tool_result`
  searches `items` rather than only this slice: across a delta boundary the
  call sits in the prefix and its result arrives in the delta, and pushing a
  *new* block for it would render the same call twice (once finished, once
  running forever). Pinned by
  `a_delta_finishes_a_tool_call_left_running_before_the_cursor`; the
  same-slice case is pinned by
  `history_matches_tool_calls_to_their_results_across_entries`.
- `PI_AGENT_SETTLED` is the one named token in `Transcript::apply`'s event match because bridge replay and runtime orchestration also read it. It lives at client-core's crate root, the lowest common module for those readers. The other event tokens are local to this match and remain literals; `harvester-proto` names only tokens read by more than one crate.
- Pi persists its failure messages, so `extend_items` reads them back on
  reopen the same way the live stream does — otherwise history would be
  quietly shorter than the conversation the user actually had.
- `find_tool_block` searches from the back, since a delta's results belong to
  the most recent calls; this also means a repeated `toolCallId` (pi mints
  unique ones, so this is a contract safety net rather than an observed case)
  can only ever affect the most recent block, never rewrite history.
- `capped_content_text` tail-caps joined result text at
  `TOOL_OUTPUT_MAX_BYTES` and moves the cut forward to a `char` boundary: the
  cap is a byte count and pi's output is arbitrary UTF-8, so a naive byte
  slice can land mid-character and panic.
- `message_failure`: pi reports a failed or aborted turn as an assistant
  message whose text is the **empty string**, with the reason beside it in
  `errorMessage` / `stopReason` (verified: pi 0.83.0
  `pi-agent-core/dist/agent.js`, the `failureMessage` it builds and then
  persists). A turn that failed is therefore shaped exactly like a turn that
  said nothing, and reading the reason out is the only thing that
  distinguishes them — skip this and a hard failure (expired token, refused
  model) renders as an empty pane the user retypes into. When `errorMessage`
  itself is empty, the fallback `format!("turn {stop_reason}")` still
  separates "pi had nothing to say" from "pi never got to answer".
- `message_text` renders text blocks verbatim; thinking and tool-call blocks
  are skipped because tool calls render as their own blocks, not as inline
  text. It joins the surviving blocks with a blank line and accepts a bare
  string `content` as well as an array, because pi sends both. It is the one
  helper in this module that is `pub` rather than private: the gallery's
  fixture replayer needs the same text out of the same payload, kept its own
  copy, and that copy had already drifted — it joined with `""` and ignored
  the bare-string shape, so a multi-block message previewed without its
  paragraph breaks. One `pub fn` and no second copy is the fix.

The test module docstring: pi 0.83.0's wire shapes are pinned here — every
fixture is the shape the client actually receives, and the point of this
module living outside the gpui crate is that these tests run without a
window.
