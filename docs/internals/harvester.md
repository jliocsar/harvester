# harvester — implementation notes

This codebase carries no code comments (see AGENTS.md). Everything a comment
would have said about `harvester` lives here instead, one section per module.
Update the relevant section in the same change that touches the code.

## crates/harvester/build.rs

Embeds the static musl `harvesterd` (recorded as slice-3 verdict 3a) so the
shipped desktop binary can bootstrap machines with nothing else required on
disk.

- A missing musl artifact at build time is treated as a warning
  (`cargo:warning=...`), not a hard build error, specifically so that
  `cargo check --workspace` still succeeds on a fresh clone that hasn't yet
  cross-built the musl daemon. Dev builds fall back to reading the target
  directory at runtime instead (see `daemon_binary` in `main.rs`).

## crates/harvester/src/assets.rs

The one `AssetSource` gpui accepts for the whole application, plus the icons
Harvester ships itself.

- Protocol fact: gpui accepts exactly one asset source per application, and
  `gpui-component` resolves every one of its ~99 `IconName`s through it.
  Handing gpui a Harvester-only asset source wholesale would blank every
  library icon, so `HarvesterAssets` tries Harvester's embedded icons first
  and falls back to `gpui_component_assets::Assets` for everything else.
- Harvester's own icon paths live under `harvester/icons/`, not the library's
  `icons/`, specifically so a Harvester-added icon can never shadow — or be
  shadowed by — a library icon of the same name. Render code always asks for
  these by full path, e.g. `Icon::default().path(GIT_BRANCH_ICON_PATH)`.
- `EMBEDDED_ICONS`: `gpui-component` ships neither of these two icons —
  it has no `git-branch` counterpart at all, and its own `Delete` icon is a
  backspace glyph rather than a wastebasket, which is why Harvester embeds a
  Lucide `trash-2` (for forgetting a machine) and `git-branch` (for the chat
  prompt strip's branch indicator) itself.

## crates/harvester/src/catalog.rs

`Catalog` is the window-free session/workspace catalog behind `FleetStore`.
It is display state derived from Pi's session listings, **not** a second
persistence layer — Pi's files on the machine remain authoritative, and this
catalog can be discarded and rebuilt from the next listing at any time.
Workspace configuration is the one exception: it is genuine user
configuration and is coordinated here with `~/.harvester/workspaces.json`.

`Catalog::load()` loads only the user's workspace configuration; session rows
deliberately do *not* load from disk — only a daemon's `list_sessions`
answer can populate them, keeping Pi the sole source of truth for sessions.
`Catalog::from_workspaces` is a small constructor kept separate so the
catalog's rules are directly testable without starting GPUI or touching a
real home directory.

`record_sessions` replaces one machine's whole listing (never patches it)
and then re-merges all machines' rows by Pi's last-activity time. This
listing is display state; it is never used to infer or manufacture
session-file contents.

Session status transitions (`set_session_status`) silently ignore an unknown
sid — the next authoritative listing will introduce it, so there is nothing
to correct here. `session_attached` marks a row running and clears that
machine's opening guard. `session_exited` marks a row exited but
deliberately does *not* touch the opening guard on its own — an exit
reported by some other machine must not clear an open in flight elsewhere,
because opening is scoped to its own machine.

`add_workspace` is atomic with respect to the on-disk file: it writes
`workspaces.json` before updating the in-memory list, and a duplicate
workspace is a silent no-op that skips the write entirely.

`forget_machine` cascade and its ordering invariant: the file write happens
*first*, and any failure aborts the whole removal — so what the sidebar
shows and what `workspaces.json` holds can never disagree. The file is the
only place a machine is persisted, so a machine that survives the next
launch must survive this call too. Session rows are dropped unconditionally
because they are display state derived from a daemon this client will never
reach again once the machine is forgotten: a missing machine's transcripts
live only on its own disk, and nothing client-side could render them anyway
(docs/SPEC.md §Flows "Forget machine").

`begin_open` / `may_open_session` implement the single in-flight open guard:
a second click on the *same* row is the explicit retry escape hatch (it
clears and re-permits); a click on a *different* row stays blocked while an
open is in flight elsewhere. `abandon_open_on` only clears the guard when
the failing machine is the one that owns it — a failure on an unrelated
machine must not cancel an unrelated in-flight open.

`open_session` resolves purely by sid, never by a display index, because the
sidebar is a filtered view and an index there would not necessarily match
the underlying table.

`insert_started_session` is not a client-side session journal: it exists
only so a newly created session has a row before the next daemon listing
confirms it. A duplicate sid is replaced rather than rendered twice while
that listing is in flight.

`visible_sessions` / `session_visible`: remote rows are always visible
because the remote disk is authoritative for what exists there. Local rows
are scoped to *stored workspaces* so the local daemon does not expose every
repo ever opened on this host, only the ones the user has explicitly added.
`visible_session_count_on` deliberately counts the *visible* table (not the
raw one) because what the forget-confirmation dialog promises to remove is
exactly what the user can currently see.

`default_new_session_target` implements SPEC §Flows "Start session" and is
deliberately derived fresh on every open, never persisted — it is a guess at
what the user wants next, and a guess that survives restarts would become a
preference nobody actually agreed to keep. Priority order: (1) the repo of
the most recently active session that is still a stored repo on a reachable
machine; (2) else the first stored repo on a reachable machine; (3) else the
first reachable machine with no repo picked yet (the picker then offers only
"Add new…"); (4) else nothing (`ChatTarget::unresolved()`).

`unix_millis_now` returns wall-clock milliseconds — the same units Pi's
session listing orders by, needed so synthesized rows sort correctly against
real ones.

## crates/harvester/src/chat.rs

`Chat` is one session's conversation, from before it exists until after its
pi exits. A chat opens **unstarted** — client-side only, no session on any
machine (docs/SPEC.md §Flows "Start session") — and its first prompt mints
the sid that makes it live. Unstarted and live are one component with a
state, not two components sharing a vocabulary: everything the user touches
is the same on both sides of that line (pane, prompt box, model control,
input). What actually differs is small and named in `SessionPhase` and
`ModelSelection`: what a model pick *means* (a launch-line argument versus a
`set_model` the running pi must confirm), and whether the machine/repository
can still be picked — a started session's launch line (`cd <repo> && pi`)
fixes both for its pi's lifetime.

Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
whole partial message — replace, never append. `agent_settled` (not
`agent_end`) ends the turn; retries/queued follow-ups may still run after
`agent_end`.

### Constants

The prompt strip's label constants live with the policy that reads them, in
`chat/prompt_presentation.rs` (see its own section below). `chat.rs` itself
holds no constants: everything left in this file needs a `Window`, an
`Entity` or the store, and none of that has a tunable number in it.

### `ChatFlowRequest`

What a chat needs the composition shell to run on its behalf. Separate from
`ChatRequest` (bridge traffic the shell only routes): this one needs Root's
modal stack, which a pane inside the shell does not own and must not
arbitrate for itself (SPEC C11). Currently just `AddRepo { machine }`:
browse `machine` for a repository to store and then select.

### `Chat` struct fields

- `state: SessionState` — the window-free owner of lifecycle, model state,
  request correlation and transcript; everything else on `Chat` is GPUI
  adaptation only.
- `store: Entity<FleetStore>` — source of this chat's machine label, machine
  candidates and stored repositories, read on every render. Observed (not
  polled) so a provider listing that renames a machine, or a repo the user
  just added, reaches this pane without anyone remembering to push it
  (SPEC C12).
- `transcript_presentation` — message/tool/markdown policy and stable
  tool-expansion state; reads the client-core transcript without copying or
  normalizing its items.
- `focus_input` — defers focusing the prompt input to the next frame (set at
  creation; `render` is the first place a `Window` is available to focus
  with).
- `_subscriptions: [Subscription; 3]` — the input's Enter, the store's
  repaints, and the store's events. Held (not detached) so all three die
  with this pane.

### Construction

`Chat::unstarted` builds a chat with no session behind it yet; what its
prompt strip needs is asked for via `request_prompt_controls`, which the
workbench calls once it has subscribed — the seconds an uncached
`list_models` costs hide behind the user typing. `Chat::live` wraps an
existing session; `entries` is pi's history verbatim (empty for a session
that has never been prompted).

The prompt `InputState` is multi-line (`auto_grow`) but still sends on
Enter: `submit_on_enter` defaults to false, and without setting it Enter
would only insert a newline. Shift-Enter remains the newline.

### Event routing

`handle_store_event` routes the store's events to the two things this pane
asks questions of: its machine's bridge, and the machines worker its branch
indicator execs on. The one machines-worker answer a chat asks for itself is
`BranchResolved` — the branch indicator is a plain SSH exec, so it never
goes near a daemon and never reaches a bridge (SPEC §Workspace).

`handle_bridge_event` is where every pi answer for this pane arrives, rather
than being routed in from above: a chat knows which sid it renders and which
machine it runs on, and that is the whole routing rule (SPEC C12). It uses
match-guards rather than a total match because *whose* an event is is the
question — the fall-through belongs to another chat, another machine, or
`Attached` (which builds chats, so it's the workbench's concern). Two
scoping tiers:
- Machine-scoped (`ConnectFailed`, `Disconnected`, `RequestFailed`,
  `ModelsListed`): the bridge this chat runs on died, or answered the
  throwaway `list_models` an unstarted chat asked for (that request has no
  sid to be found by).
- Sid-scoped (`HistoryReplaced`, `HistoryExtended`, `Pi`, `SessionExit`,
  `SessionModelsListed`, `SessionState`, `ModelSet`): sids are UUIDs, unique
  across machines, so no machine check is needed to route these. A failed
  `set_model` is the store's to report; this pane only moves its label when
  pi *confirms* one (`ModelSet`).

`runs_on(machine)` — scoping by machine keeps one machine's failure from
cancelling a start still in flight on another; without it, a retry mints a
second sid and starts a second pi for one prompt.

### History resync semantics

- `promote_to_live`: the create → attach → prompt chain landed. An empty
  snapshot is the brand-new-session case (docs/SPEC.md C7) — the prompt
  already on screen is the only history there is, so it stays rather than
  being blanked out from under the user.
- `replace_history`: a resync (`event_gap`, or a reconnect's refetch)
  answered; the entries are authoritative and replace what's rendered.
  Deliberately mutates in place rather than rebuilding a new chat entity —
  rebuilding is what let a late resync clobber the chat on screen in the
  past, discarding the input's contents and focus. The in-flight partial
  message is dropped because the snapshot supersedes it; the next
  `message_update` carries the whole partial again.
- `extend_history`: the cheap repair — only entries after the last fetch's
  cursor (docs/SPEC.md C6). The transcript owns what that means for items it
  built from live events in the meantime.

### Branch indicator

`request_branch` reads the branch of the picked repo via one client-side SSH
exec on the machines worker — the same daemon-free path the folder picker
walks (SPEC §Workspace). Every failure is silent by design: the indicator is
the only thing depending on it, and it reports nothing rather than blocking
a prompt on a `git` call. `handle_pi_event` refetches the branch only on
`agent_settled`: that is the one moment the checkout can have moved without
the user touching a control, and there is no polling behind it otherwise.

`apply_pi_event`'s `false` return means an event this model doesn't cover
(retries, compaction, queue) — nothing moved, so nothing is redrawn.

### Model selection

`model_picks` lists every model a chat can offer, in row order. Only a chat
with no session yet offers the "default (pi chooses)" row — a running
session always has a concrete model (SPEC §Flows "Switch model") — and this
is derived from state, not stored, so there's no separate answer that could
disagree with it. The picked model travels with the row (not as an index
into a possibly-refetched list), so a pick always applies the model the user
actually read.

`pick_model`: unstarted, the pick is local (becomes a launch-line argument);
live, it costs a `set_model` the session must confirm, so the label does not
move until `handle_model_set` says pi agreed.

`pick_machine`: retargeting survives the typed prompt; only the destination
changes. Picking the current machine changes nothing (no refetch). The stale
answer from the old machine is dropped by the model control's own
correlation token. A stored repo path belongs to the machine it was stored
for, so the repo follows the machine on retarget: the new machine's first
repo, or nothing if it has none.

### Rendering notes

- The prompt box is one control for both unstarted and live states (SPEC
  §Flows: the agent-input pattern, `model · machine · repository · branch`).
  Machine and repository are pickers only while unstarted: the daemon
  launches `cd <repo> && pi`, and pi's cwd is fixed for its lifetime, so a
  started session's own launch line locks both. The model stays switchable
  (pi confirms `set_model` at runtime); the branch only ever reports.
- The prompt box floats in the pane's padding with no separator line — the
  card's own border is the boundary.
- Dropdown row keys/candidates travel with the closure that opens the
  dropdown, so a listing landing while the list is open cannot move a row
  out from under a click.
- A control with nothing to offer renders as the bare label rather than an
  openable-but-empty dropdown. `render_dropdown_control` is where that rule
  lives now — one free function shared by the model, machine and repository
  controls, so "disabled or empty means no popover" cannot be decided three
  different ways. Each `render_*_control` is left with the only part that
  really differs: which rows exist, and what picking one does.
- `render_dropdown_control` takes its rows already built rather than a
  closure that builds them, so a disabled control (a live session's machine
  and repository) still assembles rows it will not show. That is a Vec of
  labels per repaint, deliberately traded for one code path; if a control
  ever grows an expensive row list, laziness belongs in the caller's own
  early return, not back in three copies of the tail.
- The exit/starting notice and the models-failed line are
  `ui::inline_note(tone, …)`, the same primitive the folder picker's error
  uses — a tone-colored `text_xs` note. These two call sites keep their
  original wrapping behavior: a failure detail is information, and
  truncating it would hide the reason it exists to show.

## crates/harvester/src/chat/prompt_presentation.rs

What each prompt-strip control *says* and whether it can be opened, as pure
functions over state — the same split `workbench/machine_presentation.rs`
makes for machine rows. Nothing here touches a `Window`, an `Entity` or the
store, so the policy is unit-testable without gpui machinery, and `chat.rs`
is left with adaptation only (read the state, build the rows, wire the
pick).

- `DEFAULT_MODEL_ROW` ("default (pi chooses)"): the unstarted chat's
  "no model chosen" row. pi launches with its own default, and saying so is
  more honest than naming a model Harvester did not actually pick.
- `ADD_REPO_ROW` ("Add new…"): the repo picker's last row, for browsing to a
  repository Harvester has not been told about yet. It is also the *only*
  row an empty picker has, which is what makes a fresh client's first
  session reachable.
- `NO_MACHINE_LABEL` / `CHOOSE_MACHINE_LABEL`: what the machine control says
  before anything is picked and when there is nothing to pick (fleet empty
  until a provider lists).
- `CHOOSE_REPO_LABEL`: repository control's pre-pick label.
- `model_control_state` returns the label *and* whether the control opens,
  because those are one decision: the control may only be opened when there
  is a list behind it. A chosen model always names itself (a launch pick a
  refetch has not landed for yet is still what the user picked); the
  unstarted default row is pickable only once a list actually loaded, which
  is why `Loading`, `NotRequested` and `Failed` all render the same words
  with the control shut.
- `model_row_label` exists so the "default (pi chooses)" wording is written
  once, next to the state function that decides when that row is offered.
- `machine_control_label` takes the fleet's label for the picked machine as
  a plain string rather than reading the store itself, which is what keeps
  it pure. The caller asks the store unconditionally — for an unpicked
  machine the fleet answers with an empty string that this function never
  looks at.
- `repo_control_label` shows the folder name, not the path: the folder is
  what the user recognizes a repository by, and a full `/home/exedev/…` in a
  strip capped at `PROMPT_CONTROL_MAX_WIDTH` would ellipsize away exactly
  the end that identifies it. The dropdown's own rows still carry the full
  paths, which is where two same-named checkouts are told apart.

## crates/harvester/src/desktop_shell.rs

The cohesive GPUI desktop shell around Harvester's workbench: owns
application-wide UI policy and window construction. The workbench itself
stays responsible for product layout and user intent; callers of this module
only provide launch inputs already resolved before GPUI starts.

- `MAIN_WINDOW_WIDTH`/`HEIGHT` (1100×720): the centered main window's fixed
  size.
- `install_system_mono_font_family` / `resolve_mono_font_family`: asks
  fontconfig (`fc-match --format %{family} monospace`) for the system
  monospace family exactly once, before anything renders. This subprocess
  call is deliberately owned by the application shell rather than any
  rendering code, because rendering code must never shell out. If resolution
  fails for any reason, `ui` silently falls back to its own hardcoded
  default (`"Menlo"`, see `core.rs`).
- `install_widget_layer`: `gpui_component::init` must run before any other
  call into the library. The library also hardcodes light mode on init, so
  dark mode is set explicitly immediately after. `Theme::sync_system_appearance`
  is deliberately never called — nothing observes the desktop's OS-level
  appearance, which is what keeps Harvester pinned to dark mode even on a
  light-mode system.
- `install_application_commands`: installs the app-level Quit action under
  both `ctrl-q` and `cmd-q` so one build behaves natively on both Linux (ctrl)
  and macOS (cmd) without a platform-conditional binary. `gpui-component`
  itself owns the Input/Table/List/Menu context keybindings, so this function
  only needs to add the one app-level action.

## crates/harvester/src/dialogs.rs

The extension-dialog modal *policy*: pi asks, the user answers, pi unblocks.
This is the client-side implementation of docs/DAEMON.md §Dialogs. Not one
widget is built here — the chrome (footers, buttons, widths, input boxes)
lives in `ui/dialog.rs`, and this file only decides which chrome a given Pi
question deserves and what a click means to Pi.

- Protocol fact: an extension calling `select`/`confirm`/`input`/`editor`
  suspends the whole agent until a response comes back. harvesterd only
  auto-cancels a dialog into an *empty* room (no client attached) — so once
  this window is attached to a session, the obligation to answer becomes
  Harvester's. Before this module existed, Harvester discharged that
  obligation by cancelling every question sight-unseen (SPEC C5); this module
  is what replaced that behavior.
- Ownership split: `DialogRequest` (from `harvester_client_core`) owns Pi's
  wire shape — what the question *is*. This file owns the client
  obligation/modal lifecycle and how a user click becomes a `DialogAnswer`.
- `DialogCoordinator` is the GPUI adapter over the window-free
  `DialogObligations` state machine, which decides ordering, duplicate
  replay, answers, and retirement on its own. The coordinator's job is
  narrower: subscribe to the one store, own the text inputs, place the front
  modal, and send exactly one answer back through the same machine route.
- `handle_modal_available`: the repo picker releases its modal lease from an
  `on_cancel` callback *before* Root performs its own automatic pop. The
  handler must `window.defer` past both the callback and that pop, or it can
  race Root's own dialog-stack bookkeeping.
- `advance`: shows the global FIFO front only *after* acquiring the modal
  lease for Pi (see `modal.rs`). While `AddRepo` owns the lease, Pi's
  obligations remain queued and no `Root` call happens at all — this is how
  the two flows never fight over the same dialog stack.
- `answer_front`: the dialog key is checked twice — once here, once inside
  the window-free `DialogObligations::answer_front` — specifically so a
  duplicate or delayed answer callback from an already-replaced modal can
  never consume its successor's answer.
- After answering, `gpui-component` pops the dialog stack itself following the
  answer callback, and `select`'s button handler performs an equivalent
  guarded pop. Advancement to the next obligation is deferred
  (`defer_advance`) so the next dialog owner never renders underneath the
  modal that's still being closed.
- `apply_retirement`: only the flow that currently owns Root's dialog stack
  may pop it. Concretely: retiring a *queued* Pi obligation while the repo
  picker owns the modal must leave that unrelated workspace modal completely
  untouched.
- `create_input`: `editor` is Pi's multi-line question kind, `input` its
  single-line one, so the two kinds pick different `ui::dialog` input
  builders. Which *kind* wants which box is Pi policy and stays here; how
  either box is configured is chrome and lives in `ui/dialog.rs`.
- `AnswerCallback` / `open`: the `answered` callback is wrapped in `Rc`
  because every exit path out of the dialog needs a reference to it, and the
  library's dialog builder closure is an `Fn` that gets re-run every frame, so
  nothing passed into it can be a consumed `FnOnce`. The alias exists so the
  three per-kind wrappers below can all take the same shared callback.
- `open_option_list` / `open_question` / `open_text_entry`: the whole Pi-facing
  half of dialog chrome — each takes the shared answer callback and hands
  `ui::dialog` two plain callbacks, translating a click into the right
  `DialogAnswer`. `ui::dialog` therefore never sees a `DialogAnswer`,
  a `DialogKind`, or anything else from `harvester-client-core`.
- `open_question`: dismissal (Escape / close) answers "no" rather than
  cancelling outright, because Pi resolves a cancelled `confirm` to `false`
  anyway — so treating Escape as a refusal loses no information and removes
  one more way for the agent to hang waiting for an answer. Its labels are
  Pi's own vocabulary ("Yes"/"No"), which is why they are passed in rather
  than baked into the shared confirmation chrome.
- `open_option_list`: the option's *text itself* is the answer; Pi has no
  notion of an index here, so the chosen label round-trips straight back as a
  `DialogAnswer::Value`.

## crates/harvester/src/main.rs

Harvester's desktop-client entry point (see docs/SPEC.md).

- Two launch forms: plain `harvester` (machines come from providers and the
  workspace list) versus `harvester <ssh-destination>` (adds that destination
  as an ad-hoc machine — the dev workflow). Argument parsing is deliberately
  manual: `clap` is scoped to `harvester-cli` only, and a single positional
  argument here doesn't earn pulling the dependency into this binary too
  (see CLAUDE.md "Earn every dependency").
- `EMBEDDED_DAEMON`: the static-musl `harvesterd` baked in by `build.rs`.
  It's empty (zero bytes) in dev builds made without the musl artifact having
  been built first — `daemon_binary()` then falls back to reading
  `DEV_DAEMON_PATH` off disk (relative to the repo-root cwd of
  `cargo run -p harvester`) instead of failing outright.

## crates/harvester/src/modal.rs

Harvester's narrow arbitration layer over `gpui-component`'s single global
Root dialog stack.

- Protocol fact/constraint: Root stores every dialog in one vector, and
  `close_dialog` always pops the *top* of that stack. Because of that,
  Harvester permits exactly one of its modal flows to own the stack at a
  time — Pi's FIFO dialog obligations, the repo picker (`AddRepo`), or the
  forget-machine confirmation (`ForgetMachine`). Anything else risks one
  flow's `close_dialog` popping a different flow's modal.
- `ModalLease` is generation-tagged specifically so a delayed callback from a
  flow that has already released its lease is harmless: it can never be
  mistaken for owning the (possibly reacquired) lease.
- `ModalAvailable` is emitted only *after* a valid lease releases the stack
  for another flow to use.
- `ModalArbiter` is deliberately window-free and knows nothing about dialog
  bodies, queues, or callbacks — those responsibilities stay entirely with
  the individual flows (e.g. `DialogCoordinator`); the arbiter only tracks
  who currently owns Root.
- `ModalCoordinator::release`: Pi is the only flow that ever *waits* for the
  lease — every other flow (repo picker, forget-machine) starts from a user
  click, which Root's own focus trap prevents while Pi owns a dialog. So a
  `ModalAvailable` event is only emitted when a non-Pi owner releases;
  avoiding emitting it for a Pi release also avoids synchronously
  re-entering the very `DialogCoordinator` that triggered the release.

## crates/harvester/src/models.rs

Model-domain types shared by the unstarted chat's picker (pre-session) and
the live chat's mid-session `set_model` call: this module holds only what the
UI renders and what a launch line or `set_model` call needs. The types are
extracted from pi's verbatim `Model` objects — the payloads themselves are
never normalized (SPEC "the client speaks Pi").

`ModelChoice.name` is a display string: pi's `name` field, falling back to
`id` when pi omits it. `model_choice_from_value` drops any model object that
lacks an `id` or `provider` — such an entry could never go on a launch line
or a `set_model` call, so it is not worth surfacing in the picker.
`extract_model_choices` reads pi's verbatim `get_available_models` answer and
maps its `models` array through `model_choice_from_value`, silently dropping
anything that fails to parse.

## crates/harvester/src/session_state.rs

Window-free state machine for one Pi chat. `Chat` (chat.rs) is a GPUI
adapter around this module: session lifecycle, model requests and their
correlation, exit state, and transcript mutations live here so they can be
tested without a window and cannot accidentally grow a second (rendering)
source of truth. Pi values enter and leave this module as
`serde_json::Value`; the state machine only extracts the small pieces needed
for controls, while pi events and history are handed to `Transcript`
unchanged — consistent with the project-wide rule that pi payloads cross
every layer verbatim.

### Constants

- `SESSION_NAME_MAX_CHARS` (48): cap on the session name derived from the
  first prompt's first line (used for pi's `--name`).
- `PI_AGENT_SETTLED` ("agent_settled"): pi's turn-completion event
  (docs/SPEC.md §Rendering pi) — deliberately not `agent_end`, after which
  retries and queued follow-ups may still run.

### `ChatTarget`

One machine and repository a chat can target. Either half may be unpicked
while a chat is unstarted: a client whose fleet or repo list is empty still
opens a New session, it just can't send from it yet (`is_resolved`). A
started chat is resolved by construction — the daemon launches
`cd <cwd> && pi`, so neither half can move afterwards.

### `is_turn_settled`

Whether a pi event ends a turn; this is when the branch indicator refetches,
because a turn boundary is exactly when pi may have moved the checkout.

### `SessionPhase`

The one-way lifecycle of a chat: `Unstarted` → `Starting { sid, name }` →
`Live { sid }`. Never moves backward except via explicit failure recovery
(`fail_start`, `fail_pending`), which resets a failed `Starting` chat back to
`Unstarted` so the user is not stuck facing a ghost session.

### `ModelsState`

Model-list loading has an explicit `NotRequested` state, distinct from a
loaded empty list and from a failed request — this avoids overloading an
`Option` as both a spinner flag and data.

### `ModelSelection`

What the model label means in each lifecycle state:
- `LaunchDefault` — unstarted, pi will choose on its launch line.
- `Launch(ModelChoice)` — unstarted, a launch-line model was picked.
- `LivePending` — live, waiting for pi's authoritative current model.
- `Live(ModelChoice)` — pi confirmed the model currently in effect.
- `LiveUnavailable` — pi answered the current-model request with an error.

### `ChatIntent`

Intents emitted by the state machine; routing to a machine bridge remains
the GPUI/workbench concern, so this type carries no sender or window.
`is_pre_session()` marks `ListModels`/`Start` as intents that may queue
while the bridge is bootstrapping.

### `SessionState` fields

`branch: Option<String>` is purely an indicator — the branch the target repo
is on, as the last exec reported it. A failure clears it and blocks nothing
else.

### Method-level invariants

- `retarget` / `pick_repo`: both only apply before a session exists — the
  daemon's launch line fixes a running pi's cwd and host for its whole
  lifetime. The caller must cancel the old model fetch before this reset, so
  its answer cannot fill the new machine's/repo's picker (see
  `Chat::pick_machine` for the caller-side half of this contract).
- `request_branch` / `cancel_branch` / `accept_branch`: mint a token, or
  `None` when there's no repo to ask about; re-asking supersedes any answer
  still in flight (token-based staleness rejection). A failed branch exec
  clears the indicator rather than reporting — the branch is never worth a
  sentence the user must dismiss.
- `forget_branch`: called whenever the target changes — the repo the
  indicator described is no longer the target's.
- `request_model_list`: the unstarted chat's model list runs a throwaway pi
  *in a cwd*, so a chat with no repo picked yet has nothing to ask and stays
  `NotRequested` rather than spinning on a request it never made.
- `accept_models`: a superseded or cancelled token is stale and rejected —
  protects against a retargeted chat accepting an answer for the old target.
- `fail_pending`: the bridge failed before an answer could arrive.
  Cancellation is part of the state transition itself: a late answer must
  not revive a control that has already moved on.
- `cancel_model_fetch`: explicit cancellation (chat retargeted or
  discarded) — the old answer is stale even if the bridge later delivers it.
- `fail_start`: a start command failed; other request failures must not
  rewind a live chat or a different in-flight start (distinguished by phase
  check).
- `pick_model`: unstarted picks are local launch settings (no round trip);
  live picks are intents whose label changes only on pi's answer
  (`accept_model_set`).
- `accept_model_set`: a successful or failed `set_model` answer consumes its
  correlation token; failures leave the previously confirmed model
  untouched (no optimistic-then-rollback flicker).
- `submit_prompt`: empty prompts, starting chats, and exited sessions
  produce no intent — the phase machine is the single gate for "can this
  chat accept a prompt right now."
- `start`: the first prompt is what mints a session, so it is also the last
  moment the target must be complete — there is no launch line without a
  machine to run on and a cwd to run in.
- `session_name_from_prompt`: derives pi's `--name` from the first
  non-blank line of the first prompt, truncated to `SESSION_NAME_MAX_CHARS`.

## crates/harvester/src/store.rs

`FleetStore` is the client's whole state, held in one GPUI entity that views
observe (docs/SPEC.md C12). Everything that is not layout or user intent
lives here: client-core's `Fleet`, the two event pumps (bridge events and
machines-worker events), and the machine/workspace/session tables (delegated
to `Catalog`). Views hold an `Entity<FleetStore>`, read it on render, and
repaint on `cx.notify()` — nothing is pushed down into views and no view
keeps a copy that can go stale.

The store has no window: it never renders and never raises a dialog or
notification directly. What a view must *act* on leaves as a `StoreEvent`;
what a view only has to *see* is a `cx.notify()`.

`StoreEvent` is deliberately not a vocabulary of its own — a bridge event the
tables have taken their share of is passed on **verbatim** as
`StoreEvent::Bridge`. This preserves internal lifecycle signals and, more
importantly, never translates Pi's protocol into a second vocabulary (SPEC
§Architecture, "the client speaks Pi"). `StoreEvent::Machines` is a
machines-worker answer no table wanted (the folder picker's). `StoreEvent::Failed`
exists because the store has no window of its own to show a notification in —
the workbench raises it instead.

`FleetStore` fields: `argv_machine` is the ad-hoc SSH destination given on
argv, connected eagerly to preserve the argv workflow. `fleet` is
client-core's own machine/bridge state. `tokens` mints correlation tokens for
worker requests, one per process, cloned into each surface. `catalog` is the
window-free session/workspace catalog — it owns display ordering, local
scoping, opening guards, and workspace persistence; `FleetStore` only owns
*when* those changes repaint and how machine events reach it.

Construction (`FleetStore::new`) starts the fleet, wires both event pumps,
loads the catalog, then calls `track_machines()` before optionally connecting
the argv machine. Per SPEC §Flows "Reopen client": machines with stored
workspaces connect in the background so their sessions reappear unprompted;
ad-hoc machines connect on this call; provider-key machines become
connectable as soon as the machines-worker's first listing resolves them.

Both `pump_bridge_events` and `pump_machines_events` drain an mpsc receiver
on a GPUI task — an mpsc receiver is reactor-independent, so awaiting it from
GPUI's executor is exactly what the split is designed for. Neither pump
threads a window through: the store builds nothing that needs one (views
that do are subscribers, and a subscription carries its own window via
`cx.subscribe_in`). When `this.update` fails, the store has been dropped
(window closed) and the pump stops.

`handle_bridge_event` is the one place a bridge event is consumed outright
before views see it: `BridgeEvent::Sessions` is the sidebar's and nothing
else has a stake in it, so it updates the catalog and returns without being
re-emitted. Everything else goes through `apply_bridge_event` (letting the
tables take their share) and is then re-emitted verbatim via
`cx.emit(StoreEvent::Bridge { .. })`.

`apply_bridge_event` per-arm notes:
- `Connected` only repaints (the fleet already took the link state, but the
  connectivity dot still needs to move).
- `ConnectFailed` / `Disconnected` clear this machine's open guard — a dead
  link cannot finish an open.
- `RequestFailed` clears the open guard and turns the daemon's detail into a
  notification.
- `Attached` marks the session running via the catalog. This is display
  state only: the daemon's list may predate a resume the client just did,
  but disk on the VM stays authoritative regardless.
- `SessionExit` marks the row exited.
- `ModelSet` with an `Err` becomes a failure notification; the session keeps
  whatever model it had, and the answer still flows on to the chat, whose
  own token guard is responsible for dropping it.
- All other bridge events (`Sessions` — already handled earlier,
  `AttachmentLeft`, `HistoryReplaced`, `HistoryExtended`, `Pi`,
  `ModelsListed`, `SessionModelsListed`, `SessionState`, and the `Ok` arm of
  `ModelSet`) are view-owned lifecycle/chat answers with no table stake, so
  their subscribers receive them unchanged.

`handle_machines_event` is a pure router: the fleet already took the machine
table and probe verdicts, so `Listed`/`Probed` only need a repaint (a listing
can rename machines, and every label reads through that rename).
`ProviderFailed` becomes a notification. Anything else is the folder
picker's and is re-emitted as `StoreEvent::Machines`.

`track_machines()` recomputes the tracked machine set (argv machine plus
every stored workspace's machine) and hands it to the fleet — which get
probed and which connect in the background is the fleet's call, not the
UI's. It is called again whenever the workspace list changes.

`ensure_connection` lazily connects through the fleet, carrying the fleet's
failure reason verbatim into the notification — it is the only diagnosis the
user gets for a machine that will not come up.

`is_live` matters because a prompt sent down a dead link fails silently into
stderr, which looks to the user exactly like a prompt pi itself ignored — so
callers must check liveness before sending.

`forget_machine`: `workspaces.json` is the only place a machine is
persisted, so removing its entries *is* removing the machine.
`track_machines()` then recomputes the tracked set from the catalog, the
fleet drops the now-untracked row (and with it the bridge handle that keeps
its thread alive), and the sidebar ghost row disappears with the tracked set
that produced it. The catalog has already dropped the machine's session rows
by the time this runs. If argv had tracked this same machine for the whole
process, that reference is cleared too — otherwise it would silently
re-track the machine the user just asked to forget. Returns `false` (with
the reason on its way to a notification) when the configuration write
failed, in which case nothing was forgotten.

`add_workspace`: after a successful add, `track_machines()` runs again
because a brand-new ad-hoc machine may have just entered the probe set — the
workspace picker itself does not connect the machine it browses (its execs
need no daemon), so this is where a machine that just earned a workspace
first gets its bridge, the same rule applied at launch.

`begin_start` raises the open guard against the session a chat just minted;
the sidebar spinner reads it, and only a failure naming this machine may
clear it. `may_open_session` and `open_session` delegate the actual
resolve-by-sid and guard bookkeeping to the catalog; `open_session` aborts
and un-guards if the connection cannot be ensured.

`insert_started_session` synthesizes the sidebar row for a session a chat
just started, so the list shows it before the daemon's next
`list_sessions` confirms it — the catalog keeps this display-only insertion
ordered and de-duplicated, and Pi's next listing remains authoritative.

`new_session_target` derives where a New session opens from the catalog's
rule, evaluated against the machines the fleet can *actually* reach right
now — this is computed per open and never stored.

## crates/harvester/src/ui/core.rs

`core` is the one place a semantic meaning becomes a color (SPEC C11) —
everything else in `ui` should ask for a `Tone`/`Dot`, never a raw `Hsla`.

- `BASE_TEXT_SIZE` (13px) is the application's body text size, set once on the
  window root (`Workbench::render`) so every descendant inherits it. Sidebar
  rows restate it because `ListItem` brings its own text size, and restating
  the shared const is what keeps a row from silently drifting away from the
  rest of the window. Anything wanting *smaller* text asks for `text_xs`
  (the library's own token), not a second number here.
- `Tone` is what a component is being told about its *subject*, in place of
  raw color: `Muted` (secondary label), `Idle` (backgrounded — an unprobed
  machine, a finished tool), `Active` (what the user is doing right now),
  `Pending` (working, outcome unknown), `Live` (confirmed good — connected
  daemon, running session), `Failed` (failed and must be visible).
- `Tone::color`: `Active` deliberately maps to `theme.blue` rather than
  `theme.primary` — in the library's dark theme `primary` is near-white, right
  for a filled button but wrong for the word "you". `Idle` is `Muted` faded by
  `IDLE_OPACITY` (0.6), the one distinction the library itself has no token
  for; the same opacity constant backs `faint()` for tertiary text (context
  lines, disabled controls, disclosure chevrons), while `dim()` is unfaded
  `muted_foreground` for secondary text/subtitles.
- `Dot`: `Solid(Tone)` vs. `Hollow(Tone)`. `Hollow` exists because one machine
  state — never probed, never connected — is not a verdict at all; an outline
  communicates *no information* without spending a color the theme doesn't
  have. `Dot::tone()` extracts the tone for text a dot's row/link should match.
  `status_dot` is the single dot implementation shared by machine links,
  session liveness, and tool status.
- `use_mono_font_family` / `mono_font_family` / `MONO_FONT_FAMILY`: gpui
  matches font families by exact name, so the application resolves the
  system's monospace family once at startup (via `desktop_shell`, which shells
  out to `fc-match`) rather than having a rendering module spawn a subprocess
  itself. `mono_font_family()` falls back to `"Menlo"` if the app never called
  the setter (e.g. in tests).

## crates/harvester/src/ui/dialog.rs

Every piece of modal chrome Harvester draws: dialog geometry, titles, the
cancel/primary button pattern, the two dialog input boxes, and the three
`open_*` builders that assemble them. Deliberately dumb — it knows nothing
about Pi, obligations, modal leases, or machines, and takes plain strings plus
callbacks that return "did this answer stick?". The three modal owners
(`dialogs.rs`, `workbench/forget_machine_flow.rs`, `workbench/picker.rs`) all
build their chrome from here, so a dialog cannot look different depending on
which flow raised it.

- `DIALOG_WIDTH` (520px): the one modal width in the app. Wide enough for a
  prompt an extension author wrote without knowing Harvester's layout, narrow
  enough to still read as a dialog rather than a second window;
  `gpui-component`'s own default (448px) crops a `select` option of any real
  length. A machine label and one sentence about it (the forget-machine
  confirmation) is not worth a second modal geometry, which is why that flow
  no longer carries a width constant of its own.
- `EDITOR_ROWS` (6, 16): visible row range for a multi-line input before it
  scrolls. A `set_editor_text`-style body is usually a commit message, so a
  handful of lines is the right initial shape.
- `UNTITLED` / `title_or_fallback`: the title shown when a caller has no title
  to give (Pi omits its own on some dialogs). A titleless question still
  blocks the agent, so a fallback title is shown rather than the dialog being
  silently dropped.
- Button-id consts: element ids are identity within one rendered tree, and
  Harvester renders at most one modal at a time (see `modal.rs`), so the two
  confirmation call sites deliberately share one pair of ids rather than
  threading per-flow ids through the builder.
- `ActionAccent`: `Primary` for the ordinary affirmative, `Danger` for an
  action the user cannot undo from the dialog (forgetting a machine). The
  accent is a parameter rather than a per-call `.primary()`/`.danger()` so
  "which button is the scary one" is stated in the caller's own vocabulary.
- `Confirmation`: the entire text of a confirmation dialog (title, message,
  both labels, accent) as data, because the library's dialog builder closure
  is re-run every frame and a `Button` cannot be captured and reused across
  frames — only the strings that rebuild it can.
- `cancel_button` vs. `dismiss_button`: a cancel button placed inside a
  `DialogClose` footer is closed *by the footer*, so it must not also dispatch
  anything; a cancel button rendered in a dialog *body* (the folder picker,
  whose whole content is its own view) has no footer to do that for it and
  therefore dispatches `CancelDialog` itself. Two functions, because getting
  this wrong means either a dead button or a double close.
- `button_row` is the body-side counterpart of `action_footer`: same
  right-aligned cancel-then-action pattern, for a dialog that owns its own
  body layout.
- `open_select`: renders one button per option deliberately, not a searchable
  list — a `select` is a short list written by an extension author, and a
  searchable-list UI would be ceremony around what's usually three choices.
  The dialog's default OK button is replaced with a cancel-only footer because
  picking an option *is* confirming — there's nothing left for a separate OK
  to do. `on_ok` is hard-wired to return `false` because Root maps a bare
  Enter keypress to "confirm the dialog", but a `select` must only be
  confirmed by clicking an actual option button. The dialog is popped by the
  option handler itself, and only when the caller reports the answer stuck.
- `open_text`: one shape for both single- and multi-line entry — same dialog,
  different input box. After building, it explicitly focuses the input because
  `open_dialog` only focuses the dialog frame itself, not anything inside it,
  and the box this whole dialog exists to have filled in should not require an
  extra click before it accepts a keystroke. The typed value is read at
  submit time and handed to the caller as a `String`.

## crates/harvester/src/ui/mod.rs

`ui` is Harvester's visual architecture, grouped by responsibility — not a
palette or a parallel widget library. Components are built with
`gpui-component`, and every color must be resolved from `cx.theme()`; the
`ui::core` submodule exports semantic *meanings* (see `Tone`/`Dot` below) and
reusable non-color metrics only, never raw colors of its own (SPEC C11). The
`pub use core::{...}` / `prompt::{...}` / `shell::{...}` / `sidebar::{...}`
re-exports exist to keep a stable `ui::…` import path for screen-level callers
while the implementation stays split across cohesive modules (`core`,
`dialog`, `note`, `prompt`, `shell`, `sidebar`, `tool_block`, `transcript`)
rather than one broad file. Only names a *screen* calls are re-exported: a builder
used solely by other `ui` modules (`status_dot`, `section_header`) is reached
through `super::core` / kept private to its module instead, so the flat `ui::`
surface stays a list of what the screens actually draw with.
`dialog` is the one submodule declared `pub mod` instead of being re-exported
flat: its names are generic on purpose (`open_text`, `button_row`,
`action_button`), and they only read correctly qualified as
`ui::dialog::open_text`.

## crates/harvester/src/ui/note.rs

The two ways Harvester states something *about* a pane rather than in it: a
line of tone-colored text. Both existed four times as hand-rolled divs (the
folder picker's walk error, the chat's models-failed line and exit/starting
notice, the workbench's empty content pane), which is how the picker ended up
reaching for `cx.theme().danger` directly instead of `Tone::Failed` — the
same color, but a rule broken (SPEC C11). One builder each is what keeps that
from happening again.

- `inline_note` is the in-flow variant: `text_xs` and tone-colored, nothing
  more. Truncation is not baked in — the chat's notices always wrapped and a
  truncated failure detail hides its reason — so a call site that wants
  ellipsis chains `overflow_hidden().text_ellipsis()` itself, as the folder
  picker's error line does to keep the modal's height stable.
- `empty_pane_hint` is the centered variant for a pane with nothing in it: it
  fills its parent and centers on both axes. It inherits the window's body
  text size rather than shrinking to `text_xs`, because it is the only thing
  on screen in that pane and reads as content, not as an annotation.
- Both take a `Tone`, never a color, and both take `impl Into<SharedString>`
  so a caller may hand over an owned `format!` result without an extra clone.
- The statusbar's link (`workbench.rs`) is deliberately *not* one of these:
  its size already comes from `statusbar_frame`, and folding it in would add
  ellipsis behavior to a band that has never needed it.

## crates/harvester/src/ui/prompt.rs

The agent prompt card and its in-place model/machine controls.

- Sizing consts are all deliberate UX choices, not arbitrary numbers:
  `PROMPT_BOX_BORDER_OPACITY` (0.5) fades the resting border while focus still
  gets the full theme ring; `DROPDOWN_MAX_HEIGHT` (280) caps an open dropdown
  so a long model list scrolls inside the window instead of overflowing it;
  `DROPDOWN_MIN_WIDTH` (200) keeps short-label dropdowns (e.g. a two-word
  model name) from opening as a sliver under a wide trigger;
  `DROPDOWN_CHECK_WIDTH` (12) is a fixed check-mark gutter so picked/unpicked
  row labels stay aligned; `SEND_BUTTON_SIZE` (22) is the round send button's
  diameter; `PROMPT_CONTROL_MAX_WIDTH` (220) stops one long model/repo name
  from pushing the other prompt-strip controls off the row.
- `prompt_box_controls()`: controls sit `gap_4` apart deliberately — wide
  enough that each icon reads as the prefix of the label to its right, which
  is what visually separates the controls instead of a rule or middle dot.
- `DropdownRow.selected`: the choice already in effect, carries the check
  mark. `DropdownRow.divider_above`: a rule above a row meaning what follows
  belongs to a different group than what precedes it (e.g. the repo picker's
  "Add new…" is an action, not one more repository).
- `PlainTrigger`: `Popover::trigger` requires `Selectable`, which
  `gpui-component` does not implement for a plain styled `div`. The prompt
  controls are meant to stay one text run and a caret with no "selected"
  appearance, so this local newtype implements `Selectable` as a no-op purely
  to satisfy the trait bound.
- `prompt_box_dropdown`: anchors `BottomLeft` and opens upward deliberately —
  the prompt box sits at the bottom of the pane, so a downward-opening list
  would run past the window edge and snap back over its own trigger.
- `prompt_box_control`'s icon is given neither an explicit size nor color:
  `Icon` falls back to the ambient text style (this control's own `text_xs`
  and muted color), so the glyph and its label always match without a second
  set of tokens. Hover only recolors the label text (resolved after layout),
  leaving the icon as the quiet, stable marker of *which* control this is.
- `prompt_box_send`'s `enabled` flag is a look, not a guard: a chat with no
  machine or repository selected refuses the prompt inside its own state
  machine; the send button's disabled rendering is just how the strip
  communicates that *before* the user starts typing.

## crates/harvester/src/ui/shell.rs

Window chrome that is not the prompt, the transcript, or a modal: section
headers, the chat pane's header, and the statusbar frame.

- `STATUSBAR_HEIGHT` (26px): the fixed statusbar band. Tall enough for one
  line of `text_xs` with breathing room, short enough that it reads as a
  status strip rather than a second toolbar.
- `collapsible_section` is the whole disclosure control — header, chevron and
  click target in one call — because a header that toggles nothing is not a
  thing the app has, and splitting it left every caller re-wiring the same
  three pieces (the sidebar did it twice, identically). It takes `expanded`
  (not `collapsed`) so the parameter reads the same way as the glyph it
  selects, plus the caller's own click listener; the collapse *state* stays
  with the screen that owns it, since `ui` holds no state.
- `section_header` and `section_chevron` are private for that reason: they are
  halves of `collapsible_section`, and a caller reaching for either one alone
  would be building a disclosure control that cannot disclose. The chevron is
  a text glyph (`▾`/`▸`) rather than an icon asset so it inherits the header's
  own faint color and text size for free.

## crates/harvester/src/ui/sidebar.rs

The sidebar's row vocabulary — every machine and session row in the app is
built from these three, so the two lists cannot drift apart visually. Only
presentation lives here; which rows exist, what they say, and what a click
means belong to `workbench/sidebar.rs`.

- `SIDEBAR_ROW_INSET` (6px) is used for both the row's outer margin and its
  inner padding, deliberately the same number: the margin is what insets the
  hover/selection fill from the sidebar edge, and matching padding keeps the
  label optically centered inside that fill instead of hugging its left edge.
- `STACKED_ROW_DETAIL_INDENT` (14px) hangs the second line under the *title*
  rather than under the dot — it is the status dot's diameter (6px, from
  `status_dot`) plus the `gap_2` (8px) that separates the dot from the title.
  Change either of those and this must change with them.
- `sidebar_row` is a `ListItem` base, not a `div`: the library owns the
  hover/selected/disabled treatments, and machine rows rely on `disabled`
  gating the row while still rendering a live suffix button (see
  `render_missing_machine_row`).
- `sidebar_stacked_row` is one shape for both lists: dot + title on the first
  line, an optional inline `note` (transient status, e.g. "opening…") after
  the title, and an optional `detail` second line (the row's context —
  provider, or folder · machine). Both secondary texts resolve their color
  through `Tone::Muted` rather than `cx.theme().muted_foreground` directly,
  so `ui` keeps its rule that a component asks for a meaning, never a theme
  field (SPEC C11).
- `sidebar_empty_note` is the "no sessions yet" line. It is a plain note, not
  a row: an empty list has nothing to select, so it must not look clickable.



Reusable visual primitives (frame, typography, status treatment) for one
transcript tool-call block; tool-specific labeling and body policy live in
`transcript.rs` instead, keeping this module purely presentational.

## crates/harvester/src/ui/transcript.rs

Transcript presentation: messages, markdown, tool labeling/bodies, and
expansion state, layered on top of `harvester-client-core`'s `Transcript`
(the verbatim-derived conversation replica). This module adds *only* visual
policy and keys all of it by stable Pi tool-call ids, so a history repair can
replace transcript items wholesale without moving expansion state onto the
wrong block.

- `TranscriptPresentation` owns exactly the state that belongs to rendering:
  which stable `toolCallId`s the user expanded. Session lifecycle, Pi events,
  prompt state, and scrolling all remain in `Chat` — this type is deliberately
  narrow.
- The scroll handle passed into `render` is owned by `Chat`: following new
  events as they stream in is lifecycle/adaptation policy, while the
  scrollable message surface itself is presentation policy — the split keeps
  those concerns in their respective owners.
- `render_tool_block`: `ToolStatus::Running` maps to `Tone::Active` rather than
  `Tone::Pending` — a running tool is the live thing the user is watching on
  screen right now, not a "just working, outcome unknown" state.
- `markdown()`: assistant prose renders through `TextView::markdown`, whose
  state lives in the window under `id`. Unchanged settled text returns early;
  only the one currently-streaming partial reparses per chunk — this is a
  performance/architecture note tied to SPEC C11.
- `markdown_style()`: the app is pinned dark, but the library's default
  highlight theme is light, so this builds the matching dark style once
  (`OnceLock`) rather than repeatedly.
- `render_message`: user text is rendered verbatim (never markdown); only
  Pi's own prose goes through the markdown renderer.
- `render_failure`: a failed turn is Pi-authored but intentionally *not*
  rendered as markdown, because provider error strings must preserve literal
  `*` and `#` characters rather than have them interpreted as formatting.
- `render_streaming`: shows the in-flight turn's partial markdown (suffixed
  with a `▍` cursor), or a bare "…" activity placeholder before the first
  partial chunk arrives, or nothing if the transcript isn't busy.
- `tool_label`: header policy for Pi 0.83.0's built-in tools (`bash`, `read`,
  `write`, `edit`, `grep`, `find`, `ls`). Any tool name Pi doesn't ship falls
  through to a compact raw-args JSON summary instead of inventing bespoke
  label vocabulary for an unknown payload shape.
- `render_tool_body`: expanded-body policy is context line first, then either
  the edit/write diff or a plain output snapshot. Errors always show output
  regardless of diff availability, because that's where the error message
  lives.
- `context_line`: the faint first line of an expanded tool body carries just
  enough data to identify the call in full (path for read/write/edit, command
  for bash, raw args otherwise).
- `diff_line_kind`: classifies Pi's own numbered diff line format —
  `" 1 ctx"` / `"-2 old"` / `"+2 new"` — by first byte.
- `compact_args`: caps the raw-JSON summary for an unknown tool by *character*
  count (`ARGS_SUMMARY_MAX_CHARS` = 80), not by byte count, so a Unicode
  argument is never split mid-codepoint.

## crates/harvester/src/workbench.rs

The window composition shell (docs/SPEC.md C12). `Workbench` owns the current
chat selection and only the orchestration that truly needs the root window:
constructing a chat input, raising fleet failures, and placing Root's overlay
layers. Sidebar, add-repository, dialog, and chat flows own themselves behind
narrow event seams — Workbench does not reach into them.

`NO_CHAT_HINT` ("start a new session to begin") is the empty content pane's
text when there is no argv machine to report on. New session is the only way
in, and it always opens; the machine and repository are picked inside the
chat it opens, not from the empty pane. `render_content` draws whichever hint
wins with `ui::empty_pane_hint`, so the "nothing here yet" pane and any
future one cannot center themselves differently. `render_statusbar` keeps its
own `div`: `statusbar_frame` already sets that band's text size, and
`inline_note` would hand it truncation semantics it has no need for.

Field-level invariants on `Workbench`:
- `store: Entity<FleetStore>` is the single observable store; the shell reads
  it for the empty pane/statusbar and for root-window chat orchestration.
- `_dialogs: Entity<DialogCoordinator>` is retained only for its store
  subscription and modal lifecycle — the Root overlay itself is still placed
  by `Workbench::render`.
- `chat: Option<Entity<Chat>>` is the current conversation, whether or not a
  session exists behind it yet (an "unstarted" chat has no sid).
- `chat_subscriptions: Option<[Subscription; 2]>` — bridge requests and flow
  requests from the current chat. They die with the chat entity, so an
  abandoned unstarted pane cannot route through this shell after being
  dropped.
- `_subscriptions: [Subscription; 5]` covers store repaint/event, the
  sidebar-intent subscription, and the two flow subscriptions (repo added,
  machine forgotten).

Event-handling ordering rules (load-bearing, not incidental):
- `handle_store_event` is the only bridge event the shell owns; it builds or
  promotes a chat and therefore needs a `Window`. Dialog events are consumed
  by `DialogCoordinator`; chat answers are consumed by `Chat` itself.
- `StoreEvent` subscribers run *while* `FleetStore` is emitting. An
  `Attached` event may need to update the store's opening catalog, so
  `defer_attached` uses `window.defer` to finish that source update before
  constructing or promoting the chat — reentering the store mid-update is not
  safe.
- Sidebar events are similarly emitted while the sidebar entity is leased.
  Some intents (e.g. selecting a session) immediately change the sidebar's
  own selected-session highlight, so `handle_sidebar_intent` also defers
  dispatch until after that lease ends, rather than re-entering the child
  entity synchronously.
- `handle_attached`: the chat that minted a given sid is already on screen
  with the first prompt rendered by the time `Attached` arrives, so the
  handler promotes that existing chat in place (`promote_to_live`) rather
  than rebuilding it from scratch — this preserves scroll position and any
  local input state.
- `install_chat` subscribes to both of a chat's seams *before* asking it to
  report its prompt-strip state: an entity cannot emit to a subscriber that
  does not exist yet, so subscribe-then-notify ordering matters.

Machine-forgotten handling: once a machine is gone from the client's own
configuration, a chat still pointed at it has no bridge to send to and no
sidebar row to return to. Dropping the `Chat` entity is the entire teardown —
its subscriptions and pending requests die with it, exactly like discarding
an abandoned unstarted chat.

Repo-added handling: a repository the folder picker validated and stored is
routed back only to the chat that asked for it (matched by machine), which
then selects it — that's the entire purpose of the add-repository detour.

`open_new_session_chat`: New session is always available. The chat opens on
defaults the store derives (see docs/SPEC.md "Start session" under Flows),
and the user re-picks machine/model in the prompt strip if the defaults were
wrong. An already-open *unstarted* chat is left alone on a repeat click —
reopening it would only discard what the user already typed.

`handle_chat_flow_request`: the chat's repo picker asks the shell for a
folder walk because it needs Root's dialog stack, which only this shell can
lend out.

`handle_chat_request`: chat intents route against whichever bridge is live
right now. The chat entity owns its own request state and rejects stale
answers on its own; the shell's only job is supplying the window-level route
and a user-visible refusal message when the target machine isn't ready.

Render: Root owns the dialog/notification overlay stacks but does not render
them itself — placing `Root::render_dialog_layer` / `render_notification_layer`
remains this top-level shell's responsibility.

## crates/harvester/src/workbench/forget_machine_flow.rs

Forget-machine orchestration (`ForgetMachineFlow`): confirm, then let one file
write cascade. Only a machine its provider no longer lists ever reaches this
flow — the sidebar puts the trash icon on no other row kind — so there is
nothing to disconnect and nothing on the remote machine to delete. The
removal is entirely about what Harvester's local client still *claims* to
know (docs/SPEC.md §Flows "Forget machine").

Shape mirrors `AddRepoFlow`: one generation-tagged confirmation
(`ConfirmationId`) and one Root modal lease. Ending a run releases the lease,
so a delayed dialog callback cannot forget a second machine or free a
different flow's modal out from under it.

`NOTHING_IS_DELETED` — the reassurance shown on every removal confirmation. A
missing machine is unreachable by definition, so this string is a statement
of fact about what forgetting does, not a promise being made to the user.

`ForgetMachineState::machine_of`: returns the machine a still-current
confirmation names, if and only if it is still the current one — this is the
guard that rejects stale confirmations.

`ForgetMachineFlow::open`: same "second ask while one is open is ignored"
rule as `AddRepoFlow::open` — Root holds one dialog, and the lease is spent.

`open_removal_confirmation`: builds nothing itself — it fills in
`ui::dialog::Confirmation` (danger accent, "Remove"/"Cancel") and hands
`ui::dialog::open_confirmation` two entity callbacks. Both callbacks carry the
generation-tagged `ConfirmationId`, which is what keeps a delayed click
harmless, and the flow shares the app-wide dialog width for free by not
having a width of its own.

`forget`: the store owns the entire cascade behind one `workspaces.json`
write — tracking removal, the ghost sidebar row disappearing, and the
machine's sessions vanishing all fall out of that single write. This flow's
only remaining job after calling `store.forget_machine` is to end its own run
and tell the shell whose chat (if any) is now homeless. A failed write
reports itself (via the store's own failure event) and forgets nothing, but
the confirmation dialog still closes regardless — leaving it open would only
offer the user the same failing button again.

`removal_message`: the session count quoted is *sidebar rows*, which is all
that a missing machine's sessions can still be to this client — their actual
transcripts live on a disk this client will never be able to reach again.

## crates/harvester/src/workbench/machine_presentation.rs

Gives one consistent presentation for a given machine state across three
different surfaces: the sidebar, the empty content pane, and the statusbar.

`MachinePresentation`: the strings differ per surface because they are
different *sentences* about the same state, not because the state means
something different in each place. Choosing the tone (`dot`) exactly once in
`present_machine` is what prevents a failed state from rendering red in one
surface and gray in another — tone and message are decided together instead
of being independently derived per call site.

`pane_message` / `statusbar_link` are both `None` for probe-only states
(`Unreachable`, `Reachable`, `Unknown`) — nothing about a daemon connection
can honestly be reported for a machine that has only been probed, not
connected to.

`MachineState::Missing` handling: a "missing" listing is only authoritative
after its provider has actually spoken (i.e. this isn't inferred from mere
silence). The fleet/store layer owns that precedence decision; this
presentation function only names the verdict once the store has already
committed to it.

## crates/harvester/src/workbench/picker.rs

The add-repository folder picker (docs/SPEC.md §Flows "Add repository"): the
walk state machine, the host ops that drive it, and its rendering. A child
module of the composition shell, paired with `repo_flow`, so the
add-repository path owns itself end to end.

Key architectural fact: the picker is the *one* surface that talks to the
machines worker rather than to a bridge. Walking a machine's directories is
plain SSH exec work (`$HOME`, `ls`, `git rev-parse`) — it does not need a
harvesterd daemon at all. Browsing a machine therefore costs one round trip
and never waits on bootstrapping a daemon it would never use.

Deliberate split: `Walk` is pure state (where it is, what it's waiting on,
what a click decides) with no window and no entities, driven end to end by
this file's unit tests. `FolderPicker` is the view: the walk plus the
virtualized list its rows live in. This split is what makes the walk testable
without gpui machinery.

`ENTRY_LIST_HEIGHT = 280.` — fixed height for the entries list so the dialog
does not resize under the pointer when a listing arrives or a filter narrows
the results.

`PickerRequest` — the one host op a folder picker has in flight, and what its
answer needs to be applied. Exactly one at a time: rows and the Add button
ignore clicks while a request is out, so the modal never has two answers to
reconcile. The correlating token belongs to `Pending`, not to this enum.
- `Listing { path }`: the path is the *request's* value, never echoed back
  from the answer — nothing comes back over the wire that the walk should
  trust as the path, so it must track its own intent.

`Walk::opening`: starts a walk on a machine by immediately asking for
`$HOME` — no connection/daemon involved (SPEC §Workspace: "no daemon or
provider involvement"). This means browsing a machine Harvester never
bootstrapped costs one round trip, not an install.

`Walk::ask`: every request goes through this one method, which is what makes
"one at a time, matched by its token" hold by construction rather than by
convention.

`Walk::request_failed`: exists because the command may never reach the
worker at all (send failure) — without cancelling `Pending` in that case, the
modal would stay gated forever on an answer that is never coming.

`Walk::fail`: the reason a host op could not even be started is shown inside
the modal rather than as a toast/notification, because the user is already
looking directly at the picker. `Render` draws it with
`ui::inline_note(Tone::Failed, …)` with `overflow_hidden().text_ellipsis()`
chained at the call site — the same rendering as before, but the color now
comes from the tone vocabulary instead of `cx.theme().danger`, which is the
rule every other screen already followed (SPEC C11).

`Walk::accept_home`: `$HOME` landing immediately becomes a listing request
for that same directory — the walk always starts by listing home. `None`
return means the answer was stale (token mismatch) or the exec failed, with
the failure reason left in `error` for the modal to show.

`Walk::accept_listing`: on failure (permissions, vanished directory), the
walk deliberately keeps showing the *previous* directory rather than clearing
state, and only surfaces the reason via `error`.

`Walk::accept_validation`: on success there is nothing further to do — the
walk is spent, and its owning flow (`AddRepoFlow`) drops it.

`Walk::waiting_note`: which note to show is a question about *the walk*, not
about the specific request in flight. An empty path uniquely means `$HOME`
has not landed yet, and that's the only state worth naming specially.
Everything else (a listing, or the `git rev-parse` behind "Add", which used
to show nothing at all) is generically "loading…".

`EntriesDelegate`: rows are ordered directories-before-files, both in `ls`'s
own alphabetical order (`EntriesDelegate::show`). Files are shown for
orientation only — a walk can only descend into directories, so file rows
render `disabled`.

`FolderPicker` doc note: it's a view of its own because `gpui-component`
builds a dialog's body from the *root* view's render, where reading the
workbench entity directly would mean reading an entity that is mid-update —
see `crate::choices` for the general rule this follows.

`handle_store_event`: `StoreEvent` answers relevant to the picker are wrapped
into `PickerAnswer` and handled via `window.defer` — a successful answer
usually starts the next host command, and store events are emitted while
`FleetStore` itself is leased, so continuing the walk must wait until that
source update has completed (same reentrancy hazard as in `workbench.rs`).
The `BranchResolved` machines event is explicitly ignored here — it belongs
to the chat's branch indicator, not the picker.

`FolderPicker::open_dialog`: Enter key handling belongs to the focused
directory `List` (`on_ok` always returns `false`), and the dialog only closes
once validation actually emits `WorkspacePicked` — not on every Enter press.

`render_buttons`: the picker's Cancel/Add pair is the shared
`ui::dialog::button_row`, not a footer, because this modal's body *is* this
view and there is no `DialogFooter` to hang it from. That is also why Cancel
is a `ui::dialog::dismiss_button` (it dispatches `CancelDialog` itself) rather
than the footer-side `cancel_button`. The Add button's `disabled` state stays
here: whether the walk can submit is walk state, not chrome.

`join_remote_path` / `parent_remote_path`: paths are always absolute (the
walk starts at the remote `$HOME`), so the only special case either function
needs to handle is the root's trailing slash / being its own parent.

## crates/harvester/src/workbench/repo_flow.rs

Add-repository orchestration (`AddRepoFlow`): hands the daemon-free host walk
to `FolderPicker` on the machine the asking chat is already targeting. The
machine is never *chosen* here — a repository is picked inside the chat that
is about to run in it, and that chat already has a machine before its repo
picker even opens. So this flow is scoped to exactly the folder walk, its
Root modal lease, and nothing else (docs/SPEC.md §Flows "Add repository").

Lifecycle invariant: the flow owns one generation-tagged run (`FlowId`) and
one modal lease. Ending a run drops its picker subscription, invalidates the
picker, and releases the lease together, so a delayed host answer or modal
callback arriving after the run ended cannot revive or corrupt a new run.

`AddRepoState`: window-free lifecycle guard for late picker/modal callbacks
(testable without gpui). `running: Option<(FlowId, String)>` pairs the active
run's generation with the machine being browsed, so a stale callback that
still names the right machine but the wrong generation is still correctly
rejected by `is_picker`.

`AddRepoFlow::open`: a second request while a walk is already open is
silently ignored — Root only ever holds one dialog at a time, and the modal
lease is already spent on the in-progress run.

`handle_workspace_picked`: after a successful validation, the chat that asked
for this walk selects the path it validated *whether or not the store write
survived* — the folder is a valid git repo either way (validation already
confirmed that), and a failed `workspaces.json` write is separately reported
to the user via the store's own failure event.

`end`: a successful validation is emitted while `FolderPicker` is leased, so
calling `update` on it here would re-enter that same entity — not safe.
Since its pending request is already consumed by that point, simply dropping
both retained handles (`picker`, `picker_events`) is sufficient teardown.
Cancellation, by contrast, comes from Root and is allowed to invalidate the
picker immediately via `picker.cancel(cx)`.

## crates/harvester/src/workbench/sidebar.rs

Sidebar presentation and intent handling. The sidebar observes `FleetStore`
directly and owns only its local view state: the two section-collapse toggles
and the selected-session highlight. It emits product intents carrying stable
values (not row indexes) so a store repaint racing a click can never cause the
click to land on the wrong session.

Layout, top to bottom: New session button, machines listing, sessions list
(docs/SPEC.md §UI "Layout"). Notably there is **no workspaces section** — a
repository is picked inside the chat that is about to use it, and
`workspaces.json` is that picker's backing store rather than being surfaced
as a sidebar citizen with its own list.

Row chrome is not here: `ui::sidebar_row`, `ui::sidebar_stacked_row` and
`ui::sidebar_empty_note` own how a row *looks* (see `ui/sidebar.rs`), and
`ui::collapsible_section` owns the two section headers, so this module is
store data → row content → intent and nothing else. The two collapse booleans
stay here because they are this screen's state; the header component is told
`expanded` and handed the listener that flips it.

`SessionRow` stays in this module on the same split: it is the mapping from a
`MachineSession` to what a row should say (name-or-sid title, "folder ·
machine" subtitle, status tone, selected/opening flags), which is screen
policy, not presentation.

Constants:
- `SID_PREVIEW_CHARS = 8` — enough of a uuidv7 to tell sessions apart at a
  glance when a session has no name.
- `MISSING_MACHINE_DETAIL = "no longer listed"` — the subtitle shown for a
  machine its provider stopped listing; this is also the only row kind that
  can be forgotten.
- `FORGET_MACHINE_TOOLTIP = "Forget this machine"` — deliberately "forget"
  rather than "delete": forgetting only edits Harvester's local record.
  Nothing on the machine itself is touched, and (being unreachable) there is
  nothing reachable to touch anyway.

`SidebarIntent` — what a sidebar click asks the composition shell to do,
captured from the rendered row so a later store repaint cannot move the click
to a different session:
- `NewSession`: carries nothing. Where the session runs is the chat's own
  question — the prompt strip picks machine and repository (docs/SPEC.md
  §Flows "Start session").
- `OpenSession { sid }`.
- `ForgetMachine { machine }`: only reachable after the shell has asked the
  user to confirm (docs/SPEC.md §Flows "Forget machine").

`Sidebar::select_session`: the *shell* owns which chat is current; this method
only updates the highlight that presents that choice — it is not a projection
of fleet state and must not be inferred from the store.

`render_new_session_button`: this is the sidebar's one action and the only
way into every session. A chat opens with no session behind it and picks
where it runs from its own prompt strip; there is no machine or repository
chosen here first.

`render_missing_machine_row`: the one machine row that can be acted on. A
machine its provider stopped listing is the only kind Harvester can be sure
is truly gone, so it's the only kind worth offering to forget — every other
row is still listed by a live provider, and a reachable machine's rows are
not the user's to clean up from the sidebar. The trash button lives in the
row's suffix, which `ListItem` renders outside its own click-gating, so the
row itself stays disabled exactly like every other machine row.

## crates/harvester/src/workspaces.rs

The client-side workspace store persists to `~/.harvester/workspaces.json`
as plain JSON `[{machine, path}]` (docs/SPEC.md §Workspace). This is user
configuration, not session state — the VM's disk remains the sole authority
on sessions regardless of what is stored here.

`Workspace.machine` is a `<provider>/<id>` key, resolved to real ssh info
through the provider list at use time. A value without the separator is
treated as an ad-hoc ssh destination used verbatim (covers legacy entries
and machines supplied on argv).

`folder_name` returns a workspace's on-screen name: the path's last
component (or the whole path when there is no `/`). It lives in this module
because a workspace path is this module's domain, and both the sidebar and
the chat need to render it the same way.

`load()` treats a missing file as an empty list (first run, not an error).
An unreadable or corrupt file also degrades to an empty list, logging the
reason to stderr rather than failing the caller — the next successful save
will rewrite the file whole, self-healing the corruption.

`save()` reports errors to the *caller* rather than stderr, because the
statusbar (not stderr) is the right place for the user to see a save
failure. The write itself stages to a sibling file (`*.json.next`) and
renames it into place — the same atomic-swap pattern the daemon installer
uses. Writing the real file directly would truncate it before the new
content is fully written, so a crash mid-write would leave torn JSON; `load`
would then read that torn file as an empty list, and the *next* save would
persist that emptiness over the user's entire workspace configuration. Using
a sibling file (not a `/tmp` file) keeps the rename on one filesystem, which
is what makes the rename atomic.

## crates/harvester/tests/no_comments.rs

The enforcement gate for the no-comments rule (AGENTS.md §"No comments in
code"): a zero-dependency lexer that walks every `.rs` file under `crates/`
and fails on the first comment token it finds.

It is a lexer, not a grep, because a comment is a lexical token and `//` is an
ordinary substring inside string literals (URLs, separator strings). The
scanner therefore tracks: plain strings with `\` escapes, raw strings with any
`#` count (including `br`/`cr` prefixes), char and byte-char literals, and
block comments. The one genuinely ambiguous token is `'`: it opens either a
char literal or a lifetime. Heuristic — `'\` is always a char literal; `'x'`
(closing quote two characters later) is a char literal; anything else
(`'static`, `'a>`, loop labels) is a lifetime and only the quote is consumed.
Multi-byte scalar values work because the scan iterates `char`s, not bytes.

Skipped directories: `target` (build output) and `bin` (spike binaries —
throwaway code is exempt from house rules). The scan roots at the parent of
`CARGO_MANIFEST_DIR`, i.e. `crates/`, so every workspace crate is covered from
this one test regardless of which crate hosts it.
