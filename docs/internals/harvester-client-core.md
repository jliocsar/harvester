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

## crates/harvester-client-core/src/bridge/attach_window.rs

**Purpose.** Buffers the live events a session emits between its `attach`
landing daemon-side and its history reaching the frontend. The daemon starts
streaming a session's events the instant `attach` lands (docs/DAEMON.md
§attach — deliberate daemon behavior, not something this client can or should
reorder), but the frontend has nowhere to put those events until
`Event::Attached` publishes the `get_entries` snapshot one round trip later.
An event arriving in that window is missing from the snapshot *and* would be
dropped by the frontend if delivered live — concretely, a lost
`tool_execution_end` leaves a tool card spinning for the life of that chat,
because streaming text self-repairs (every `message_update` carries the whole
partial) but tool-end events do not. Hence the bridge holds that window back
and flushes it right after the snapshot.

Everything in this file is pure and synchronous by design — no daemon, no pi,
no async runtime — specifically so it is the one piece of the whole fix that
can be unit-tested in isolation (see the test module).

**`ATTACH_EVENT_BUFFER_CAP = 1024`.** How many live events one session may
hold back while its history fetch is in flight. Chosen as generous headroom,
not a tuned limit: a `get_entries` round trip is on the order of
milliseconds, and pi emits only a few events per second, so 1024 is orders of
magnitude more than should ever be needed. The cap exists purely so a wedged
fetch can't grow an unbounded `Vec`.

**`PushOutcome`** — what the select loop should do with a live event it just
offered to the buffers: `Deliver` (no flow is holding this session back —
send it live), `Buffered`, `Overflowed` (the cap was just hit on this event —
the held events are dropped and the session is marked for resync; this is the
one outcome worth a stderr line), `Discarded` (already overflowed —
everything after the trip point is dropped on the floor because the
resulting resync is authoritative anyway, so re-holding events is pointless).

**`FlushOutcome`** — what an attach flow finds waiting once `Attached` is
about to be published: `Events` (deliver these, in arrival order, before
anything else) or `Overflowed` (events were lost, so the snapshot just about
to be published is already stale).

**`AttachEventBuffers::register`.** Idempotent by design: a resync racing an
already-in-flight open joins the window that flow already registered rather
than resetting it — resetting would discard everything held so far for that
race loser's benefit, silently reintroducing the exact gap the mechanism
exists to close.

**`AttachEventBuffers::flush` returning `Events(vec![])` for an absent
session** is not an error path: it means another flow for the same sid
already flushed first, its events are already out, and its own `Attached`/
repair event already seeded the chat — so there's nothing left owed to this
caller.

**`AttachRegistration`.** RAII guard owning one session's registration for
the length of an attach flow. RAII specifically because the ways a flow can
exit early outnumber the way it completes normally — a failed attach and a
failed `get_entries` both return early — and a registration that outlived its
owning flow would silently swallow that session's live events for the rest of
the bridge's life, which is strictly worse than the buffering gap this whole
mechanism exists to close. Same defensive shape as `Pending` in `client.rs`
and `ForwardGuard` in `connection.rs` elsewhere in this crate — a recurring
pattern worth recognizing.
- Must be constructed *before* the `attach` request is sent, not after: the
  daemon's event stream starts the moment `attach` lands, not when its
  response comes back to the client.
- `flushed: bool` field exists because `flush()` already removed the buffer
  entry from the map; without tracking that, a `Drop` running after a
  same-sid *re-registration* (e.g. a resync racing right behind a flush)
  would wipe a window it no longer owns, belonging to someone else's
  in-progress registration.

**Test coverage worth knowing about (regression intent, not just assertions):**
- Order of flushed events is asserted explicitly because a `tool_execution_end`
  only mutates an already-existing block rather than being self-contained —
  delivering it out of order relative to `tool_execution_start` breaks
  rendering, so arrival order is a real invariant, not incidental.
- A dropped (never flushed) registration must reopen the live stream — this
  is the concrete failure mode a failed attach or failed `get_entries` must
  not trigger: an early return must not leave the session's events
  permanently buffered.
- Overflow test: the event that trips the cap is the only one that logs
  `Overflowed`; everything after is `Discarded` silently, and the eventual
  flush reports `Overflowed` — mirroring the daemon's own `event_gap`
  semantics rather than inventing a parallel concept.
- A flushed registration's `Drop` must not wipe a subsequent same-sid
  registration's window (the `flushed` flag's whole reason to exist).
- A second registration joining a window already in flight (open racing a
  resync) means the "loser" of the race owes nothing on its own flush — the
  winner already delivered everything.

### Recorded follow-up

None — no unresolved TODOs or deferred decisions were embedded as comments in
these three files; every comment either explained current, intentional
behavior or documented a past bug being guarded against by a test.

## crates/harvester-client-core/src/bridge/flows.rs

One straight-line async flow per command, plus two the bridge starts on its
own (the initial `sessions_flow` list_sessions, and `resync_flow` after
events are lost). Every flow publishes exactly one outcome — an `Event` the
frontend renders, or a `RequestFailed` via `report_failure` — and none of them
return a value; spawning a flow is fire-and-forget.

`UNKNOWN_PI_ERROR` is the stand-in detail for a pi response that says
`success: false` without saying why — a contract violation on pi's side, but
one that must still produce a sentence rather than an empty error string.

**`attach_session`.** The spine both entry points share: take the `attached`
guard, leave the previous session, create-or-resume, register the attach
window, attach. It hands back the `AttachRegistration` on success and `None`
on any failure — each failure already published its own `RequestFailed`, so
the caller has nothing to add. Extracted because `open_flow` and
`start_session_flow` had drifted into two copies of it that differed only in
which `create_session` fields they filled in; a fix to the ordering rules
below had to be made twice, which is exactly how the ordering rules get
broken. The guard is dropped when this helper returns, so both flows now
publish their snapshot event outside the attachment lock — previously only
`open_flow` did.

**`open_flow`.** `Open`'s implementation: create-or-resume, then attach.
Resume is the *same call* as create (per DAEMON.md), and it's idempotent for
an already-live session, so this one chain correctly covers a running
session, an exited one, and one that died while the client was detached —
with no client-side "is it running" state that could go stale. Follow-up
requests queue on pi's stdin in order, so no readiness probe is needed after
attach.

Sequencing details that are load-bearing, not incidental (the first two now
live in `attach_session`, which is what makes them true of both flows):
- The `AttachRegistration` is created *before* the attach request goes out,
  not after — the daemon streams events from the moment `attach` lands, and
  `Event::Attached` is a whole `get_entries` round trip later. Registering
  early is what closes the window; registering late would miss the same
  events it exists to catch.
- The async `attached` mutex guard is dropped as soon as the attachment
  transition itself is done, before `get_entries` runs: an async mutex should
  cover a state transition, never a remote call. Holding it across
  `get_entries` used to let one wedged pi block every later `Open` on this
  bridge for the life of the app.
- History replay order is attach → `get_entries` → live events
  (DAEMON.md §attach) — deliberately not reorderable.

**`resync_flow`.** Handles both the daemon's `event_gap` and the bridge's own
attach-window overflow with the same `get_entries` cure, since they're the
same underlying situation (events lost, need a fresh source of truth). It
registers its *own* attach window — a resync that let events arrive
unbuffered during its own fetch would reintroduce exactly the hole it exists
to close. It deliberately declines to do two things it used to do: repair a
session nobody is watching, and run two concurrent repairs for the same
session (guarded by `ResyncGuard`/`resyncing`). The
retry-on-overflow is a `loop`, not a respawn, because the retry belongs to the
claim this flow already holds; it can in principle overflow again, but that
would require the session to outrun the cap on every single attempt — the
daemon's own ring buffer in front of the bridge is the narrower bound in
practice.

The `attached` check sits at the *top of the loop*, not at the flow's
entry: an overflow retry can start arbitrarily long after the entry check,
and by then the pane may have moved to another session — the retry would
then refetch a session nobody watches and publish history for a detached
sid. Checking per iteration costs one uncontended lock and makes "only the
attached session is worth repairing" true for every iteration rather than
only the first.

A *failed* repair still flushes the attach window before returning. The
window was registered for the duration of the fetch, so the live events that
arrived during it are sitting in the buffer with nobody but
`AttachRegistration::drop` to reach them — and dropping them was pure loss,
because no snapshot replaced them. Those events are still valid and still
the only account of what happened: a discarded `tool_execution_end` leaves a
tool card spinning on screen forever, which is the exact loss class the
attach window exists to prevent (docs/SPEC.md, "any doubt → refetch"). The
`RequestFailed` from `repair_history` remains the user-visible signal that
the repair itself did not land.

**`repair_history`.** Implements docs/SPEC.md C6: a delta refetch when this
session left a cursor, a full refetch otherwise — so a long-lived session
repairs by fetching only the handful of entries it missed rather than its
whole history. A failed delta silently falls back to a full refetch instead
of being reported as an error: the cursor has one specific documented failure
mode — pi answering `Entry not found: <since>` for an id that compaction
retired — and starting over is the documented cure. Matching pi's error text
to special-case that would be the narrower, more brittle check; falling back
unconditionally is less code and produces the same answer, because a fetch
that fails for any other reason fails again on the fallback and is reported
there.

**`fetch_entries`.** One `get_entries` call, with or without a `since`
cursor, returning pi's entries plus the `leafId` that becomes the next
cursor. Per HARNESS_RESEARCH.md, "an entry id works as a durable cursor …
even across client restarts."

**`cursor_for` / `record_cursor`.** The cursor is one global slot — the
attached session's — filtered by sid on read rather than being cleared on
attach: a cursor tagged with the session it came from cannot be spent on the
session that replaced it, regardless of what order the attach and the fetch
actually land in. A fetch that returns no `leafId` leaves no recorded cursor
— the next repair for that session just refetches whole, which is always
correct, never worse than guessing wrong.

The write side is compare-and-set against the *attachment*, not against the
slot: `record_cursor` re-reads `attached` and stores nothing unless the sid
it was called for is still the attached one. Comparing against whatever the
slot happens to hold would be the wrong test in both directions — it would
refuse the legitimate handover (the newly attached session's first cursor
would find the old session's still in the slot) and it would still let a
stale flow claim an empty slot. The attachment is what makes a flow stale in
the first place, so it is what the write is gated on. Without the gate, a
resync that finishes after the pane has moved on overwrites the new
session's cursor with its own leaf id, and the new session's next repair
spends a foreign cursor — self-healing (pi answers `Entry not found` and the
fallback refetches whole) but a wasted round trip born of a slot nobody
owned. `is_attached` is the shared one-line predicate for both this and the
resync loop's per-iteration check.

**`ResyncGuard`.** RAII guard enforcing "one repair per session at a time",
for the same reason `AttachRegistration` is RAII: a flow that returns early —
or gets cancelled — while still holding a claimed sid would block every later
repair for the life of the bridge, which is strictly worse than the duplicate
work the guard prevents.

**`forget_attachment`.** Runs when a pi exits: a pi that exited took its
attachment with it. Without this, the bridge kept believing it was still
attached, and the next `Open` sent a `Detach` for a session whose pi is
already gone — which fails and surfaces to the user as a statusbar error for
an action they never took.

**`replay_history`.** The opening-fetch counterpart to `repair_history`: seeds
a chat with a session's whole history because there's no cursor to work from
yet (nothing has been fetched for this session before). Returns `true` when
events were lost anyway during the fetch and a follow-up repair is owed.

**`deliver_buffered_events`.** Releases the attach window the instant the
frontend has a chat to put events into. There is no `await` between sending
the snapshot event and this call, so no live event can slip in ahead of the
events the window held back. Returning `true` means the window overflowed, so
the snapshot just published is already stale — deliberately reusing the
daemon's own `event_gap` vocabulary/semantics rather than inventing a second
recovery concept.

**`start_session_flow`.** `StartSession`'s half of `attach_session` plus the
first prompt. `Attached` publishes with empty entries since the session
file is brand new — there is no history to fetch. It still comes back from
`attach_session` holding an `AttachRegistration` even though the window will
be empty in practice, purely so that "events between attach and `Attached`
are held" remains a property of every attach path, not an accident specific
to `open_flow`. The chat is moved
into live state immediately after attach (no waiting on the prompt): the
prompt's queued-ack rides its own flow and only rejection there is notable.
Correspondingly there's no cursor recorded here — the session file is brand
new, so the first repair (if any) will fetch it whole anyway.

**`detach_previous`.** One chat pane implies one attachment: leaving the
previous session is what stops the daemon from streaming events nobody
renders. A refused detach aborts the whole transition and preserves the
previous attachment rather than proceeding — proceeding anyway would leave
two rooms attached daemon-side while the bridge's own bookkeeping remembered
only the replacement.

**`prompt_flow`.** Sends `{"type": "prompt", ..., "streamingBehavior":
"steer"}`. Acceptance means queued, not completed — progress arrives
separately as events, so only rejection here is notable. Rejection publishes
`Event::PromptFailed` (sid-scoped), *not* `RequestFailed` (machine-scoped):
the frontend rendered this prompt and raised a busy indicator for it before
the flow ran, and only the chat holding that sid can retire them. It also
goes to stderr, because that chat may have been closed in the meantime and
a lost prompt is worth a line either way. `streamingBehavior`
is mandatory in practice, not just documented: pi rejects a prompt outright
while it's mid-stream unless this field is given (verified against
`agent-session.ts`, pi 0.83.0) — without it, every prompt typed mid-turn
would silently be dropped. `"steer"` specifically matches what pi's own
interactive mode does with a mid-turn message; pi ignores the field entirely
when the session is idle, so it's always safe to send.

**`stop_flow`.** Hands the session to the daemon's stop ladder and stops
caring. Nothing is published here on success: the reply only confirms the
ladder started, and the outcome the frontend actually renders
(`Event::SessionExit`) arrives on the session stream, possibly *before* this
response returns (DAEMON.md §stop) — publishing anything from this flow would
be a second, earlier, and less true account of the same fact. There is
deliberately no explicit detach here either, because the resulting session
exit already runs `forget_attachment`.

**`answer_dialog_flow`.** The client's half of the dialog contract: with a
client attached, the daemon forwards rather than auto-cancels, so pi stays
blocked until this lands (docs/DAEMON.md §Dialogs). Deliberately does *not*
use `pi_flow_result`, because that helper interprets `success`, and the
daemon answers every `extension_ui_response` with a bare `null` regardless of
whether it actually forwarded the answer or dropped it as too late — reading
a verdict out of that `null` would falsely report a failure for every dialog
answered normally. Only a transport-level error means anything here, so only
that is reported.

**`pi_request_flow`.** The whole implementation of `Command::PiRequest`: run
the verbatim `cmd` through `pi_flow_result` and publish `Event::PiAnswered`
with the same sid, the same token, and `pi_method(&cmd)` — the command's own
`type` string, read once here so no other layer has to re-type the command it
just sent. It is the only place in the crate that inspects a pi payload's
contents at all, and it inspects exactly one field it then copies verbatim.

**`pi_flow_result`.** Shared helper turning a pi command into the `Result`
that picker/statusbar-facing events carry: `data` verbatim on success, pi's
error string otherwise.

## crates/harvester-client-core/src/bridge/mod.rs

**Module shape and threading model.** The bridge is one background thread per
machine, running a current-thread tokio runtime that owns that machine's
daemon connection — this crate is tokio-based, but gpui's own executor has no
tokio reactor, so the daemon connection has to live on a dedicated thread.
Frontends talk to a bridge over tokio mpsc channels (reactor-independent, so
awaitable straight from gpui tasks): commands go down each bridge's own
sender, and events from every bridge come back tagged with the machine key on
one shared channel. The module is deliberately gpui-free.

`mod.rs` is the whole of the bridge's *shape*: the `Command`/`Event` contract
a frontend codes against, the thread and its `tokio::select!` loop, and the
router (`dispatch`) that turns one command into one spawned flow. Two things
are deliberately factored out:
- `flows` — one straight-line async flow per command; responses resolve at
  their await points, so there is no response-routing state machine anywhere
  in the crate. This is *why* the select loop in `mod.rs` can stay a pure
  router — see `dispatch`'s note below.
- `attach_window` — the live events held back between `attach` landing and
  `Event::Attached` publishing; pure, synchronous, and tested standalone.

**`Command` (UI → bridge).** A command answered by exactly one event carries
a `RequestToken` that the answering event echoes (docs/SPEC.md §Decisions,
C2). `Open`, `StartSession`, and `Prompt` carry no token: they have no single
reply — what they produce is the session's own event stream, keyed by sid,
which outlives the request that started it.
- `Open`: `create_session` (resume-or-create, idempotent for an
  already-running sid) then attach. Deliberately carries no "is it running"
  flag — the daemon owns that truth, and any client-cached copy goes stale
  the instant a session exits while the client is detached. Ends in
  `Event::Attached` or a failure event.
- `StartSession`: turns an unstarted chat into a real session (SPEC §Flows
  "Start session"): `create_session` → `attach` → first prompt, all riding
  pi's in-order stdin queue, no readiness probe needed. Ends in
  `Event::Attached` with empty entries (session is brand new) or a failure
  event.
- `ListModels`: daemon `list_models`, uncached — costs a real pi startup.
  Answered by `Event::ModelsListed`. Deliberately *not* folded into
  `PiRequest`: it is a harvester command to the daemon's throwaway
  `--no-session` pi, not a passthrough scoped to a running session, so it has
  no sid to carry. Use `PiRequest` instead for a session that is already
  running — `get_available_models` works against a live pi and costs
  milliseconds instead of a process start.
- `PiRequest { sid, cmd, token }`: the one session-scoped round trip. `cmd` is
  a pi command verbatim (`get_available_models`, `get_state`, `set_model`,
  `get_session_stats`, `set_session_name`, `set_thinking_level`, `compact`, …)
  and the bridge neither reads nor rewrites it beyond echoing its `type` back
  as `method`. Answered by `Event::PiAnswered`. This replaced three typed
  commands (`ListSessionModels`/`GetSessionState`/`SetModel`) that were each a
  variant, an identically shaped event and a one-line flow over
  `pi_flow_result` — a typed shadow of an untyped thing, in a crate whose
  whole rule is that the client speaks Pi. Every further pi command is now
  zero bridge code.
- `Stop`: runs the daemon's stop ladder (pi `abort`, then stdin EOF, then a
  SIGKILL backstop). The session file survives, and the same sid resumes
  through `Open` — this is a reap, not a delete. Carries no token because the
  reply is not the outcome: `stop` answers `{"stopping": true}` the instant
  the ladder starts, and the authoritative completion signal is
  `Event::SessionExit`, which the daemon may deliver *before* that reply
  since events and responses travel independent paths (docs/DAEMON.md
  §stop).
- `AnswerDialog`: passthrough `extension_ui_response`. Carries no token
  because there is nothing to correlate: pi resolves an
  `extension_ui_response` with `null`, and the daemon replies the same way
  for a dialog id it has already retired — success and "answered too late"
  are indistinguishable by design (docs/DAEMON.md §Dialogs).

**`Event` (bridge → UI).** Pi payloads stay verbatim `Value` — the client
speaks Pi. An event answering a command echoes that command's `RequestToken`;
where a session-scoped answer also carries `sid`, that `sid` is routing (which
chat renders it), not correlation. Everything else is unsolicited (connection
lifecycle, the session event stream) and carries no token.
- `Attached`: a session was opened — attach completed and history was
  replayed via passthrough `get_entries`, with `entries` being pi's list
  verbatim. This is the frontend's cue to *create* the chat for `sid`.
- `AttachmentLeft`: this bridge successfully detached from its previous
  session. Emitted before the flow creates or attaches its replacement, so
  frontend state tied to that exact attachment room can retire immediately.
- `HistoryReplaced`: the same `get_entries` snapshot as `Attached`, but as a
  repair after events were lost (a daemon `event_gap`, or the attach window
  itself overflowing) for a session the frontend already has a chat for.
  Split from `Attached` because the two mean different things and conflating
  them was a real bug: rebuilding the chat entity on a repair is what let a
  late resync clobber the chat on screen. A `HistoryReplaced` should replace
  the existing chat's entries; if there's no chat for `sid`, ignore it — the
  bridge only ever repairs the session it is attached to.
- `HistoryExtended`: the cheap repair variant — `entries` are only those
  *after* the cursor the last fetch left (docs/SPEC.md C6), so they extend
  the frontend's replica rather than replacing it. Same "ignore if no chat"
  rule as `HistoryReplaced`. The frontend must drop whatever it built from
  live events before applying these: live events carry no entry ids, so the
  delta is the authoritative account of the very span the frontend was
  guessing at while attached. `Transcript::extend_history` encodes this rule
  once.
- `Pi`: one live pi event for an attached session, verbatim.
- `RequestFailed`: a request failed daemon- or pi-side. Statusbar material —
  the connection itself is still healthy.
- `PromptFailed`: the prompt for `sid` never reached pi (the daemon or pi
  refused it, or the connection broke under it). Sid-scoped rather than
  machine-scoped because the damage is sid-scoped: the frontend already
  rendered that prompt and raised its busy indicator, and only the chat
  showing `sid` can end that turn and say why. The machine-wide
  `RequestFailed` it used to ride mapped to a start-phase reset, which is a
  no-op on a live session — the prompt sat there forever with a spinner. The
  flow also logs the detail to stderr, since the only UI for it is one chat
  that may no longer exist.
- `ModelsListed`: answers `ListModels`; pi's `data` verbatim (the client
  speaks Pi — the workbench extracts what its picker renders). Errors render
  inside the unstarted chat, not the statusbar.
- `PiAnswered { sid, token, method, result }`: answers `PiRequest`. `result`
  is pi's `data` verbatim on success and pi's own error string on failure.
  `token` is the correlation (the asking control's slot rejects anything
  else); `sid` is routing (which chat renders it). `method` is the `type`
  field of the `cmd` that was sent, echoed by the one generic flow, and it
  exists for the consumer that is *not* the asking control: the statusbar
  reports a failed `set_model` no matter which chat asked, and re-deriving
  that from the token alone would mean re-typing the command the frontend
  just erased. Empty when the command carried no `type` — a malformed request
  nobody can build from the app today.

**`BridgeContext` fields and their invariants.**
- `attached: Arc<AsyncMutex<Option<String>>>` — the one session this bridge
  is attached to (single chat pane per bridge). The *async* mutex also
  *serializes* attachment changes: a second `Open` waits out the first flow
  rather than interleaving with it.
- `attach_buffers` — live events held back while an attach flow is still
  fetching history (see `attach_window.rs`).
- `resyncing: Arc<Mutex<HashSet<String>>>` — sessions with a repair already
  in flight, so a burst of gaps costs one `get_entries` rather than one per
  gap.
- `cursor: Arc<Mutex<Option<SessionCursor>>>` — the attached session's replay
  cursor from the last `get_entries` (docs/SPEC.md C6). One slot only,
  because one bridge holds exactly one attachment.

`SessionCursor` pairs pi's `leafId` with the sid it was fetched for. The sid
half is not optional convenience: a cursor is only ever valid for the session
it came from, and pairing them makes spending it on the wrong session
structurally impossible rather than merely unlikely (e.g. after a
same-bridge session swap).

**Bridge main loop notes.**
- The sidebar renders from `sessions_flow`, so it's fired immediately on
  connect.
- In the `tokio::select!` loop: a `None` from the command channel means the
  UI dropped the bridge — dropping the `client` tears down the connection and
  any ssh forward. A `None` from `session_events` means the client itself
  stopped, so this bridge is done; there's no attachment left to explicitly
  forget because this whole context (and its daemon client) dies with the
  loop.
- `EventGap` is recovered in the backbone, not the UI: refetch the history
  that was just lost and republish it through the same events every frontend
  already knows how to render (`HistoryReplaced`/`HistoryExtended`).
- `dispatch`: every command is its own spawned straight-line flow so the
  select loop above never itself awaits on the daemon — event forwarding
  therefore cannot stall behind a slow command.
- `handle_pi_event`: dialog requests ride this same path as any other pi
  event — answering a dialog is the frontend's obligation now (docs/SPEC.md
  C5), and the bridge's only job is to put the question in front of it
  unaltered.

**Test harness note.** `Harness` wires a `BridgeContext` to a fake daemon over
an in-memory `tokio::io::duplex` pair — this is the whole reason
`DaemonConnection` stopped being hardwired to `UnixStream`.

Recorded regression coverage (why these tests exist, not just what they
assert):
- A session that exited must not be detached from on the next `Open` — the
  bridge used to remember an attachment forever (cleared only by
  `detach_previous`), so opening a second session after the first one's pi
  exited sent a `Detach` for an already-dead sid, surfacing the daemon's
  refusal to the user as a statusbar error for an action they never took.
- Forgetting an attachment on exit must be scoped to the session that
  actually exited, or one chat pane can silently end up with two live
  attachments.
- A gap for a session nobody is watching must cost nothing — `event_gap`
  handling used to spawn a full `get_entries` without consulting `attached`,
  so unwatched sessions still paid for a history refetch.
- A burst of `event_gap`s must cost exactly one refetch, not N concurrent
  ones.
- A repair must publish as its own event (`HistoryReplaced`), not reuse
  `Attached` — `Attached` means "create the chat", conflating the two let a
  late-landing resync clobber the chat already on screen.

## crates/harvester-client-core/src/client.rs

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

`last_request_id` starts at the handshake ping's id (`HANDSHAKE_PING_ID`), so
the first real request issued is the one immediately after it — request ids
share one monotonic sequence with the handshake instead of restarting.

`DaemonClient::start` splits a `DaemonConnection` into a background routing
task (owns the reader) and a client handle (writer + pending map). Dropping
both the client handle and the event receiver is what tears the connection
(and any ssh port forward backing it) down — there's no separate explicit
teardown call.

`resolve`: when a pending waiter for a response id can't be found, that means
the requester's task already gave up (e.g. it was dropped mid-await) — this is
logged as noteworthy but is not treated as fatal, since the daemon did nothing
wrong.

The `Pending` RAII guard (inner helper type, not `token::Pending`) exists
because the ways a request's pending-map entry needs cleaning up outnumber the
ways it resolves normally: a failed write, a cancelled future (the caller's
task dropping mid-await — which the UI does routinely whenever a flow is
abandoned), or ordinary resolution. Without RAII, a leaked entry means a
waiter that can never be woken and a `HashMap` that only grows — so removal
cannot safely live at a single call site and must be tied to the guard's drop.

`envelope_mismatch`: the daemon echoing a request's id back on the *wrong*
envelope type (e.g. a `Harvester` request id showing up on a `PiResponse`) is
a daemon-contract violation — impossible under docs/DAEMON.md's contract, so
it's surfaced as an error to report rather than something the client tries to
recover from.

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
  connection, so id 0 is unambiguous; `client.rs` starts its own request ids
  above it.
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
(ssh.rs) rather than on this invocation: the *master* connection owns the
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

`process_is_alive` shells out to `sh -c 'kill -0 <pid>'` (`pid_liveness_script`)
rather than pulling `libc` in for one syscall — the same trade the project
makes elsewhere, and `kill` is a shell builtin so nothing has to be
installed. A probe that fails to run at all is read as *alive*, because
"cannot tell" must not become "delete it".

**Cross-reference**: docs/DAEMON.md §Protocol, §ping.

## crates/harvester-client-core/src/dialog.rs

Pi's dialog vocabulary: the four blocking questions an extension can raise, and
the three answer shapes. This is the one place in the client that reads a
dialog payload's shape — everywhere else the client speaks Pi verbatim, but a
modal has to know whether to draw a list, a yes/no, or a text box, and that
decision is pi's wire format, not a rendering choice. Keeping the parsing here
(instead of the gpui crate) lets the vocabulary be tested without a window.

**Pi-version-coupled**: pi 0.83.0, `dist/modes/rpc/rpc-types.d.ts:385-455`.
Two shape facts are easy to get wrong and are the reason this module exists:

- Both request and response envelopes are **flat**. There is no `params`
  object — `title`, `options`, `message`, etc. sit beside `type`/`id`/`method`
  because pi spreads the request into the envelope (`rpc-mode.js:76`).
- The answer field is **not** called `response`. It is `value` (a string — for
  `select` the option's *text*, never its index), `confirmed` (a bool), or
  `cancelled: true`. Pi reads these fields in that priority and a second field
  present alongside would be silently ignored (`rpc-mode.js:83-85`), so
  `DialogAnswer::to_response` sets exactly one.

`DIALOG_METHODS` (`select`, `confirm`, `input`, `editor`) are the
`extension_ui_request` methods that actually block pi until answered. The
fire-and-forget UI methods (`notify`, `setStatus`, `setWidget`, `setTitle`, …)
are forwarded to the client but never answered — `DialogRequest::parse`
returning `None` for them is what keeps a response off the wire for an id pi
is not waiting on. Sending an answer for a non-blocking method would be a
response to nothing.

`DialogRequest.id` is pi's dialog id, echoed back verbatim in the answer — the
only correlation there is, since a response carries no `method`.

`DialogKind::Editor` is unique among the four: pi gives it **no timeout and no
abort path** (`rpc-mode.js:173-192`), so an unanswered editor dialog blocks the
extension for the life of the process. Nothing rescues it but an answer — this
is why the client must never leave a dialog unanswered (see docs/DAEMON.md
§Dialogs).

A missing `title` on a request is not treated as fatal — the dialog still
blocks pi regardless, so showing it untitled beats leaving it unanswerable.

Pi omits absent optionals rather than sending `null` (it serializes with
`JSON.stringify`), so "absent" and "null" collapse to the same `None` when
reading fields like `placeholder`/`prefill`.

`DialogAnswer::Cancelled` corresponds to Escape, or to a dialog Harvester could
not render — identical in effect to a human dismissing the dialog in pi's own
TUI. For `confirm` specifically, pi has no distinct cancellation shape: a
cancelled confirm resolves to `false`, the same outcome as `confirmed: false`
(`rpc-mode.js:84`). That equivalence is what lets the modal map Escape onto
"No" for confirm dialogs — the one dialog kind where dismissing counts as a
real answer rather than a refusal to answer.

## crates/harvester-client-core/src/dialog_obligation.rs

Client-side state machine for dialogs forwarded by the daemon while a client
is attached (pi stays suspended until answered). This module is deliberately
window-free — a GPUI modal is only an adapter over the "front obligation".

Core invariant: **empty-room cancellation is daemon policy, not client
policy.** Leaving a room here only retires the client's *view* of questions
the daemon has already cancelled (or is about to); this module never
manufactures a `cancelled: true` answer itself. Retirement removes obligations
without ever sending pi an answer.

Room model: one active room (`machine` + `sid` pair) per machine, tracked
independently, so rooms on different machines never retire each other and
share one global FIFO presentation queue (`DialogObligations.pending`). This
lets multiple machines' dialogs queue fairly rather than one machine starving
another.

`enter_room`: switching a machine's active sid (including to a brand-new sid)
retires only that machine's previously-owed questions. Attachments on other
machines are untouched and keep their queue position.

`leave_machine` (bridge disconnect) empties only that machine's room, without
touching the queue for other attached machines.

`retire_room` (attachment leave or pi's authoritative session exit) retires
only the exact matching room. Session ids are always paired with a machine
rather than assumed globally unique — consistent with how every other bridge
event is routed.

`enter_room` and `leave_machine` share one predicate — retire everything on
that machine — because an obligation belonging to the room being entered
cannot be pending in the first place: `enqueue` only accepts obligations for
the machine's current room, and every path that changes that room retires
what it replaces. The disjunct that used to spare "obligations already in the
new room" was therefore dead, and the two operations differ only in what they
leave in `rooms_by_machine`.

`retire_room` (attachment leave or pi's authoritative session exit) keeps the
one guard that is *not* redundant: it clears `rooms_by_machine` only when the
entry still names the room being retired. A `SessionExit` for a sid that has
already been replaced must retire that sid's obligations without evicting the
room that replaced it — evicting it would make every later `enqueue` for the
new room a rejection, which is a permanently wedged queue, not a lost dialog.
`a_stale_session_exit_cannot_close_the_room_that_replaced_it` pins exactly
that.

Retirement returns the obligations it dropped and nothing else. It used to
also report whether the FIFO front changed; the only consumer needs "was the
dialog on screen retired", which is a strictly stronger question (the
displayed obligation is always the front), so the flag was a second way to
ask something already answered.

`enqueue` accepts a forwarded request only if it belongs to the machine's
*currently* attached room and is not already pending; everything else is
`Rejected`. Identity for duplicate detection is `(room, dialog_id)`, not full
payload equality — pi replays pending requests on attach, and a replay must
collapse into the obligation already queued rather than create a second one.
`EnqueueResult` says only accepted-or-rejected: a duplicate and a
wrong-room request are the same non-event to a caller, and "visible" versus
"queued" is not worth distinguishing either, because the caller's advance is
idempotent — it shows the front when nothing is on screen and returns
otherwise.

`answer_front` accepts exactly one answer for a given key: if the modal
callback fires after the front obligation has already advanced (e.g. a stale
closure), the accept is a no-op rather than mis-answering the new front.

`len`, `is_empty` and `room` are `#[cfg(test)]`: they read internal state that
only this module's own tests ask about, and a published accessor with no
caller is a promise that constrains refactors for nothing.

Recorded follow-up: none beyond what's captured above — the module's own docs
frame this as the deliberate, finished design (single active room per
machine, global FIFO, retirement never answers).

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

The fleet is as reactor-independent as the workers it owns, for the same
reason `bridge.rs` documents: gpui's executor has no tokio reactor, so a
frontend must never be handed something it has to `await` on a tokio
primitive. Every method here is therefore *synchronous and non-blocking*:
commands go out down unbounded senders (which never block and never need a
reactor), and answers come back as events on the two receivers `FleetEvents`
hands the frontend, which the frontend pumps however it likes (gpui task,
tokio task, thread — its choice). The fleet itself never awaits, never
spawns a task, and holds no runtime; the two worker threads it starts hold
their own.

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

`Machine::Linked` keeps its `BridgeHandle` even after a failure, for two
reasons: a command sent to a dying machine still has somewhere to go (the
send becomes a no-op once the thread is gone), and retouching the machine
(calling `ensure_connection` again) is literally how a failed/lost bridge
gets retried — there is no separate retry path.

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

### `FleetError`

Every variant renders the exact sentence the statusbar showed before this
type existed. The human-readable `detail` string *is* the payload — a
frontend that collapsed this down to "not connectable" would throw away the
only diagnosis a user gets for a machine that will not come up.
`HostWorkerMissing` vs `HostWorkerStopped` distinguishes "the machines
worker thread never started, so there's no runtime to exec host ops on and
nothing to wait for" from "it started but its thread has since ended."

### `Unapplied<T>` — making a footgun unrepresentable

Bridge/machines-worker events used to be handed to the frontend bare, and
the fleet was fed by convention (the frontend had to remember to call the
record method *before* rendering). A frontend that forgot got a
`MachineState` table that silently stopped tracking reality. `Unapplied<T>`
is an opaque wrapper — the type has no public way to reach the value inside
except by handing it to `Fleet::observe_bridge_event` /
`observe_machines_event`, which applies it on the way through. This makes
the wrong order (render before record) a compile error instead of merely a
documented rule.

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
  `linked_machine_count`; dropping the row is also what drops its
  `BridgeHandle`, which is what actually stops the bridge thread.
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
  thread.
- `ensure_connection` / `connect_decision` / `spawn_bridge`: lazily
  connects. Reuses a live bridge, respawns a failed-or-lost one (retouching
  a machine is the retry, by design), spawns a fresh one otherwise. The
  decision is computed in a pure `connect_decision` before anything is
  spawned, which is what makes the connect policy unit-testable without
  threads (see the `ConnectDecision` enum, tested directly in `tests`).
  `spawn_bridge` unconditionally replaces any previous handle in the table —
  that replacement is what makes "retouch a failed machine" equivalent to
  "retry it."
- `send` / `send_host_command`: `send` routes a command to a machine's
  bridge. Link *state* still comes from link events, never from the send's
  return value, but both ways of losing a command (no row/no bridge, or a
  bridge thread that has already ended) are logged to stderr rather than
  swallowed: this is the "forget racing a send" path, and a command that
  evaporates in silence is indistinguishable from one the daemon ignored.
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

None beyond what's captured above — the module's doc comments recorded
design rationale, not open TODOs.

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
bridge forever. On the SSH arm, the remote command is re-executed through
`exec sh -c <quoted script>` rather than run directly, because sshd runs
commands through the user's *login* shell, which is only POSIX-compatible by
luck — fish and csh reject the arithmetic, async-subshell and `&&`-chain
constructs the project's scripts are written in. Re-execing through `sh`
guarantees both arms really get the POSIX shell this module promises.

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

`ExecOutput::require_success` is the crate's single "did this exec work?"
check: `Ok(())` on exit 0, otherwise the crate's `Error::Exec` carrying a
caller-supplied `context` plus the child's own stdout/stderr diagnosis
(`describe_failure`). Every non-zero exit anywhere in the crate is reported
through this path, so the shape of an exec failure can't drift between call
sites.

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

## crates/harvester-client-core/src/lib.rs

Crate-level docs describe `harvester-client-core` as the client backbone:
bootstrapping harvesterd on a machine (plain SSH or the local shell — unified
through one exec boundary, `Host`), forwarding its Unix socket, and speaking
the docs/DAEMON.md envelope. Deliberately gpui-free so a second frontend could
be built on it.

Three layers, bottom-up:
1. **One machine at a time**: `bridge` (that machine's daemon connection,
   command flows, event fan-in) and `machines` (provider listing, reachability
   probes, and the daemon-free host ops the folder picker runs directly).
2. **Machine identity**: `MachineRef` is the one place that decides what a
   stored machine string actually means. It is crate-internal: every question
   a frontend asks about a machine string (`host_for`, `label`, `state`) is
   answered by `Fleet`, which is the only caller.
3. **Across machines**: `Fleet` owns the bridge pool, the machine table, probe
   verdicts, and the connect policy — modeling a machine's reachability as one
   `MachineState` specifically so a probe verdict and an actual daemon link
   can never disagree with each other.

What a second frontend gets "for free" from this crate: connecting a machine,
listing/opening sessions, streaming a turn, browsing folders, reading every
machine's state, and the two policies that are easy to get subtly wrong —
*when* to start a bridge, and which of a probe result vs. an active daemon
link to trust when they might conflict. Everything exposed is synchronous and
event-driven on purpose; nothing here requires the caller to run its own tokio
reactor (tokio is used internally — see `fleet`'s own module docs for how that
boundary is crossed).

What this crate does **not** own, by design, and would have to be rebuilt if
someone tried to reuse it without the gpui crate: the workspace store
(`~/.harvester/workspaces.json` lives in the gpui crate, not here), the merged
newest-first session list across machines, and any rendering decision (what
color a `MachineState` maps to, what a session row says, how a chat with no
session yet behind it behaves).

Two entry points matter most for a new integrator: `Fleet::start` (the whole
client backbone) and, underneath it, `bootstrap_and_connect` + `DaemonClient`
for anyone driving the protocol directly (this is what the `harvester-cli`
dev tool does).

The exported surface is deliberately no wider than those two doors. Names
that only this crate's own layers call — `bridge::start`/`BridgeHandle`,
`machines::start`/`MachinesWorker`, `MachineRef`, the four `workspace` host
ops (`resolve_home`, `list_directory`, `validate_git_workspace`,
`resolve_git_branch`, all reached through `machines::Command`),
`RequestTokens::mint` and `DialogAnswer::to_response` — are `pub(crate)`.
Publishing them was speculative: each was offered as "a frontend that wants
less could use this", none ever had a caller outside the crate, and every
published name is a promise that constrains refactors. Re-widening any of
them is a one-word change the day a second frontend actually asks; the
vocabulary those workers speak (`bridge::Command`/`Event`,
`machines::Command`/`Event`) stays public, so nothing about the design is
hidden.

`Error` variants and when each applies:
- `Io`: local I/O — spawning ssh, the forwarded socket, serialization.
- `Exec`: a control-plane exec failed (transport error or non-zero exit),
  uniformly whether it ran over SSH or through the local shell (both cross the
  same `host.rs` boundary).
- `DaemonConnect`: the daemon never became reachable — the socket forward
  never came up, or the handshake went unanswered. This also covers the local
  machine, where there is no SSH forward at all, so the same variant has to
  mean "reachable" in a forward-free sense too.
- `VersionMismatch`: the daemon still reports the wrong version *after* a
  forced reinstall was attempted — in practice this almost always means a
  stale local daemon binary (the fix is rebuilding the musl target, see
  Build & test in CLAUDE.md).
- `Provider`: a machine provider executable misbehaved — contract violation,
  timeout, or its own `{"error"}` object surfaced verbatim (docs/PROVIDERS.md
  defines the provider contract this maps to).

## crates/harvester-client-core/src/machine.rs

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

## crates/harvester-client-core/src/machines.rs

The machines worker: one background thread (same tokio-on-a-thread shape as
`bridge.rs`) that owns everything that execs on a machine *without* a
daemon — the one-shot provider executables, the SSH probes, and the folder
picker's path work are all tokio-process work, which gpui's executor cannot
run.

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

`Command`/`Event` are the worker's UI-facing protocol. Every `Command` that
is answered carries the `RequestToken` its answer echoes; `SetAdHocMachines`
has no answer, so it carries none. On the `Event` side, the three host-op
answers carry only the token of the request that asked for them — nothing
else identifying — so a frontend applies an answer only while that token is
still the one it has outstanding (stale in-flight requests are naturally
ignored).

`Event::Listed.listed_providers`: every provider whose `list` has succeeded
at least once, this cycle or an earlier one. A provider absent from this set
has never reported, so its machines' absence from `rows` proves nothing —
this is the whole difference between "that machine was deleted" and "that
provider is broken" (consumed by `Fleet::is_missing` in fleet.rs).

`MachineRow.target: Option<SshTarget>` is `None` for the argv ssh escape
hatch (unsupported yet) — such machines list, but stay unreachable/unprobed.

Worker startup sequence and its rationale:
1. `install_bundled_providers()` — writes the bundled exe-dev provider into
   place whenever what is on disk is not byte-identical to what this build
   ships. A failure only costs the default provider; discovery still runs.
2. `sweep_leaked_forward_sockets()` — this worker is the client's one
   startup point inside client-core, running before any bridge binds a
   forward, so it's the right moment to reclaim socket files a previously
   killed client could not clean up itself.

Concurrency design of the main loop (`worker_main`):
- Listing runs off the `select!` loop on its own task and reports back
  through a channel, so a slow provider never delays a probe tick or a
  command being answered.
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
the sidebar dot renders. This logic lives here (not in the `ssh` transport
module) because a probe is a *machine* question answered with an exec:
`ssh` owns addressing, `host` owns execs, and having `ssh` reach back through
`host` to `ssh` would be a dependency cycle with exactly one caller (this
file).

`probe_provider_machines` skips machines with no `target` (the argv ssh
form) — no target means no probe, and the dot stays unknown rather than
being reported unreachable.

## crates/harvester-client-core/src/provider.rs

Implements the machine provider contract (docs/PROVIDERS.md): user-supplied
one-shot executables in `~/.harvester/providers/` that answer "what machines
exist and how do I SSH into them." Control plane only — session traffic
never touches a provider. Each operation is one local exec: an optional JSON
object on stdin, a JSON object result on stdout, diagnostics on stderr
(logged, never parsed). This file implements the mandatory core (`info`,
`list`); `create`/`delete` are a later slice.

`SUPPORTED_PROVIDER_PROTOCOL = 1`: providers reporting any other protocol
version are rejected at `check_info` (docs/PROVIDERS.md §info).

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

The write is staged (`exe-dev.next.<pid>`) and renamed into place, for the
same two reasons the daemon install is: a rename is atomic, so a reader
never sees a half-written script, and two clients installing at once cannot
interleave their writes into one path. It also dodges `ETXTBSY` from a copy
another client is executing right now. `BUNDLED_PROVIDER_MODE` (0o755) is
set on the staged file *before* the rename, so the provider is never
momentarily present-but-unexecutable — discovery would silently skip it in
that window. A staging file left behind by a failed write is removed.

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

`SshInfo::Argv` is parsed (a provider using it must still `list` cleanly)
but not yet wired into an actual connection — `target()` returns
`Option<SshTarget>` and the argv form is simply `None`. "No target" is the
whole answer the only caller (`machines::machine_rows`, which stores an
`Option<SshTarget>`) ever wanted; the hand-written error this used to build
was constructed only to be thrown away with `.ok()` one line later. The
variant still has to exist even though nothing reads its payload:
PROVIDERS.md §list makes "such machines still list, with no target"
normative, and without a variant to decode into, argv entries would fail to
parse and silently vanish from the sidebar. Such a machine surfaces as
`FleetError::NotConnectable` at the moment someone tries to use it, which is
where a user can actually act on it.

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
itself is `host::run`'s job (the crate's one exec helper) — this function
keeps only what's provider-specific: the deadline and the contract's result
classification.

`InfoResult.name` / `MachineEntry.status` are `#[allow(dead_code)]`: both are
required *by contract* (so deserializing them is itself the validation that
the provider sent them), but nothing in this crate consumes their value —
`name` because the UI uses the provider's file name instead, `status`
because Harvester's own SSH probe is authoritative.

`parse_ssh_info` deliberately is not an untagged enum over `{host,...}` vs
`{argv,...}`: untagged enums report "data did not match any variant" on
failure, which throws away the field-level diagnostic (e.g. "missing field
`host`") that callers log per-entry.

`parse_machine_list` / `parse_machine` decode entries individually so one
malformed *entry* is skipped with serde's own diagnostic instead of failing
the whole `list` result; only a malformed *result* (missing `machines`) is a
hard error.

**Cross-references**: docs/PROVIDERS.md §info, §list, §Discovery,
§Transport, §Reference implementation; docs/SPEC.md §Machine.

## crates/harvester-client-core/src/ssh.rs

Plain-SSH plumbing shared by one-shot execs (`host.rs`) and the long-lived
socket forward (`connection.rs`): target addressing plus the base ssh
invocation.

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

`SshTarget::command` builds the actual ssh invocation; `extra_args` are
inserted before the destination because ssh requires options before the
target argument.

## crates/harvester-client-core/src/token.rs

Reply correlation for the client's workers — see docs/SPEC.md §Decisions, C2.

Problem this replaces: a frontend fires a command at a worker and gets the
answer back as an event a round trip later. Historically "which request does
this answer belong to" was inferred from an echoed key (the `cwd` a model
list was asked for, the `path` a listing walked), which needed a bespoke
staleness guard per command and still could not distinguish two in-flight
requests that happen to share the same key. `RequestToken` replaces all of
that with one mechanism: mint a monotonic token per request, echo it on the
answer, and only apply an answer whose token still matches the one recorded
as outstanding.

`RequestToken` is intentionally opaque — nothing may derive meaning from the
number, only compare it for equality.

`RequestTokens` is cloneable and meant to be shared by every surface that
talks to a worker (e.g. the workbench and each chat hold the same source), so
a token identifies a request across the *whole client*, not just within one
surface. This is what keeps two surfaces asking the same worker the same
question at the same moment distinguishable. Wrapping a `u64` counter is not
a real concern — at one mint per nanosecond it takes centuries.

`Pending<T>` is "the other half" of the mechanism: minting a token is only
half of it, this type is the hand-rolled bookkeeping that used to be
duplicated three different ways in the frontend. All three needed the same
three rules: a re-ask supersedes the request before it, an answer is applied
only while its token is still the outstanding one, and everything else is
dropped on the floor. `T` is whatever context the asker needs to apply the
eventual answer (a path, a sid, or `()` when the answer speaks for itself).

`Pending::ask` **records on intent, not on delivery** — the request counts as
outstanding as soon as `ask` is called, before the command has actually
reached the worker. This means any caller whose send can fail must call
`cancel` on that failure path, or the pending state waits forever for an
answer nobody is coming to send. Anything gated on `is_waiting` (e.g. the
folder picker's modal, the first caller that needed this) stays incorrectly
gated otherwise.

`Pending::accept` returns `None` for exactly three cases that used to each
need their own bespoke staleness guard: a late answer, an answer to a
superseded (re-asked) request, and an answer to a request already applied.

`Pending::is_outstanding` peeks without consuming — the staleness test
`accept` is built on. It is private: it was published for answers arriving
in installments (a stream, a progress event), and no such caller ever
appeared, so it stays available to `accept` and to this module's tests
rather than standing in the crate's contract. `RequestTokens::mint` is
`pub(crate)` for the same reason and one stronger: minting a token outside
`Pending::ask` produces a token nothing is waiting on.

`Pending::is_waiting` answers "is any answer still wanted" without needing to
hold the specific token — for a surface (like a modal) that just wants to gate
its input while a request is in flight.

## crates/harvester-client-core/src/tokio_thread.rs

The one way this crate gets a tokio reactor. gpui drives its own executor and
never installs a tokio reactor, so no async work in this crate can run on the
caller's thread — every I/O-owning worker (a bridge's daemon connection, the
machines worker) instead lives on a dedicated std thread that builds its own
runtime and blocks on one future for its whole life. The runtime is
current-thread on purpose: one worker is one logical actor, so a work-stealing
pool would buy nothing and would cost extra threads.

This was factored out of the call sites because the *failure* case is the
interesting half: building a runtime fails only when the OS refuses a
resource, and at that point the worker cannot start at all, so its owner must
be told in whatever way that particular worker reports failure — which is the
only thing the two call sites actually disagree about (one sends a
`ConnectFailed` event, one logs to stderr).

`run_on_tokio_thread`: if the runtime fails to build, `on_runtime_error` gets
the formatted error and `main` is dropped un-polled — safe here because every
worker's main future does all of its real work only after its first poll (no
worker relies on drop-without-polling side effects).

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
the id does. `ToolBlock.diff` holds edit's pre-rendered numbered diff
(`result.details.diff`).

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

`Item` is the conversation in render order: messages interleaved with tool
blocks. Pi emits the assistant `message_end` before its tool executions start,
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
- `Transcript::streaming` exposes the in-flight assistant message. Every
  `message_update` carries the whole partial (never a delta), so applying one
  is a replacement, never an accumulation.
- `Transcript::is_busy`: a turn is running until `agent_settled`, not
  `agent_end` — retries, compaction and queued follow-ups may still run after
  `agent_end`.
- `Transcript::replace_history` answers a resync (docs/DAEMON.md §Events): the
  fetched entries are authoritative and replace everything rendered,
  including the in-flight partial (the next `message_update` will carry the
  whole partial again). `busy` survives a replace: turn state belongs to the
  event stream, and a snapshot fetched mid-turn does not end the turn.
- `Transcript::extend_history` folds a delta from `get_entries {since}`
  (docs/SPEC.md C6) onto what is already held — the cheap repair, versus
  `replace_history`'s expensive full rebuild. The live tail is truncated
  first (see the `history_len` rationale above for why it cannot be
  reconciled entry by entry); what survives is the prefix pi already
  confirmed. A session with a thousand entries repairs by fetching only the
  handful it missed. `busy` survives here too, for the same reason as above.
- `Transcript::push_user_prompt` renders the prompt before pi echoes anything
  back, and starts the turn immediately: `agent_start` may be milliseconds
  away, and the input must not look idle in the interim.
- `Transcript::end_turn` fires on `agent_settled`, or on a `session_exit` that
  ended a turn the hard way; the streaming partial is dropped with it since
  whatever was streaming will never be completed now.
- `Transcript::fail_turn` is `end_turn` plus the reason, for the turn that
  ended before pi ever saw it (a prompt the client could not deliver). The
  user's prompt stays in the list — they did type it — and the `Item::Failure`
  under it is what says it went nowhere; deleting the prompt instead would
  make the input's contents vanish with no account of why.
- `Transcript::apply` advances by one pi event and returns `false` when the
  event belongs to a slice this model does not cover yet (retries, compaction,
  queue) — the frontend need not redraw in that case.
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
- `extend_items`: results are collected into a `tool_call_id → toolResult`
  map ahead of the main walk because results land in later entries than their
  calls. A result whose call is already held finishes that block in place —
  across a delta boundary the call sits in the prefix and its result in the
  delta, and pushing a *new* block for it would render the same call twice
  (once finished, once running forever).
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
  text.

The test module docstring: pi 0.83.0's wire shapes are pinned here — every
fixture is the shape the client actually receives, and the point of this
module living outside the gpui crate is that these tests run without a
window.

## crates/harvester-client-core/src/workspace.rs

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
