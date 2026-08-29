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
- The artifact it looks for is
  `target/x86_64-unknown-linux-musl/daemon-release/harvesterd` — the daemon
  has its own cargo profile (`--profile daemon-release`), so the path is
  *not* `.../release/`. Why the profile exists is in
  docs/internals/harvesterd.md. The path itself is
  `harvester_proto::DEV_DAEMON_PATH`, which a build script cannot import
  normally (that would drag the proto crate and its tokio tree into the
  build-dependency graph for one string), so `build.rs` `include!`s the
  const's source file (`crates/harvester-proto/src/dev_daemon_path.rs`)
  directly — the standard cargo pattern for sharing a const with a build
  script. See docs/internals/harvester-proto.md §`DEV_DAEMON_PATH`.

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
to correct here. `session_attached` marks a row running, clears that
machine's opening guard, and makes that sid the selected one: attaching is
exactly the moment the shell builds or promotes the chat for it, so the
sidebar highlight is derived from this rather than pushed in behind it.
`clear_selected_session` is the one thing attachment cannot express — a chat
with no session behind it yet — and `is_selected_session` is the only reader
(the sidebar, per row). A sid that never got a row (a session still
`Starting`) is selected internally and highlights nothing, because there is no
row to highlight; `forget_selection_without_a_session` drops a selection whose
row disappeared, so a re-listed machine cannot resurrect an old highlight. `session_exited` marks a row exited but
deliberately does *not* touch the opening guard on its own — an exit
reported by some other machine must not clear an open in flight elsewhere,
because opening is scoped to its own machine.

`add_workspace` is atomic with respect to the on-disk file: it writes
`workspaces.json` before updating the in-memory list, and a duplicate
workspace is a silent no-op that skips the write entirely.

`forget_machine` also drops the selection when the selected session was one
of the rows it removed; a selection on another machine survives untouched.

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

`insert_started_session` stamps its synthesized row with
`harvester_proto::unix_millis_now` — wall-clock milliseconds, the same units
Pi's session listing orders by, so a synthesized row sorts correctly against
real ones. The clock reader lives in proto because proto owns the field it
fills (`SessionInfo.mtime_ms`) and the daemon stamps the same field with it;
this module had a byte-identical private copy until that duplication was
removed.

`Catalog` is the store's own table and is now reached as
`store.catalog()` from the surfaces that read it (sidebar, chat, modal). See
the `store.rs` section for why the *fleet* is not exposed the same way.

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
`models.rs` (see its own section). What is left here is the copy this pane
renders itself, one named const each (DESIGN.md §8):

- `NEW_SESSION_LABEL` (`"New session"`) — the unstarted pane's header title,
  and `pub` because `workbench/sidebar.rs` imports it for the button that
  opens exactly this pane. It was written twice, as a literal here and as a
  const there; two spellings of one product term is how a rename half-lands.
  `chat.rs` owns it rather than the sidebar because the sidebar's button is a
  way *into* this pane, and `workbench/sidebar.rs` is private to `workbench`
  and unreachable from here anyway.
- `UNSENT_REQUEST_DETAIL` (`"Not connected"`) — the reason this pane records
  against its own controls when the store's send gate refuses a command. It is
  deliberately shorter than the notification the gate raises — the toast names
  the machine and the diagnosis, while this string is composed into in-pane
  sentences via `MODEL_FALLBACK_SUFFIX`, and restating the whole toast there
  would read as a wall.
- `CONNECTION_LOST_DETAIL` (`"Connection lost"`) — what every request still
  outstanding is failed with when this chat's bridge disconnects.
- `MACHINE_CHANGED_DETAIL` (`"machine changed"`) — why an in-flight model
  fetch was abandoned. It reaches the screen: `cancel_model_fetch` parks it in
  `ModelsState::Failed`, which `render_unstarted_body` renders through
  `MODEL_FALLBACK_SUFFIX`.
- `MODEL_FALLBACK_SUFFIX` (`" — pi will use its default model"`) — the
  consequence half of every model-failure sentence. The detail is interpolated
  before it, so the const is the fixed tail rather than a whole template.
- The composer's placeholder is *not* a const here any more. It moved into
  `ui/prompt.rs` beside `create_prompt_input`, which is now the one place the
  prompt `InputState` is built — see that section. `Chat::new` calls the
  factory and holds the resulting entity; it no longer knows the placeholder,
  the row bounds or the submit-on-enter rule.
- `UNRESOLVED_TARGET_SUBTITLE` (`"Pick a machine and a repository below"`) —
  the unstarted header's subtitle before a target is picked, which names the
  next action rather than restating the title (DESIGN.md §8).

### `ChatFlowRequest`

What a chat needs the composition shell to run on its behalf — now the *only*
thing a chat emits. Bridge traffic used to be an event too (`ChatRequest`,
routed by the workbench into `FleetStore::send`); it goes straight to
`store.send_gated` from the pane that wants it, because the workbench added
nothing to that trip but a liveness check the store was better placed to make
(see `FleetStore::send_gated`). What is left here is what a pane genuinely
cannot do: `AddRepo { machine }` needs Root's modal stack, which a pane
inside the shell does not own and must not arbitrate for itself (SPEC C11).

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
- `_prompt_submitted` / `_store_repaint` / `_store_events` — one named
  `Subscription` field each, held (not detached) so all three die with this
  pane. They were a `[Subscription; 3]` array, which made the field name say
  only how many there were and pushed "which three" into this paragraph; the
  names carry it now. Drop order is not significant — none of the three
  observes another.

  `_store_events` uses `cx.subscribe_in`, like every other store subscriber in
  the crate (`workbench.rs`, `workbench/modal.rs`, `workbench/picker.rs`). It
  used to be the one `cx.subscribe`, whose handler takes the emitting
  `Entity<FleetStore>` **by value** — a clone per store event that
  `handle_store_event` never read. The cost of converging is one unused
  `_window: &mut Window` parameter on the handler, and this file already knew
  the shape from `_prompt_submitted`.

### Construction

`Chat::unstarted` builds a chat with no session behind it yet; what its
prompt strip needs is asked for via `request_prompt_controls`, which the
workbench calls once it has subscribed — the seconds an uncached
`list_models` costs hide behind the user typing. `Chat::live` wraps an
existing session; `entries` is pi's history verbatim (empty for a session
that has never been prompted).

The prompt `InputState` comes from `ui::create_prompt_input`; the reasoning
for its four settings lives in the `ui/prompt.rs` section. What stays this
pane's business is what it does with the entity: the `subscribe_in` on
`InputEvent` and the focus rule below.

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
  `PromptFailed`, `PiAnswered`): sids are UUIDs, unique across machines, so
  no machine check is needed to route these. `PiAnswered` is every
  session-scoped pi answer arriving through one arm; `handle_pi_answered`
  hands it straight to `SessionState::accept_pi_answer`, which dispatches on
  the echoed pi `method` and then lets the addressed control's token decide
  whether the answer is still wanted. A failed `set_model` is *also* the
  store's to report (statusbar) — that is a deliberate duplicate consumer,
  not a leak, because a chat that has been dropped would otherwise take the
  only signal with it; this pane only moves its label when pi *confirms* a
  switch.

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

`models::model_picks(&self.state)` lists every model a chat can offer, in row
order. Only a chat with no session yet offers the "default (pi chooses)" row —
a running session always has a concrete model (SPEC §Flows "Switch model") —
and this is derived from state, not stored, so there's no separate answer that
could disagree with it. The picked model travels with the row (not as an index
into a possibly-refetched list), so a pick always applies the model the user
actually read. It lives in `models.rs` with the rest of the strip's policy and
stays `pub` for the one caller outside a render: `handle_pick_model`, which
re-derives the list to resolve the action's index.

`pick_model`: unstarted, the pick is local (becomes a launch-line argument);
live, it costs a `set_model` the session must confirm, so the label does not
move until `accept_model_set` says pi agreed.

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
- `Chat` builds no prompt-strip chrome of its own, and since the presentation
  move it decides none of its *content* either. `render_bottom_row` reads the
  three lists it needs from entities (model picks, machine candidates, stored
  repos), asks the window whether the input is focused, and hands all of it to
  `models::present_prompt_strip`, which returns the whole `ui::PromptStrip`
  value (labels, enabled flags, rows, branch, `can_send`, `can_stop`,
  `focused`, notice). It then wraps that with a `send` and a `stop` closure and
  calls `ui::prompt_strip`. Everything visual — the ids, the icons, the
  "disabled or empty means no popover" rule, the notice's tone — lives in
  `ui/prompt.rs`; see that section for the contract. What is left in `chat.rs`
  is exactly what needs the store, the state machine or an `Entity`.
  `focused` is the one window-derived input, which is why it is a plain `bool`
  parameter rather than something `present_prompt_strip` could read itself.
- **Picks arrive as actions, not as captured closures.** The pane registers
  `handle_pick_model` / `handle_pick_machine` / `handle_pick_repo` via
  `on_action` in `render`, and each one re-reads its own candidate list and
  indexes into it. That replaced `create_prompt_strip_handlers`, a free
  function that existed only to *move* three candidate lists into `'static`
  closures so `&self`-borrowing render code could still hand them off — a
  whole function's worth of ceremony to work around a borrow.
  The rule it used to preserve was that a dropdown's row keys travel with the
  closure that opened it, so a listing landing while the menu is open cannot
  move a row out from under a click. That rule is now *weaker by construction*:
  the handler resolves `action.index` against the list as it stands when the
  click lands, not as it stood when the menu opened. This is a deliberate,
  accepted trade — the alternative is threading a snapshot through a gpui
  action, which actions are not for — and the exposure is small: these lists
  only change on a provider listing or a repo the user just added, and a menu
  is open for a second or two. If a mis-pick is ever actually observed, the fix
  is a generation token on the strip, not a return to captured closures.
  `handle_pick_repo`'s missing index is not an error: the repo picker's last
  row is "Add new…", which is *past* the end of `repo_candidates`, so a `None`
  lookup is exactly how that row is recognized.
- **Stop is the strip's second direct control.** `can_stop` comes straight
  from `SessionState::can_stop`, so the button is on screen exactly while the
  chat's session has a turn in flight — the same condition that puts the busy
  indicator in the transcript. `stop_session` re-asks the state machine for
  the command (`stop_command`) instead of trusting the flag the last frame
  rendered: a `session_exit` can land between paint and click, and the state
  machine is the only thing that knows. Sending goes through the same
  `Chat::send` as everything else, so the store's liveness gate and its
  "nothing was sent" reporting apply unchanged. A stop that races an exit the
  client has not seen yet is not an error either — harvesterd answers a
  no-longer-running sid with `{"stopping": false}` (docs/DAEMON.md §stop) —
  which is why the button needs no disabled-while-stopping state: it stays
  live until the turn it interrupts is gone, and pressing it twice costs one
  wasted round trip and nothing else.
- `model_control` / `machine_control` / `repo_control` (all in `models.rs`
  now) still build rows for a control that is disabled (a live session's
  machine and repository). That is a Vec of labels per repaint, deliberately
  traded for one code path; if a control ever grows an expensive row list,
  laziness belongs in the caller that reads the entities, not back in three
  copies of the render tail.
- The lists are built even when a notice short-circuits the strip. They are
  pure store reads, and the alternative — an early return in `chat.rs` —
  would put the notice decision in two places, which is exactly what
  `PromptStripNotice` exists to prevent.
- The branch indicator's host lookup reads `store.host_for(machine)`, which
  now returns `harvester_client_core::Result<Host>` rather than a demoted
  `Result<_, String>`; the failure is still only ever a stderr line, printed
  with `{error}` where it used to print `{detail}`.
- The models-failed line is `ui::inline_note(tone, …)`, the same primitive
  the folder picker's error uses — a tone-colored `text_sm` note. It keeps
  its original wrapping behavior: a failure detail is information, and
  truncating it would hide the reason it exists to show. The exit/starting
  notice is now the same primitive reached through
  `ui::prompt_strip`.

## crates/harvester/src/dialogs.rs

Which chrome a given Pi question deserves, and what a click on it means to
Pi. This is the client-side half of docs/DAEMON.md §Dialogs that is *about
the question*; the obligation lifecycle (queue, display, answer, retirement)
belongs to `workbench/modal.rs`, and not one widget is built here either —
footers, buttons, widths and input boxes live in `ui/dialog.rs`.

- Protocol fact: an extension calling `select`/`confirm`/`input`/`editor`
  suspends the whole agent until a response comes back. harvesterd only
  auto-cancels a dialog into an *empty* room (no client attached) — so once
  this window is attached to a session, the obligation to answer becomes
  Harvester's. Before this module existed, Harvester discharged that
  obligation by cancelling every question sight-unseen (SPEC C5); this module
  is what replaced that behavior.
- Ownership split: `DialogRequest` (from `harvester_client_core`) owns Pi's
  wire shape — what the question *is*. This file owns the translation from
  that question to modal chrome, and from a click back to a `DialogAnswer`.
- `open_pi_dialog` is the single entry point: one `DialogObligation` in, one
  Root dialog out, with the caller's `answered` callback wired to every exit
  the chrome offers. It is generic over the owning view so this module never
  names the modal coordinator; the coordinator is the only caller.
- `AnswerCallback` / `open_pi_dialog`: the `answered` callback is wrapped in
  `Rc` because every exit path out of the dialog needs a reference to it, and
  the library's dialog builder closure is an `Fn` that gets re-run every
  frame, so nothing passed into it can be a consumed `FnOnce`. The alias
  exists so the per-kind wrappers below can all take the same shared callback.
- `open_option_list` / `open_question` / `open_entry`: the whole Pi-facing
  half of dialog chrome — each takes the shared answer callback and hands
  `ui::dialog` two plain callbacks, translating a click into the right
  `DialogAnswer`. `ui::dialog` therefore never sees a `DialogAnswer`,
  a `DialogKind`, or anything else from `harvester-client-core`.
- The text box is built *here*, at display time, by the arm that needs it:
  `editor` is Pi's multi-line question kind and `input` its single-line one,
  and the two match arms differ only in which `ui::dialog` input builder they
  hand to `open_entry` as its `create_input` fn pointer (plus the seed string:
  a placeholder for `input`, a prefill for `editor`). Which *kind* wants which
  box is Pi policy and stays in the match; how either box is configured is
  chrome and lives in `ui/dialog.rs`. There is no input map and no "input
  missing" guard: an obligation that is not on screen has no input entity in
  existence, and the one that is on screen cannot be displayed twice (see
  `workbench/modal.rs`).
- Titles: `open_pi_dialog` does not compute one fallback title for every
  kind. Each arm names its own condition — `UNTITLED_SELECT_CONDITION`
  ("Choose an option"), `UNTITLED_INPUT_CONDITION` ("Enter a value"),
  `UNTITLED_EDITOR_CONDITION` ("Write a response"),
  `UNTITLED_CONFIRM_CONDITION` ("The agent needs a yes or no") — and hands it
  to `ui::dialog::title_or_named_condition`. This is why `Input` and `Editor`
  do not share a match arm: they take the same *chrome* but they are not
  the same *question*, and the shared arm was hiding that. Which words go with
  which `DialogKind` is Pi policy and stays here.
- `condition_and_consequence` is the one piece of Pi-copy reasoning in this
  file. A Pi `confirm` carries a title and a message, and extensions in the
  wild fill them in three different ways: both meaningful, only the message
  meaningful, or the message merely repeating the title. So: an empty title
  promotes the *message* to the title (and leaves the body empty); a message
  identical to the title is dropped rather than said twice; otherwise both are
  shown, title as the condition and message as its consequence. Rendering the
  degenerate cases literally is what produced dialogs that said the same
  sentence twice, or that had a generic heading over the only real sentence.
  Note the limit of that rule: **the copy inside a Pi dialog is not ours.**
  Only whitespace trimming and the drop-the-duplicate rule touch what Pi wrote;
  everything else reaches the screen verbatim, phrasing and all. The literals
  this client owns are the four untitled conditions and the confirm dialog's
  action label, and nothing else. Four unit tests pin the three branches plus
  the both-blank case, which is the one path where the untitled condition and
  the empty consequence have to appear together; the function is pure, so
  `#[cfg(test)] mod tests` beside it is enough (house rule 11).
- The machine and session id are both available on the obligation at this point
  and are deliberately kept out of the title. The dialog is modal over a
  workbench that already shows which session is attached, and a title that
  restates the surrounding window is spending the user's most valuable line on
  context they can already see.
- `open_question`: dismissal (Escape / close) answers "no" rather than
  cancelling outright, because Pi resolves a cancelled `confirm` to `false`
  anyway — so treating Escape as a refusal loses no information and removes
  one more way for the agent to hang waiting for an answer. Its labels are
  `CONFIRM_ACTION_LABEL` ("Confirm") and the shared `ui::dialog::CANCEL_LABEL`
  ("Cancel"), not "Yes"/"No". The decline word is imported rather than
  redeclared here: it was written four separate times across the app (this
  file, `ui/dialog.rs`, `workbench/modal.rs`, and as a bare literal in
  `workbench/picker.rs`), and one word on one button is one const. They still pass through `Confirmation` rather than being baked
  into the shared chrome — the forget-machine confirmation needs
  "Remove"/"Cancel" — but Pi's own vocabulary is no longer what they say:
  "Yes"/"No" only reads correctly under a question, and a Pi `confirm` title
  is very often a statement. The wire settles the argument too:
  `DialogAnswer::dismissal_for` a `confirm` is `Confirmed(false)`, so the
  decline button, Escape and an overlay click all send exactly the same
  thing — calling that path "Cancel" makes the button say what Escape does,
  where "No" implied two distinct commitments over a protocol that has one
  commitment and one non-commitment.
- `open_option_list`: the option's *text itself* is the answer; Pi has no
  notion of an index here, so the chosen label round-trips straight back as a
  `DialogAnswer::Value`.

## crates/harvester/fixtures/ (provenance)

Three checked-in pi session captures drive `tests/fixtures.rs` and the
gallery's transcript stories (which read them across the crate boundary — see
`docs/internals/harvester-gallery.md`). They live here, with the workspace
member, because the test that folds them is the transcript layer's contract
check and a clean clone must be able to run it. Each file is LF-only JSONL,
one **raw pi event object** per line — the inner event exactly as
`Transcript::apply` consumes it, never the harvesterd envelope.

- `streaming.jsonl` — one markdown-heavy answer (headings, bullets, numbered
  list, python fence, table, blockquote) with its 18 incremental
  `message_update` frames; replaying line-by-line grows `streaming()` from 2
  to 2418 chars. Frame count is provider-batched (`claude-sonnet-4-5@llm`);
  chunky 1× replay is faithful to the wire — do not subdivide frames.
- `tools.jsonl` — four turns, nine tool calls: `ls`, `read`, `write`, `edit`
  (real `details.diff`/`patch`), `grep`, a **failed** `bash`
  (`isError: true`), and `metrics_query`, an extension-registered name that
  exercises the unrecognized-tool fallback label.
- `mixed.jsonl` — three user turns interleaving `read`/`edit`/`bash` with
  markdown answers.

Captured 2026-08-18 against **pi 0.83.0** on a throwaway exe.dev VM (created
and deleted the same session), keyless LLM gateway, real RPC over SSH: stdin
held open, one `prompt` per turn, next turn only after `agent_settled`,
shutdown by dropping stdin (exit 0). The exeuntu image had already moved to
pi 0.84.2 and was pinned back with `exeuntu update pi --version 0.83.0`
before capture (see `HARNESS_RESEARCH.md` §Watch).

Sanitization was text substitution on the raw lines only, preserving pi's
compact serialization byte-for-byte: provider tool-call ids (`toolu_…`) →
`call_0001…`, provider response ids (`msg_…`) → `resp_0001…`, timestamps
rebased so each file starts at `1767225600000` with relative offsets kept.
Host/user/path scrubbing rules matched nothing — the pi event stream carries
no absolute paths, hostnames, usernames, session ids, or credentials. The
only line-level edit: RPC `{"type":"response",…}` command acks were removed —
they are responses, not session events. `tool_execution_update` appears only
twice per multi-turn capture (pi emits it only for long-running tool output),
so the running-with-partial-output render path is thinly covered.

## crates/harvester/src/lib.rs

The lib target exists for exactly one reason: the `harvester` binary and the
out-of-workspace `harvester-gallery` crate share a component layer and a
window bootstrap. It began as a two-binaries-one-package split (the gallery
was a `[[bin]]` here); the gallery has since moved to
`crates/harvester-gallery` with its own workspace, which changed the
*consumer* of this lib from a sibling bin to a path dependency and changed
nothing else. The rule stands and is now enforced by the crate boundary
rather than by convention: what the lib exports is exactly what both the app
and the catalog need, no more.

What lives lib-side:

- `ui` — the whole visual layer. It was already dependency-free with respect
  to the rest of the crate (no `crate::` path in it pointed outside `ui`
  before this split, and the only one added since is
  `assets::GIT_BRANCH_ICON_PATH` for the prompt strip's branch indicator),
  so moving it cost no rewiring.
- `assets` — `HarvesterAssets` is the one `AssetSource` gpui accepts per
  application, and both binaries install it (the gallery through
  `shell_bootstrap`).
- `shell_bootstrap` — the shared application/window start-up.

Everything else stays declared on the bin side (`main.rs`): `chat`,
`workbench`, `dialogs`, `store`, `catalog`, `session_state`, `models`,
`workspaces`. Those are the *application*, not a component
library; the gallery must not be able to reach them, and keeping them bin-side
is what enforces that — a bin's modules are unreachable from a dependent
crate, so the boundary is compiler-checked, not documented.

`main.rs` re-exports the two lib modules at the bin crate root
(`pub use harvester::{assets, ui};`) so every existing `crate::ui::…` /
`crate::assets::…` path in the app keeps resolving. This is deliberate: the
alternative was rewriting several hundred import paths for no behavioral
gain. The re-export is not a compatibility shim to be removed later — the
app's modules genuinely treat `ui` as part of its own crate root, and the
lib boundary is a packaging fact, not a layering statement.

`build.rs` is unaffected by the split: it writes `harvesterd` into `OUT_DIR`
once per *package*, and only `main.rs` reads it (`include_bytes!`). The lib
never touches it, and neither does the gallery — which does pay for it, since
depending on this package runs its build script (and prints its
no-musl-daemon warning) in the gallery's build too.

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
  `harvester_proto::DEV_DAEMON_PATH` off disk (relative to the repo-root cwd
  of `cargo run -p harvester`) instead of failing outright. `DEV_DAEMON_PATH`
  names the `daemon-release` profile's output directory, and the error it
  prints quotes the matching `--profile daemon-release` build line, so a user
  who hits the fallback can copy the command that fixes it.
- `pub use harvester::{assets, ui};` is the bin crate root adopting the two
  lib modules under the paths the app already uses (`crate::ui`,
  `crate::assets`). See the `lib.rs` section for why the split exists and why
  this re-export is the intended shape rather than a migration step.

### Starting the desktop shell

`run` and `main_window_title` (private here; folded in from the former
`desktop_shell.rs`, which was one `pub fn` with one caller) are what is
*product-specific* about starting the Harvester application: the window
title, the window size, and the fact that the root view is a `Workbench`.
Everything generic about starting a gpui application (fonts, assets, theme,
quit binding, `Root`) moved to `shell_bootstrap` on the lib side when the
gallery binary appeared — see that section for the contract. This half stays
on the bin side, because `Workbench` and everything under it is bin-only; the
lib/bin quarantine is unchanged by the fold.

- `MAIN_WINDOW_WIDTH`/`HEIGHT` (1100×720): the centered main window's fixed
  size.
- `main_window_title` takes the argv machine by reference and is a free
  function rather than a method: with the bootstrap owning the window
  lifecycle there is nothing left for a `DesktopShell` struct to hold, and
  the two launch inputs (`argv_machine`, `daemon_binary`) travel straight
  into the root-view closure instead.
- The root-view closure is where `Workbench` is constructed, so the
  bootstrap never learns the app's view type — it only ever sees an
  `AnyView`.

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

### Prompt-strip presentation

`present_prompt_strip(state, model_picks, machine_candidates, machine_label,
repo_candidates, focused) -> ui::PromptStrip` is the whole prompt strip's
content decided in one window-free function — the same shape
`workbench/machine_presentation.rs`'s `present_machine` has for machine rows,
and now the same rule stated once for the app:

> **A window-free `present_*` function returns one presentation value; the
> view only reads entities and renders it.**

Nothing here touches a `Window`, an `Entity` or the store. Every input is
either plain data (`&SessionState`, the three lists, the fleet's label for the
picked machine) or the one fact only a window knows (`focused`), which is why
that one arrives as a `bool` parameter. The whole strip is therefore
unit-testable without gpui machinery, and it is tested here: the send gate on
an unresolved target, a live session's frozen machine/repo controls, the
"Add new…" row's position and divider, the starting and exited notices, and
which model row carries the check mark.

This was `chat/prompt_presentation.rs` (label functions only) until it folded
in here and `src/chat/` was deleted, and it absorbed the rest of the strip when
`chat.rs`'s `Render` impl was found to be building ~150 lines of controls,
notice, `can_send` and `branch_label` that touched neither `Window` nor
`Entity`. Merging it the other way — into `chat.rs` — would have put a
unit-test module inside a `Render` impl that has never had one.

The private helpers are the pieces of that one value: `prompt_strip_notice`,
`model_control`, `machine_control`, `repo_control`, `is_current_model`,
`can_send` and `is_unstarted`. `is_unstarted` is named that way rather than
`unstarted` only because `Chat::unstarted` is already the constructor for a
chat with no session behind it.

`model_picks` is the one item here that is `pub` beyond the strip:
`Chat::handle_pick_model` re-derives the list to resolve a picked row's index
(see the `chat.rs` section on picks arriving as actions).

- `DEFAULT_MODEL_ROW` ("Default (pi chooses)"): the unstarted chat's
  "no model chosen" row. pi launches with its own default, and saying so is
  more honest than naming a model Harvester did not actually pick. The const
  itself now lives in `ui/prompt.rs` (the gallery renders the same row); the
  wording decision is still this module's.
- `ADD_REPO_ROW` ("Add new…", also in `ui/prompt.rs` now): the repo picker's
  last row, for browsing to a repository Harvester has not been told about
  yet. It is also the *only*
  row an empty picker has, which is what makes a fresh client's first
  session reachable. Its ellipsis is the conventional one — the row opens a
  dialog rather than committing anything — unlike a progress ellipsis, which
  the app spends far more carefully.
- `NO_MACHINE_LABEL` / `CHOOSE_MACHINE_LABEL`: what the machine control says
  before anything is picked and when there is nothing to pick (fleet empty
  until a provider lists).
- `CHOOSE_REPO_LABEL`: repository control's pre-pick label.
- Those three consts are **private** to this module now. They were
  `pub(super)` only so the old `chat/` sibling could not accidentally be the
  one place they were named; nothing outside these functions and their tests
  ever read them, and the fold removed the reason for the annotation. The four
  label functions below are private for the same reason: `present_prompt_strip`
  is the only thing that calls them now that `chat.rs` no longer assembles the
  strip itself.
- `LOADING_MODELS_LABEL` ("Loading models…"), `DEFAULT_MODEL_LABEL` ("Default
  model", read from two arms), `PENDING_MODEL_LABEL` ("Model") and
  `UNAVAILABLE_MODEL_LABEL` ("Model unavailable") are the model control's own
  labels. They were bare literals inside `model_control_state` while the three
  labels above them were consts in the same file — one file spelling the rule
  both ways, which is what made it worth finishing (DESIGN.md §8). The tests
  below still assert the words themselves rather than the consts, so a rename
  that changes the copy still has to be intended.
- `model_control_state` returns the label *and* whether the control opens,
  because those are one decision: the control may only be opened when there
  is a list behind it. It was the only one of the four that already did so;
  the other three left their enabled flag behind in the view, which is the
  asymmetry the presentation move closed. A chosen model always names itself (a launch pick a
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
  control capped at `max_w_56` would ellipsize away exactly the end that
  identifies it. The dropdown's own rows still carry the full paths, which is
  where two same-named checkouts are told apart. The last-component rule is
  `harvester_client_core::folder_name` — one implementation for the whole
  client, trailing slash and root included (see the `ui/transcript.rs` and
  `workspaces.rs` sections).

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
  retries and queued follow-ups may still run. It is imported from
  `harvester-proto`, not declared here: client-core's transcript matches on
  the same token, so it is a name two crates must agree on. Same for
  `PI_GET_AVAILABLE_MODELS`. `PI_GET_STATE` and `PI_SET_MODEL` stay local —
  nothing outside this crate speaks them.

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

### Commands

`SessionState` returns `harvester_client_core::bridge::Command` values
directly — it used to return a `ChatIntent` whose variants re-declared the
bridge's commands field for field, which the workbench then translated back
verbatim. Three vocabularies for one thing is three places to add a field to,
and the
translation layer's only original contribution (a liveness gate) now lives on
the store as `send_gated`. The state machine still carries no sender and no
window: it *returns* commands, it never sends them.

Where a command is session-scoped pi passthrough it is a
`Command::PiRequest` carrying a verbatim pi payload built here — hence the
`PI_GET_AVAILABLE_MODELS` / `PI_GET_STATE` / `PI_SET_MODEL` constants. They
are named because they are spoken twice: once to build the request and once
to recognize the answer's echoed `method` in `accept_pi_answer`.
`PI_SET_MODEL` is `pub` for one out-of-pane reader, `store.rs`, which reports
a failed model switch in the statusbar whether or not the asking chat is
still open.

### `SessionState` fields

`branch: Option<String>` is purely an indicator — the branch the target repo
is on, as the last exec reported it. A failure clears it and blocks nothing
else.

### Construction

`unstarted` and `live` are the two public constructors and they differ in
exactly three facts: the phase, the transcript they start from, and what a
model selection means. Both funnel into a private `new(phase, transcript,
selection, target, tokens)` that writes the other nine fields once — the same
shape `Chat::unstarted` / `Chat::live` / `Chat::new` already had one layer up.
Two full struct literals is how a field added to one and not the other becomes
a silent difference between a fresh chat and a resumed one.

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
- `request_model_controls`: returns `Option<[Command; 2]>` — a live session's
  refresh is always the same pair (`get_available_models` then `get_state`),
  and a non-live phase has nothing to ask. The array states the "exactly two"
  in the type; `None` replaces what used to be an empty `Vec`.
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
- `fail_prompt`: a prompt that never reached pi — refused by the daemon, or
  refused by the store's send gate before it left the client. The transcript
  already shows the prompt and a busy indicator (`submit_prompt` pushes both
  before anything is sent, because `agent_start` is milliseconds away in the
  normal case), so the turn has to be ended *here* or the pane spins forever.
  Live-only: an unstarted or starting chat has no turn to end, and its own
  failure path is `fail_pending`.
- `pick_model`: unstarted picks are local launch settings (no round trip);
  live picks are commands whose label changes only on pi's answer
  (`accept_model_set`). A live pick stashes the selection it displaces *in
  the pending slot itself* — the token and the thing to put back if the
  answer is a refusal are one fact, and `Pending<T>`'s context slot already
  exists to carry exactly that.
- `accept_model_set`: a successful or failed `set_model` answer consumes its
  correlation token. A refusal (and a success pi answered with an
  unreadable model object) restores the selection the pick displaced, so the
  control keeps naming the model the session is actually running. Leaving the
  slot alone was not equivalent: a live chat starts in `LivePending`, whose
  label is the bare placeholder "Model", and a refusal used to strand it
  there indefinitely while pi ran a perfectly good model.
- `accept_pi_answer`: routes one `Event::PiAnswered` to the control that
  asked, by the pi `method` the bridge echoed. Method picks the control;
  the token decides whether that control still wants the answer. An unknown
  method changes nothing — no pane in this app sends a pi command it has no
  answer handler for, so the arm is a floor, not a feature.
- `can_stop` / `stop_command`: the affordance and the command it sends read
  the same predicate — live phase, no exit notice, and a busy transcript — so
  the button cannot be on screen for a state the command would refuse. Busy
  is the gate rather than "is there a session": `Command::Stop` ends the pi
  process (docs/DAEMON.md §stop), and offering that beside an idle prompt box
  would be a foot-gun disguised as a cancel. `stop_command` mints the command
  and never sends it, like every other method here; the sid it names is the
  one the pane is rendering, taken from the phase rather than from a
  caller-supplied string.
- `submit_prompt`: empty prompts, starting chats, and exited sessions
  produce no intent — the phase machine is the single gate for "can this
  chat accept a prompt right now."
- `start`: the first prompt is what mints a session, so it is also the last
  moment the target must be complete — there is no launch line without a
  machine to run on and a cwd to run in.
- `session_name_from_prompt`: derives pi's `--name` from the first
  non-blank line of the first prompt, truncated to `SESSION_NAME_MAX_CHARS`.

## crates/harvester/src/shell_bootstrap.rs

Everything about starting a gpui application that is true for *any*
Harvester window, hoisted out of the app's own window setup (then
`desktop_shell.rs`, since folded into `main.rs`) when the gallery binary
needed the same start-up. Lib-side, because both binaries call it.

Contract: `run_desktop_shell(shell_window, build_root_view)` never returns
(it hands control to gpui's run loop) and does, in order: start
`gpui_platform::application()` with `HarvesterAssets`, install the widget
layer, install the app commands, then open one window whose root view the
caller builds.

- `ShellWindow { title, width_in_pixels, height_in_pixels }` is the entire
  per-binary window parameterization. Bounds are always
  `Bounds::centered`; a caller that ever needs a non-centered or
  restored-from-disk window should extend this struct rather than reach
  around the bootstrap.
- `build_root_view: FnOnce(&mut Window, &mut App) -> AnyView` returns an
  `AnyView`, not a concrete view type, deliberately: it keeps the bootstrap
  free of any generic view parameter and keeps app-only types
  (`Workbench`) invisible to the lib. The bootstrap wraps whatever comes
  back in `Root` itself, so no caller can forget to — `Root` is what makes
  modals, notifications and the dialog stack work at all (see
  `workbench/modal.rs`).
- `actions!(harvester, [Quit])` lives here now. The action's namespace stays
  `harvester` for both binaries: an action namespace is a keymap identity,
  and the gallery is the same application wearing a different root view.
- Fonts are not resolved here. Both font families (UI and monospace) are
  theme slots set by `ui/theme.rs`, applied by `install_widget_layer`; the
  old fc-match subprocess and its `OnceLock` were deleted when the theme
  gained explicit font slots.
- `install_widget_layer`: `gpui_component::init` must run before any other
  call into the library. The library also hardcodes light mode on init, so
  dark mode is set explicitly immediately after, and then
  `ui::install_graphite_theme` replaces the library's neutral dark palette
  with Harvester's own (see `ui/theme.rs`). `Theme::sync_system_appearance`
  is deliberately never called — nothing observes the desktop's OS-level
  appearance, which is what keeps Harvester pinned to dark mode even on a
  light-mode system. The gallery inherits this, which is why it is dark-only
  too.
- `install_application_commands`: installs the app-level Quit action under
  both `ctrl-q` and `cmd-q` so one build behaves natively on both Linux (ctrl)
  and macOS (cmd) without a platform-conditional binary. `gpui-component`
  itself owns the Input/Table/List/Menu context keybindings, so this function
  only needs to add the one app-level action.
- `open_shell_window` treats a failed `open_window` as fatal: there is no
  second window to fall back to, so it reports on stderr and quits rather
  than leaving a running application with nothing on screen.

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

`apply_bridge_event` is a pure router: every arm with more than one statement
delegates to a named handler (`handle_link_lost`, `handle_request_failed`,
`handle_session_attached`, `handle_session_exited`), leaving the match itself
readable as the routing table it is. Per-arm notes:
- `Connected` only repaints (the fleet already took the link state, but the
  connectivity dot still needs to move) — one statement, so it stays inline.
- `ConnectFailed` / `Disconnected` clear this machine's open guard — a dead
  link cannot finish an open.
- `RequestFailed` clears the open guard and turns the daemon's detail into a
  notification.
- `Attached` marks the session running via the catalog. This is display
  state only: the daemon's list may predate a resume the client just did,
  but disk on the VM stays authoritative regardless.
- `SessionExit` marks the row exited.
- `PiAnswered` whose `method` is `set_model` and whose `result` is an `Err`
  becomes a failure notification; the session keeps whatever model it had,
  and the answer still flows on to the chat, whose own token guard is
  responsible for dropping it. This arm is deliberately chat-independent:
  picking a model in session A and then opening session B drops A's chat, and
  a chat-owned notification would be the only signal for the failure — gone
  with the pane that asked. Matching on the echoed pi `method` is what lets
  the store recognize the one answer it cares about without re-typing the
  command the bridge carries verbatim.
- All other bridge events (`Sessions` — already handled earlier,
  `AttachmentLeft`, `HistoryReplaced`, `HistoryExtended`, `Pi`,
  `PromptFailed`, `ModelsListed`, and every other `PiAnswered`) are
  view-owned lifecycle/chat answers with no table stake, so their subscribers
  receive them unchanged. `PromptFailed` in particular is the chat's alone —
  it lands as a failure item in the very transcript that shows the prompt,
  which is a better account than a toast detached from it.

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

`host_for` hands back the fleet's `harvester_client_core::Result<Host>`
untouched. It used to demote the error to a `String` here, which was a shim
around a second error enum (`FleetError`) that no longer exists; both callers
(the chat's branch indicator, the folder picker) stringify at the point they
display, which is where the sentence is actually needed.

`is_live` matters because a prompt sent down a dead link fails silently into
stderr, which looks to the user exactly like a prompt pi itself ignored — so
callers must check liveness before sending.

`send_gated` is that check, made unskippable: it gates, notifies on refusal,
and returns whether the command left. `send` (ungated) survives for the
store's own traffic, which has already established a connection on the way in
(`open_session`). The gate has two rules, chosen by `is_pre_session_command`:
- `ListModels` and `StartSession` are *pre-session* — they are what a chat
  does before any session exists, so they may bring the machine up
  (`ensure_connection`) rather than requiring it to be up already. They queue
  on the bridge's command channel while it bootstraps.
- Everything else names a running session, so it needs a live link now.

Exactly one notification comes out of a refusal, and it carries the fleet's
own diagnosis for a machine that will not connect — the pre-session path
therefore calls `fleet.ensure_connection` directly instead of the store's
`ensure_connection`, whose job is to notify on its own behalf. Two toasts for
one refusal (the connect error, then "nothing was sent") was the shape
before, and the second one is the sentence that actually tells the user their
action did not happen.

`raise_open_guard_for` is why `begin_start` is no longer a caller's
responsibility: raising the sidebar's open guard is the store's bookkeeping
for a `StartSession` it just sent, and doing it before the gate — as the
workbench once did in two separate steps — leaves the guard raised for a
command that never left.

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

`clear_selected_session` is the store's half of the sidebar's current-session
bookkeeping: it is the one selection change attachment cannot express, and it
repaints. Reading the selection goes through `catalog()` (below), because the
sidebar is not allowed to see the chat and the highlight is therefore read
from the one place both the sidebar and the attachment path already meet.

### `catalog()`, and why there is no `fleet()`

`catalog(&self) -> &Catalog` replaced seven one-line forwarders that were each
`self.catalog.<same name>` (`repos_on`, `visible_sessions`,
`visible_session_count_on`, `session_cwd`, `opening_sid`,
`is_selected_session`). Adding a catalog read meant editing two files for no
decision; call sites now read `store.catalog().visible_sessions()`. `&Catalog`
hands out no mutator, so nothing structural is lost.

**The eleven fleet forwarders stay, deliberately — do not "finish the job".**
`Fleet::send` takes `&self`, so handing out a `&Fleet` would let any caller
send a command without going through `send_gated`, and that gate is documented
here as *made unskippable*: a prompt down a dead link fails silently into
stderr and looks to the user exactly like a prompt pi ignored. The forwarders
are the price of the gate being structural rather than a convention. The two
mutating catalog calls (`may_open_session`, `open_session`) stay on the store
for the same reason — `may_open_session` clears the open guard as a side
effect, which a `&Catalog` accessor cannot carry and a `catalog_mut()` would
buy only by also handing out workspace persistence.

### Tests

`is_pre_session_command` is the store's window-free decision and is pinned
against every `bridge::Command` variant, so adding a command forces a verdict
on whether it may bring a machine up. `gate_failure` and `raise_open_guard_for`
are *not* unit-tested and cannot be from here: both need a live `Fleet`, whose
`start` spawns the machines worker (which runs provider executables), and
`raise_open_guard_for` additionally needs a gpui `Context<FleetStore>`.

## crates/harvester/src/ui/core.rs

`core` is the one place a semantic meaning becomes a color (SPEC C11) —
everything else in `ui` should ask for a `Tone`, never a raw `Hsla`.

- `core` holds **no text-size constant of its own any more**. Body text is the
  theme's `font_size` slot (`BASE_FONT_SIZE`, see `ui/theme.rs`), which the
  window root restates as `text_size(cx.theme().font_size)` so every
  descendant inherits it, and which is simultaneously the rem unit every
  rem-based padding, gap and height in `ui` scales off. Sidebar rows no longer
  restate it either: `ListItem` inherits the ambient size once nobody
  overrides it. Anything wanting *smaller* text asks for `text_sm`, the
  library's own token — **not** `text_xs`, which at rem 14 renders 10.5px and
  falls under the gpui-component design guide's ~12px typography floor.
  `text_sm` (12.25px at rem 14) is the app's one secondary tier.
- `Tone` is what a component is being told about its *subject*, in place of
  raw color: `Muted` (secondary label), `Idle` (backgrounded — an unprobed
  machine), `Active` (what the user is doing right now), `Pending` (working,
  outcome unknown), `Live` (confirmed good — connected daemon, running
  session), `Failed` (failed and must be visible).
- `Tone::color` resolves through the library's *semantic* status slots only —
  `info` / `warning` / `success` / `danger` plus `muted_foreground` — never a
  raw palette entry. That is an invariant, not a preference: a palette entry is
  a hue, a role is a meaning, and only the meaning survives a theme swap.
  `Active` is `theme.info`: it was `theme.blue` (a palette
  color, which broke the rule that a meaning resolves to a meaning) and before
  that `theme.primary`, which in the dark theme is near-white — right for a
  filled button, wrong for the word "you". `Idle` is `Muted` faded by
  `FAINT_TEXT_OPACITY` (0.6), the one distinction the library itself has no
  token for; the same opacity constant backs `faint()` for tertiary text
  (context lines, disabled controls, disclosure chevrons), while `dim()` is
  unfaded `muted_foreground` for secondary text/subtitles — it delegates to
  `Tone::Muted.color(cx)` so "Muted means `muted_foreground`" is stated in
  exactly one arm of `Tone::color`, and a theme re-mapping has one place to
  land.
- The text hierarchy is **exactly three tiers and the set is closed**:
  `cx.theme().foreground` for body text, `dim()`/`Tone::Muted` for secondary
  labels, `faint()`/`Tone::Idle` for the tertiary run. `FAINT_TEXT_OPACITY` is
  the single number behind the third tier — it was inherited from the two call
  sites that each carried their own copy of it, and it is the lowest step that
  still clears comfortable legibility against the graphite canvas and surface
  fills. `Tone::Idle` calls `faint()` rather than re-deriving the opacity, so
  the tier has one definition and one accessor. Do not add a fourth tier:
  something that needs to recede further than faint should leave the surface or
  move behind a disclosure, not be dimmed again.
- `Tone::ALL` is the variant order, written down once. It exists for the
  gallery, which needs the tones as an indexable list twice over: a choice
  knob's labels (`tone_labels()` maps `ALL` through `Tone::label`) and the
  knob index → tone mapping (`tone_at`) with its inverse (`tone_index`, so a
  preset reads `tone_index(Tone::Live)` rather than a bare `4`). The catalog
  used to keep its own copy of the order *and* a parallel label list; both
  were this enum restated, and the label list had already drifted from the
  app's words. A const slice on the type they describe cannot. It is the one
  `ui` export whose only consumer is the catalog and not a screen, on the same
  reasoning as the dot builders: a catalog has to be able to enumerate what a
  screen only ever picks one of.
- `Tone::label` is the tone's one user-facing word ("Neutral", "Idle",
  "Running", "Pending", "Live", "Failed"). It exists because a colored dot is
  invisible to a screen reader and ambiguous to anyone reading in a hurry, so
  every dot in the app now sits in a slot that carries this string as a
  tooltip (and, in the sidebar, as the slot's `aria_label`; a tool block folds
  its status into the header's own accessible name instead, so the dot does not
  announce itself twice). Naming the word here, once, is what keeps two
  surfaces from calling the same state two different things — though note the
  tool block deliberately overrides it with its own vocabulary, since
  "Completed" is the right word for a settled call and "Neutral" is not.
  Two of the six words are not the variant's own name, on purpose. `Active`
  speaks as "Running" because that is the word the transcript and the session
  list already use for work in progress — the enum keeps the tone's name, the
  label speaks the product's. `Muted` speaks as "Neutral" so the tone with no
  lifecycle meaning cannot be confused with "Idle", which has one. The words
  are consts rather than inline literals, under the house rule that
  user-visible strings are named once.
- `Tone::is_hollow` derives the dot's *shape* from the meaning: `Idle` and
  `Pending` are hollow, everything else solid. An outline says "no verdict
  yet" — unprobed, or working with the outcome still unknown — without spending
  a color the theme doesn't have. Making this a function of the tone is what
  retired the last per-call-site shape choice; `status_dot_for_tone(tone, cx)`
  is the only way to draw a dot, and nobody picks solid-versus-hollow by hand.
  In one sentence: hollow means the dot is a placeholder for work that has not
  begun; solid means it stands for something real — running or finished, good
  or bad. That is why `Failed` is solid: a settled terminal state must not read
  as "not started".
  It is a `bool`, not a shape enum. There *was* a `Dot { Solid(Tone),
  Hollow(Tone) }` and a `Dot`-taking `status_dot`, both exported and both with
  zero callers — `Dot` was only ever produced by `Tone::dot` and consumed by
  `status_dot` one line later, and the public constructor read as an invitation
  to hand-build a shape again. Folding the match into `status_dot_for_tone` and
  naming the rule `is_hollow` leaves the same single decision point with
  nothing to construct: the two readers that need the *name* of the shape (the
  machine-presentation tests and the gallery's dot story caption) ask the tone
  directly.
- `STATUS_DOT_DIAMETER` (6px) is the dot itself, and it is one of the few
  pixel numbers in `ui` that is deliberately **off** the rem axis. A 6px disc
  is a raster boundary rather than product geometry: it is the smallest circle
  that still antialiases into a circle, and the rem steps on either side land
  it either visibly mushy or off the half-pixel grid on a 1× display. The dot
  should stay the same physical size when `BASE_FONT_SIZE` moves; the lane it
  sits in (`SIDEBAR_STATUS_LANE`, the tool block's `size_4` slot) is what
  scales instead, which is also why the diameter can change without moving any
  text.
- `disclosure_chevron` is the one expand/collapse marker for the whole app
  (section headers, tool blocks): `ChevronDown` when expanded, `ChevronRight`
  when collapsed, at `size_3p5` — the `text_sm` line, so it sits optically
  centered next to a `text_sm` label, and expressed as the library's own rem
  step rather than a pixel const so it tracks the base font size. It is an
  `Icon`, not a text glyph, because glyph chevrons sit on the text baseline
  and visibly misalign against icons elsewhere in the same row; color still
  comes from the ambient text style, so callers keep tinting it with
  `faint()` for free.
- `mono_font_family(cx)`: reads the theme's `mono_font_family` slot
  ("Geist Mono", set in `ui/theme.rs`) so every mono run in the app agrees
  with gpui-component's own widgets. No startup resolution, no fallback
  logic — the theme is the single owner of font names.
- `truncated_to_max_chars(text, max_chars)` is the app's one display
  truncator, and its contract is that the result is **at most `max_chars`
  characters with `TRUNCATION_ELLIPSIS` already counted inside that number**.
  It lives here, not beside either caller, because both `ui/prompt.rs`
  (control labels) and `ui/transcript.rs` (tool-argument summaries) need it and
  `core` is the module they both already import. It returns `Option<String>`
  rather than always allocating: `None` means "already fits", which lets the
  prompt strip hand back the caller's own `SharedString` untouched in the
  common case, and lets the transcript return the JSON string it already built.
  Three properties are load-bearing and pinned by unit tests here as well as
  through the callers:
  - The cut counts **characters, not bytes** (`chars().take(...)`), so a
    multibyte label is never split mid-codepoint.
  - The ellipsis is inside the budget (`max_chars - 1` kept), which is the
    thing the two callers used to disagree about — the prompt strip landed on
    exactly its budget, the transcript one character over, and a transcript
    test pinned that discrepancy as if it were intended. `saturating_sub`
    guards a `max_chars` of 0 even though no caller passes one.
  - The kept prefix is `trim_end`ed before the mark is appended, so a cut that
    lands on a space renders "word…" rather than "word …". That trim used to be
    a prompt-strip detail; it is a typographic rule that holds for any caller,
    so it lives in the helper and the transcript inherits it.

  What is deliberately *not* folded in here: `sid_preview`
  (`workbench/sidebar.rs`), `session_name_from_prompt` (`session_state.rs`) and
  client-core's `capped_content_text`. The first two take a prefix and never
  mark it, so an ellipsis would be wrong; the third keeps a byte-budgeted
  *tail* of streamed output. Same word, three different operations.

### Notes and empty states

These three builders were `ui/note.rs` until they folded in here: their only
non-gpui input was `core`'s own `Tone`, and `core` already owns the
tone-taking builders (`status_dot_for_tone`), so the split was a file
boundary with no rule behind it. The flat `ui::` re-export surface is
unchanged — `ui/mod.rs` exports them from `core` now, which is the path the
gallery and the screens both import through either way.

The two ways Harvester states something *about* a pane rather than in it: a
line of tone-colored text. Both existed four times as hand-rolled divs (the
folder picker's walk error, the chat's models-failed line and exit/starting
notice, the workbench's empty content pane), which is how the picker ended up
reaching for `cx.theme().danger` directly instead of `Tone::Failed` — the
same color, but a rule broken (SPEC C11). One builder each is what keeps that
from happening again.

- `inline_note` is the in-flow variant: `text_sm` and tone-colored, nothing
  more. Truncation is not baked in — the chat's notices always wrapped and a
  truncated failure detail hides its reason — so a call site that wants
  ellipsis chains `overflow_hidden().text_ellipsis()` itself, as the folder
  picker's error line does to keep the modal's height stable.
- `empty_pane_hint` is the centered variant for a pane with nothing in it: it
  fills its parent and centers on both axes. It inherits the window's body
  text size rather than shrinking a tier, because it is the only thing on
  screen in that pane and reads as content, not as an annotation — and it
  centers its text (`text_center`) for the same reason, since a wrapped hint
  ragged against an invisible left edge reads as a stray label. It is a
  `v_flex` with `gap_4` and `p_8` rather than a bare centered div so a caller
  may `.child(...)` an action underneath the sentence. Both steps are chosen as
  relationships rather than as pixel counts: `gap_4` is the "separate groups
  inside one section" step (the sentence and the action are two things, not
  one), and `p_8` is the major-region step an empty state wants — the pane has
  nothing else in it, and crowding its one sentence against the edges is how an
  empty state reads as broken rather than as waiting. The signature did not
  change when the action slot was added, so every existing caller keeps
  rendering exactly what it rendered before.
- `empty_pane_action` is that action: a small primary `Button`, one per empty
  state. An empty pane that states a problem without offering the one way out
  of it is a dead end, and the alternative — every empty pane hand-rolling its
  own button — is how the tone rule got broken the last time.
- All three take `impl Into<SharedString>` so a caller may hand over an owned
  `format!` result without an extra clone; the two note builders take a `Tone`,
  never a color.
- The statusbar's link (`workbench.rs`) is deliberately *not* one of these:
  its size already comes from `statusbar_frame`, and folding it in would add
  ellipsis behavior to a band that has never needed it.

## crates/harvester/src/ui/dialog.rs

Every piece of modal chrome Harvester draws: dialog geometry, titles, the
cancel/primary button pattern, the two dialog input boxes, and the three
`open_*` builders that assemble them. Deliberately dumb — it knows nothing
about Pi, obligations, the active modal, or machines, and takes plain strings
plus callbacks that return "did this answer stick?". The three callers
(`dialogs.rs`, `workbench/modal.rs`, `workbench/picker.rs`) all
build their chrome from here, so a dialog cannot look different depending on
which flow raised it. Every public item in the module has at least one caller
outside it; nothing here is built speculatively for a dialog kind that does not
exist yet.

- The modal widths are three named rem measures, not one pixel constant:
  `CONFIRMATION_DIALOG_WIDTH_IN_REMS` (30), `SELECT_DIALOG_WIDTH_IN_REMS` (36),
  `TEXT_DIALOG_WIDTH_IN_REMS` (44). There used to be a single `DIALOG_WIDTH`
  (520px) on the theory that one modal geometry is a feature. It isn't: the
  three dialog kinds hold visibly different things, and one width sized for
  the widest of them left a two-button confirmation floating in a half-empty
  box while still not being generous to a `set_editor_text` body. The ladder
  is by *content*: a confirmation is one sentence and two buttons; a `select`
  is a scannable list of option labels; a text entry is something the user has
  to compose in. They are `pub` because the gallery's dialog story renders the
  same chrome inline at the same widths — a story that guesses the width is
  not showing you the dialog. Being rems, they also grow with the base font
  size, which a pixel constant did not. `dialog_width` is the one place they
  are resolved against `window.rem_size()`, and it runs inside the dialog
  builder closure, which the library re-runs every render — so the widths track
  a base-font change live instead of freezing at whatever the size was when the
  dialog opened. What each rung is sized *for*: 30 rem is about a
  sixty-character measure, which is one paragraph of consequence under a title
  and no more (the forget-machine confirmation is the worst real case and fits);
  36 rem is for option strings that are branch names, paths and command lines —
  more room than a sentence needs, still narrow enough that the list scans as a
  column; 44 rem is the widest because the text entry is the only dialog where
  the user *authors* something rather than picking it.
- `EDITOR_ROWS` (6, 16): visible row range for a multi-line input before it
  scrolls. A `set_editor_text`-style body is usually a commit message, so a
  handful of lines is the right initial shape. It stays a bare pair of integers
  rather than becoming a rem measure because it is a row *count* handed to
  `auto_grow`, not geometry.
- `title_or_named_condition(title, named_condition)`: the title shown when a
  caller has no title to give (Pi omits its own on some dialogs). It replaced a
  single `UNTITLED` const ("The agent is asking") because one generic fallback
  answers the wrong question — the user knows the agent is asking; what they
  need is what it is asking *for*. Each call site now names its own condition
  ("Choose an option", "Enter a value", "Write a response", "The agent needs a
  yes or no"), so a titleless dialog still says what kind of answer it wants. A
  whitespace-only title counts as absent (the title is trimmed first), which is
  what stops a stray space from defeating the fallback.
- Button-id consts: element ids are identity within one rendered tree, and
  Harvester renders at most one modal at a time (see `workbench/modal.rs`),
  so the two
  confirmation call sites deliberately share one pair of ids rather than
  threading per-flow ids through the builder. `select` and `text` carry their
  own pairs because they are different dialogs, not two callers of the same one.
- Commit-button wording is per dialog kind, not a generic "OK": `Reply` for a
  text entry, `Confirm` for a question, and `Cancel` everywhere for the
  refusal. `CANCEL_LABEL` is `pub` and is the app's *only* declaration of that
  word: `dialogs.rs`, `workbench/modal.rs` and `workbench/picker.rs` import it
  rather than each spelling it out. It belongs here because this is the shared
  modal chrome — every dialog in the app is assembled from this module, so the
  refusal word is a property of the chrome and not of the flow that raised it. A `select` has no commit button at all — see below. The button should say what it is about to do, so
  a user who reads nothing but the buttons still knows what they are answering.
  `Reply` specifically replaced `Send`: `Send` is already the chat composer's
  verb for handing a turn to the agent, and one product term should name one
  object — answering a blocking question is not sending a turn. `cancel_button`
  is the plain default `Button` variant everywhere, which is the library's own
  convention for the leave-without-committing action; the select dialog used to
  outline its cancel and was the odd one out.
- `ActionAccent`: `Primary` for the ordinary affirmative, `Danger` for an
  action the user cannot undo from the dialog (forgetting a machine). The
  accent is a parameter rather than a per-call `.primary()`/`.danger()` so
  "which button is the scary one" is stated in the caller's own vocabulary.
  `Danger` has exactly one caller and that is not an accident: a Pi `confirm`
  payload carries only a title and a message, so destructiveness cannot be
  *inferred* from it, and every Pi confirmation is therefore `Primary`. Do not
  "fix" that by sniffing the message for scary words — the only flow that knows
  it is about to do something irreversible is the flow that initiated it.
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
- `open_select` renders a `List` of option rows over a cancel-only footer.
  It used to render one full-width `outline` button per
  option, with no OK button at all, on the reasoning that picking an option
  *is* confirming. Two things were wrong with that. A stack of buttons has no
  selection, so there is nothing to arrow through and nothing for Enter to
  mean — `on_ok` had to be hard-wired to `false` precisely because the dialog
  had no notion of a current choice — and a list of five branch names rendered
  as five buttons reads as five actions rather than one question. A `ListState`
  gives selection, keyboard navigation and Enter-to-confirm for free. It also
  ends the commit button: `List` commits on a *single click* — `on_click` sets
  the index and calls `on_action_confirm` in the same handler — and Enter does
  the same for the keyboard. Nobody can ever observe a selected-but-uncommitted
  row, so a footer `Choose` button was unreachable by mouse and redundant by
  keyboard. It was deleted along with `commit_chosen_option`, and the select
  dialog's footer is `dismiss_only_footer`: a way out and nothing else, because
  the list itself is the action. This is the one dialog whose footer is not a
  cancel/commit pair, and the gallery's select story matches it. Beyond
  selection and Enter, the list brings a selected-row style, `up`/`down`
  traversal, `escape` to deselect, list-item roles carrying position-in-set,
  and a virtualized scroller for Pi's unbounded option counts — all things the
  button stack would have had to grow one at a time.
- **Nothing is selected when a select dialog opens, deliberately.** A default
  selection would let a stray Enter send option one to a pi process that is
  blocked waiting for an answer, which is the worst failure this dialog has.
  With no selection the commit path is a no-op that leaves the dialog open, and
  that is enforced in the commit logic rather than by a disabled button, since
  the dialog's action wrapper dispatches whether or not the button inside it is
  disabled.
- Escape has to reach Pi through a focused `List`, and it does so by
  propagation rather than by luck: the list's own cancel handler propagates
  before doing anything, and gpui walks every binding that matches a keystroke
  while propagation is alive — so `escape` first clears the list's selection
  and then resolves to the dialog's cancel, which answers Pi and closes.
  **Upgrade watch:** if a future `gpui-component` bump makes that handler stop
  propagating, the select dialog silently swallows Escape and leaves a Pi
  dialog unanswered, which is the one thing CLAUDE.md forbids. Re-check it on
  every library bump.
- `option_row` and `options_height` are `pub` for the gallery: the story
  renders the same rows at the same height inline on the canvas, and would
  otherwise be a lookalike. `options_height` clamps to
  `SELECT_VISIBLE_OPTION_ROWS` (8) rows of `SELECT_OPTION_ROW_HEIGHT_IN_REMS`
  (2) each — a short list gets exactly its own height (no dead space under
  three options) and a long one scrolls inside a fixed box instead of growing
  the modal past the window. The height is *definite* rather than a `max_h`
  with inferred sizing because a virtual list needs a resolved height to
  compute its visible range and the dialog body is auto-height; measuring under
  indeterminate available space is the fragile path. The 2 rem row is the
  library list item's own padding plus one line at the theme's line height, so
  if that padding changes upstream the rows clip slightly and the list starts
  scrolling a little early — cosmetic drift, not a correctness bug. Eight
  visible rows keeps even the tallest select at 16 rem of list, which sits
  inside a normal window above the footer.
- `OptionListDelegate` holds the options, the selected `IndexPath` and the
  caller's `chosen` callback together, because the library's confirm path and
  the footer button both need all three and neither owns the dialog. Note the
  delegate renders every row with `selected: false` and lets `ListState` draw
  the selection itself — passing our own flag would fight the library's.
- `open_confirmation` omits the message element entirely when the message is
  empty, rather than rendering a blank child. This is not defensive coding: it
  is the rendering half of the "condition and consequence" rule in
  `dialogs.rs`, where a Pi `confirm` whose message merely repeats its title
  collapses to a title alone. An empty `div` there would leave a gap the user
  reads as a missing sentence.
- `open_text`: one shape for both single- and multi-line entry — same dialog,
  different input box. After building, it explicitly focuses the input because
  `open_dialog` only focuses the dialog frame itself, not anything inside it,
  and the box this whole dialog exists to have filled in should not require an
  extra click before it accepts a keystroke. The typed value is read at
  submit time and handed to the caller as a `String`.
- Focus and dismissal, stated once for all three dialogs. Returning focus to
  whatever opened the modal is the library's job — `open_dialog` records the
  previously focused handle and `close_dialog` restores it — so nothing here
  duplicates it. What each dialog does on *open* differs by what it has to
  offer: a text entry focuses its input, a select focuses its list so
  `up`/`down` and Enter work without a pointer, and a confirmation leaves focus
  on the frame handle the library created. That last one is deliberate: there
  is no field to put a caret in, the frame sits inside the dialog's focus trap
  so Tab reaches cancel and then the commit, and Enter/Escape resolve on the
  dialog's own key context — focusing the primary button instead would only
  give Enter a second path to the same call. In all three, Escape ends at the
  dialog's cancel callback, which answers Pi. There is no dismissal path that
  closes a Pi dialog without answering it.

Tests: the module's two pure functions carry an inline `#[cfg(test)] mod tests`
(house rule 11). `title_or_named_condition` is pinned on all three inputs a
caller can hand it — a real title, a whitespace-only one, and an empty one —
because the trim is what stops a stray space from defeating the fallback and it
is not visible from the call sites. `options_height` is pinned at its two
boundaries and past them: an empty list still reserves one row (the clamp's
lower bound, so a `List` never gets a zero height to compute a visible range
against), a short list gets exactly its own height, and anything at or beyond
`SELECT_VISIBLE_OPTION_ROWS` gets the same capped height and scrolls inside it.
Nothing here touches a `Window`, which is why these are unit tests and not a
gallery walk.

## crates/harvester/src/ui/mod.rs

`ui` is Harvester's visual architecture, grouped by responsibility — not a
palette or a parallel widget library. Components are built with
`gpui-component`, and every color must be resolved from `cx.theme()`; the
`ui::core` submodule exports semantic *meanings* (see `Tone` below) and
reusable non-color metrics only, never raw colors of its own (SPEC C11). The
`pub use core::{...}` / `prompt::{...}` / `shell::{...}`
re-exports exist to keep a stable `ui::…` import path for screen-level callers
while the implementation stays split across cohesive modules (`core`,
`dialog`, `prompt`, `shell`, `theme`, `transcript`) rather than one broad file.
That list is shorter than it was: `sidebar` folded into `shell` (one alignment
spine, one file) and `tool_block` into `transcript` (its only consumer). The
flat `ui::` surface did not move a name in either merge — `sidebar_row`,
`sidebar_stacked_row`, `sidebar_empty_note` and `SIDEBAR_ROW_GUTTER` are
re-exported from `shell` now and are still spelled `ui::sidebar_row` and so on
at every call site, because the export list is a contract with the screens and
the gallery, not a mirror of the file layout. Only names a *screen* calls are re-exported: a
builder used solely by other `ui` modules (`section_header`) or by its own
module (`prompt_box_input`, assembled into `prompt_strip` and never drawn
alone) is reached through `super::core` / kept private to its module instead,
so the flat `ui::` surface stays a list of what the screens actually draw
with. Two of the re-exports are strings rather than builders — `ADD_REPO_ROW`
and `DEFAULT_MODEL_ROW`, the two dropdown rows that are *chrome* rather than
data (see `ui/prompt.rs`) — and they are exported for the same reason a
builder is: both the chat screen and the gallery render them, and a copy in
either place is drift waiting to happen. `status_dot_for_tone` is the exception
the gallery bought: no screen calls it directly (screens reach a dot through
`sidebar_stacked_row` or a tool block), but a component catalog has to be able
to render one on its own, and the gallery is a screen in every sense that
matters here. It is now the *only* dot export — the `Dot`-taking `status_dot`
and the `Dot` type itself had no callers at all and are gone (see
`ui/core.rs`). `empty_pane_action` is exported on the same footing — it is the
trailing button slot `empty_pane_hint` grew, catalogued before the first
screen adopts it. `create_prompt_input` is the third of the widget factories
(`ui::dialog`'s two inputs are the others) and is exported for the reason
those are: the chat screen and the gallery must build the same
`InputState`, and the export is what makes that a fact rather than a
convention.

Two of the re-exports are not builders at all and are worth naming for that
reason. `KeyboardFocusRing` (from `shell`) is a trait with a blanket impl, so
it has to be imported by name at the call site to be usable —
`use crate::ui::{self, KeyboardFocusRing as _}` in `workbench/sidebar.rs` is
the shape. `SIDEBAR_ROW_GUTTER` (from `shell`) is a metric rather than a
component: the sidebar's own screen needs it to line its New-session button up
with the rows below, which is the one case where a caller genuinely has to
know a number `ui` owns.
`dialog` is the one submodule declared `pub mod` instead of being re-exported
flat: its names are generic on purpose (`open_text`, `button_row`,
`action_button`), and they only read correctly qualified as
`ui::dialog::open_text`.

`ui` lives on the lib side of the crate (see the `lib.rs` section) so the
`harvester` binary and the out-of-workspace `harvester-gallery` crate share
one copy. That makes this flat re-export surface the gallery's catalog as well
as the app's import path: anything a story renders must be exported here — and
nothing else should be, which is why deleting the three narrow prompt stories
also un-exported `prompt_box`, `prompt_box_control`, `prompt_box_controls`,
`prompt_box_dropdown` and `prompt_box_send`. They are assembled into
`prompt_strip` and no longer drawn alone by anyone, so they went back to being
private to `prompt.rs` alongside `prompt_box_input`. `prompt_strip` and its value
types (`PromptStrip`, `PromptStripControl`, `PromptStripNotice`) are the first
composite — not a primitive but a whole assembled strip — and the rule that
only names a *screen* calls get re-exported still holds, since the chat screen
calls exactly that one. The three picker *actions* (`PickModel`,
`PickMachine`, `PickRepo`) are exported alongside them because a gpui action is
half a contract: `ui` dispatches them and the screen listens for them, so both
halves need the same type (see `ui/prompt.rs`).

## crates/harvester/src/ui/prompt.rs

The agent prompt card and its in-place model/machine controls.

The governing decision for this whole module: **the pickers and the send
button are gpui-component widgets, not hand-rolled divs.** Every one of them
used to be a styled `div` with its own hover fill, press fill, disabled
branch, chevron size and max width — eight sizing consts, a hand-rolled
popover with its own row rendering and check-mark gutter, and a `PlainTrigger`
newtype whose entire purpose was to no-op `Selectable` so a plain `div` could
satisfy `Popover::trigger`'s bound. All of that reimplemented chrome the
library already ships, and reimplemented it subtly wrong: no keyboard
operation, no tooltips, no menu navigation. `Button`, `PopupMenu` and `Tag`
deleted the lot and brought arrow-key/Enter/Escape navigation with them. What
is left in this file is only what the library does not know: which icon a
control wears, what its rows say, and what a pick *means*.

- Consequently there are no sizing consts left. The controls are
  `Button::ghost().small()` and cap at `max_w_56` (14rem — a long model or
  repo name still cannot push the rest of the row off the strip); the send
  button is `Button::primary().small().rounded_full()`. The pickers are
  `.small()` and not `.xsmall()` specifically so they match the send button's
  height: a strip whose pickers sit a few pixels shorter than the button
  beside them reads as three loose controls rather than one row.
- The `max_w_56` cap is not cosmetic: `Button` lays its label out in a
  `flex_none` child with no ellipsis lane of its own, so without a cap an
  unusually long model name pushes the send button out of the box — a clipped
  label is a cosmetic defect, an unreachable send button is a functional one.
  The label is therefore cut before it is handed over:
  `truncated_control_label` holds the label to `CONTROL_LABEL_MAX_CHARS` (24)
  *characters* including the ellipsis. It owns the budget and nothing else —
  the cut itself is `ui::core::truncated_to_max_chars`, shared with the
  transcript's argument summaries, which is where the character-boundary rule
  and the trailing-space trim now live. What stays here is the `SharedString`
  round trip: the helper answers `None` when the text already fits, so the
  common case hands back the caller's own `SharedString` without allocating a
  second one. All three real label sources (a folder name, a fleet machine
  label, a model name) are short in practice, so the budget should rarely bite;
  the unit tests pin the boundary, the trim and the multibyte case through this
  function, and `ui/core.rs` pins the same properties on the helper.
- `prompt_box` rounds through `theme.radius_lg` and takes the full theme
  `ring` on focus, plain `border` at rest. The old half-opacity resting border
  plus `shadow_md` are gone: with the graphite ladder the input well already
  reads as raised against the canvas, and a shadow under a control that also
  fades its own border was two devices doing one job. The half-opacity border
  was also a pseudo-role invented at the call site: the resting state is the
  plain `border` role now, for the same reason `faint()` became one named
  accessor instead of an inline `0.6` in two files.
- `ADD_REPO_ROW` ("Add new…") and `DEFAULT_MODEL_ROW` ("Default (pi chooses)")
  live here, not in `models.rs` where they were written,
  because two renderers draw them: the chat screen and the gallery's prompt
  strip story. They are the only dropdown rows that are chrome rather than
  data — every other row is a model, a machine or a repository the app was
  told about — and while the gallery kept its own copies the catalog drifted
  inside the very pass that shipped it (it offered "Add a folder…" and an
  "Add a model…" row the app has never had). A shared const makes the drift
  unrepresentable. The wording rules that earned them are recorded under
  `models.rs`.
- `create_prompt_input(window, cx)` is the one place the composer's
  `InputState` is built, and `PROMPT_INPUT_PLACEHOLDER`
  (`"Message pi — Enter to send"`, which names the send keystroke because the
  send button's tooltip is the only other place that does) and
  `PROMPT_INPUT_ROWS` (1–8) are private to it. Multi-line with `auto_grow`,
  but still `submit_on_enter(true)`: that flag defaults to false, and without
  it Enter would only insert a newline — Shift-Enter remains the newline. It
  is a factory in `ui/` for the same reason `ui/dialog.rs`'s
  `create_single_line_input`/`create_multi_line_input` are: `chat.rs` and the
  gallery's `StoryRuntime` both need this exact widget, and while they built
  it separately the gallery named the four values as consts on its side and
  the app hardcoded them — a preview surface that reconstructs the component
  it previews is the drift the catalog exists to prevent.
- `prompt_box_input(input)` is the input well's padding (`px_3`, `pt_3`,
  `pb_2`) and nothing else. It is private to this module: `prompt_strip_box`
  is its only caller and no screen or story draws a bare input well, so it
  fails the `ui/mod.rs` re-export rule (only names a *screen* calls are
  published). It stays a named builder rather than an inline `div()` because
  the controls row's inset is solved *against* this one — the two numbers are
  a pair, and the next reader has to be able to see both.
- `prompt_box_controls()`: `pl_2` + `pr_3`, `gap_2`. The asymmetry is
  deliberate — the button's own internal padding supplies the rest of the left
  inset, while the send button carries none, so `pr_3` is what lands it on the
  same margin as the input above. The rule the left inset is solved against is
  optical: the controls row is inset one step less than the input row so that
  the first picker's *content* starts on the same x as the input's first
  character, which means the row's own padding plus the library's padding for
  the button size in use has to add up to the input's `px_3`. It is a
  deliberate off-scale correction, documented here because that is the price of
  making one.
- `DropdownRow` is the one shape a picker's rows are described in, and it is
  now purely a description: `label`, `selected` (the choice in effect — the
  menu draws its own check mark) and `divider_above` (a separator meaning what
  follows belongs to a different group than what precedes it, e.g. the repo
  picker's "Add new…" is an action, not one more repository).
- **Picks travel as gpui actions, not callbacks.** `PickModel`, `PickMachine`
  and `PickRepo` each carry the index into that control's own `rows`, and
  `prompt_box_dropdown` takes an `action_for_index` closure that mints one.
  The reason is `PopupMenu`: a library menu item is bound to an action, not to
  an arbitrary `Fn`, because that is what lets the same item be reached by
  keyboard, echoed in a tooltip and dispatched through the focus tree. The
  action's `action_context` is the prompt input's own `FocusHandle`, so a
  dispatch lands on the chat pane that owns the input rather than on whatever
  happened to be focused when the menu opened. `chat.rs` listens with three
  `on_action` handlers (see its section). This is also what deleted
  `PromptStripHandlers`: three of its four callbacks became actions, and one
  callback (`send`) does not justify a struct.
  Mechanically, `PopupMenu` focuses `action_context` and *then* dispatches, and
  gpui walks the dispatch tree upward from whatever is focused — which buys two
  things at once: the action provably passes through the chat pane, and focus
  is back in the prompt input when the menu closes, which the hand-rolled
  popover never managed. `action_context` is an `Option` because the gallery's
  standalone dropdown story has no input to restore focus to; with `None` the
  action still dispatches from the menu's own handle (a deferred descendant of
  whatever rendered the trigger), it simply restores nothing.
  The three actions are declared `no_json`: they are never addressed by name
  from a keymap and never deserialized, so they have no business carrying a
  `Deserialize`/`JsonSchema` derive to satisfy a code path that would only ever
  error.
- `send` stayed a plain closure rather than becoming a fourth action. It is a
  direct control — click, send — it carries no index, and routing a click
  through `dispatch_action` would be indirection bought for nothing.
- `prompt_box_dropdown` anchors `BottomLeft` and opens upward deliberately —
  the prompt box sits at the bottom of the pane, so a downward-opening list
  would run past the window edge and snap back over its own trigger. The menu
  is `scrollable(true)` so a long model list scrolls inside the window instead
  of overflowing it; the library owns the popover's padding, width, check-mark
  gutter and dismissal, all of which this module used to specify by hand. No
  max height is set: `PopupMenu` defaults to half the window height, which is a
  better rule than the pixel literal it replaced because it scales with the
  window rather than with nothing. (Submenus are unsupported while `scrollable`
  is on — the pickers have none.) The trigger also *looks* open now for free:
  the library selects a popover's trigger for as long as the menu is up, and a
  real `Button` renders that selected state, which is precisely what the
  deleted `PlainTrigger` newtype existed to no-op away.
- `prompt_box_control`'s icon is given neither an explicit size nor color:
  `Icon` falls back to the ambient text style, so the glyph and its label
  always match without a second set of tokens. `dropdown_caret(true)` is the
  library's own trailing caret, which is what keeps truncation ellipsizing the
  label without ever eating the affordance.
- `prompt_box_send` carries a `tooltip_with_action` naming the `Enter`
  keystroke in the `Input` key context (`SEND_KEYSTROKE` / `INPUT_KEY_CONTEXT`)
  — the send button and Enter are the same action, and the tooltip is where
  that is said once. The keystroke is not an invented advertisement: the
  library binds `enter` to that exact action inside the `Input` key context,
  and the prompt's `submit_on_enter` is what turns it into a submit, so the
  tooltip names a binding that genuinely exists. `.rounded_full()` is chained
  through `Styled` rather than expressed as a huge radius, because `Button`
  refines the caller's own style over its per-corner rounding, so the chained
  call wins outright. Its `enabled` flag remains a look, not a guard: a chat
  with no machine or repository selected refuses the prompt inside its own
  state machine; the disabled rendering is just how the strip communicates
  that *before* the user starts typing.
- `branch_indicator` is a `Tag::secondary().small()`, not a disabled picker.
  It never opens anything, and rendering it as a dead dropdown trigger made it
  look like a control the user had failed to earn. A tag reads as a label
  about the session, which is all it ever was. `Tag` is this library's chip —
  `gpui_component::Badge` is a notification overlay dot, not the same thing —
  and the `secondary` variant is chosen because a branch name carries no
  success, warning or danger meaning; a strip with a row of colored badges in
  it is a hierarchy decision nobody made. There is exactly one badge on the
  strip and it stays quiet. Its icon/label `gap_1` is the "parts of one
  control" step, not the "separate controls" one. It is still the only place in
  `ui` that names an asset path (`assets::GIT_BRANCH_ICON_PATH`, at `size_3`),
  and still the reason `assets` and `ui` had to land on the same side of the
  lib/bin split.

### `PromptStrip` — the whole bottom row as one value

The `prompt_box*` builders above are the pieces; `prompt_strip` is the
assembled bottom row, and it is the only thing `chat.rs` calls. The point of
the split is that the strip is now renderable from a plain value with no
`Chat`, no store and no session behind it — which is what lets the gallery
render it in every state, and what makes "what does this strip look like
when models failed to load" a question with one answer instead of three.

- `PromptStripControl { label, enabled, rows }` is one shape for all three
  dropdown controls. `enabled` is the caller's gating decision (see
  `chat.rs`); `rows` are already built (`DropdownRow`), never a closure.
- `PromptStrip` fields: `notice`, `model`, `machine`, `repo`, `branch`
  (`None` when there is nothing to report), `can_send`, `can_stop`,
  `focused`. Nothing here is an `Entity`, a `Window` or a callback — the one
  live handle `prompt_strip` takes is the `&Entity<InputState>` for the text
  box, which gpui owns and cannot be reduced to a value.
- `can_stop` is a *presence* flag, not an enabled flag: false renders no stop
  button at all, where `can_send` renders a disabled one. The difference is
  what each control means when it cannot act. Send is the strip's permanent
  commitment and has to keep its place in the row whether or not it is
  available; stop only exists while there is a turn to interrupt, and a
  permanently greyed Stop next to an idle prompt box would advertise an
  action the session cannot offer. It also keeps the row from growing a hole:
  the button appears at the right of the strip, immediately left of send, and
  leaves with the turn.
- `PromptStripNotice` is the short-circuit: `SessionStarting` (the session is
  being created, there is nothing to type into yet) and
  `SessionExited(detail)` (pi is gone; the detail is the state machine's own
  wording). `Some(notice)` replaces the entire prompt box with one
  `inline_note(Tone::Idle, …)`. Modeling this as an enum inside the strip —
  rather than leaving the caller to choose between two render calls — is what
  makes the bottom row a single render path, so a caller cannot draw a notice
  and a prompt box at the same time or drift on the tone. `SESSION_STARTING_NOTICE`
  ("Starting session…") is a const here for the same reason: the wording is
  presentation, and the state machine has no business owning it. Its ellipsis
  is doing real work and stays until something replaces it: the honest control
  for ongoing work is an indeterminate progress indicator, but `inline_note`
  has no leading slot to put one in, and dropping the dots without adding a
  spinner would delete the "still happening" reading and leave a flat
  statement.
- `prompt_strip(strip, send, stop, input, cx)` takes two callbacks, both
  `Fn(&mut Window, &mut App)`: the click event carries nothing a caller needs,
  and dropping it keeps `gpui::ClickEvent` out of the surface. They are the
  strip's two *direct* controls — a command with no index, per DESIGN.md
  §Interaction — while the three picks are gpui actions carrying a row index
  (above). `stop` is passed even when `can_stop` is false; an `Option` of a
  closure would buy a generic parameter's worth of noise to skip building a
  button the strip already knows it will not render.
- `prompt_box_stop` is `outline` where the pickers are `ghost` and send is
  `primary`: DESIGN.md reserves `outline` for a command in a dense control
  surface, which is exactly what an interrupt beside three pickers is, and
  reserves `danger` for what cannot be undone — stopping a session is not
  that, since the session file survives and the sidebar resumes it. It is the
  one strip control that carries both an icon and a label (`STOP_LABEL`):
  the label is what gives it an accessible name, which the icon-only send
  button still lacks (DESIGN.md §Known gaps), and "Stop" is short enough that
  the row is not disturbed by it. `STOP_TOOLTIP` says where the session goes,
  because "Stop" alone reads like "cancel this turn" and the command ends
  pi.
- Element-id consts (`MODEL_CONTROL_ID` = `"chat-model"`, and the
  machine/repo/send/stop counterparts) kept their original `chat-*` names from when
  this render code lived in `chat.rs`: element ids are identity within one
  rendered tree, so the names cost nothing, and keeping them byte-identical is
  what made the extraction verifiable as a pure refactor. The per-dropdown ids
  are gone with the hand-rolled popovers — the library's menu hangs off the
  trigger's own id. A second strip in the same window would need ids
  parameterized; nothing has needed that yet.
- `dropdown_control` is the shared tail for all three controls: a control
  that is disabled *or* has no rows renders as the bare button rather than an
  openable-but-empty menu. One function, so "disabled or empty means no
  popover" cannot be decided three different ways.
- `prompt_strip_box`'s three `dropdown_control` calls stay three explicit
  calls, decided deliberately against a `(id, icon, action-constructor)`
  const table iterated into `.children(...)`. The calls differ in exactly the
  three things a reader searches for — the element id, the icon and which
  `Pick*` action a control mints — and a table would move all three behind an
  index while saving ~20 lines of argument lists. Grepping `PickModel` or
  `MODEL_CONTROL_ID` must land on the line that wires it; three fixed
  controls do not earn the indirection. Revisit only if the strip grows
  enough pickers that the table reads as a table.

## crates/harvester/src/ui/shell.rs

Window chrome that is not the prompt, the transcript, or a modal: the sidebar's
row vocabulary, section headers, the chat pane's header, the statusbar frame,
and the app's one keyboard focus ring. The sidebar rows used to live in a
separate `ui/sidebar.rs` and were merged in here, because the split was not a
seam: the row geometry and the section header *are* one alignment spine, and
keeping them apart forced `section_header` to import a `pub(super)` padding
constant back out of the sidebar module — a geometry leak that existed only
because the spine was written down in two files. Every machine and session row
in the app is built from the three row builders, so the two lists cannot drift
apart visually, and the headers above them cannot drift away from the rows
either. Only presentation lives here; which rows exist, what they say, and what
a click means belong to `workbench/sidebar.rs`.

The chrome rhythm used to be deliberately tight (a fixed 24px statusbar,
`pt_2`/`pb_1` section headers, `py_1p5` chat header) on the theory that chrome
must never compete with content for height. It now sits one tier up — the
**standard** tier rather than the compact one — because at rem 14 the compact
tier stopped being tight and started being cramped: a 24px band around a
12.25px line leaves under 6px of air on each side, which reads as a clipped
strip rather than a deliberate one. Density here now comes from the rem scale,
not from squeezing individual bands, and every number below is a rem step so it
tracks `BASE_FONT_SIZE` instead of drifting from it — which is also the deeper
fix, since a pixel band under a growing base size gets tighter every time the
type does.

Wording convention, app-wide: user-visible text is sentence case
("Loading models…", "No sessions yet", "Session exited…", "Machines",
"Sessions", "Controls"). Section headers used to `to_uppercase()` their title
and set `SEMIBOLD`; both are gone. Small-caps at 12.25px is the least legible
thing the app could render, and the weight was doing the job the `faint`/`dim`/
foreground brightness ladder already does. The decisive argument was
mechanical, not aesthetic: caps are only legible as a compact shape when they
come with measured tracking, and this gpui revision exposes no letter-spacing
API at all, so shipping caps would have been shipping half of the recipe at the
smallest size the app draws. Level is carried by size and brightness instead.
Restoring caps later is a change to the caller's literals — `ui/shell.rs`
uppercases nothing and would not need to change. Lowercase is now reserved for
identifiers (tool names, branch names, model ids) and the gallery's knob and
preset vocabulary, which mirrors code — the gallery's *group* names are
sentence case like every other heading.

- `KeyboardFocusRing` is a trait with a blanket impl over every
  `InteractiveElement`, so any element can chain `.keyboard_focus_ring(cx)`.
  It draws the theme `ring` as a `FOCUS_RING_SPREAD` (1.5px) box-shadow at
  `FOCUS_RING_OFFSET` (0), through `focus_visible` — *visible* focus, so a
  mouse click does not leave a ring behind while a Tab press does. It is a
  shadow rather than a border because a border changes an element's layout box
  and would shift a row by a pixel the moment it gained focus; a shadow draws
  outside the box and moves nothing. One trait, so the sidebar, the section
  headers and the gallery cannot each invent their own ring. `FOCUS_RING_SPREAD`
  and `FOCUS_RING_OFFSET` are the two `px()` literals in this module that are
  meant to stay pixels: a focus ring is a physical hairline, in the same class
  as a 1px border, and it must stay one crisp ring at any rem size. 1.5px is
  also what gpui-component uses for its own ring, so ours differs from the
  library's in exactly one respect — the library draws the ring color at 0.2
  alpha and we draw it at full strength, because a keyboard focus ring is the
  one thing on screen that must not be subtle.
- Keyboard activation is nowhere written by hand, and that is a gpui fact worth
  knowing before anyone adds a key handler: `tab_index` sets both `focusable`
  and `tab_stop`, gpui creates the `FocusHandle` in element state for a
  focusable element that has no tracked one (so no view has to hold a handle
  and no `&Window` has to be threaded into a row builder), and gpui synthesizes
  a keyboard `ClickEvent` from Enter/Space on a focused element that has click
  listeners — so an existing `on_click` is already the keyboard behavior. Tab
  order resolves by `(path, insertion index)`, meaning equal indices fall back
  to paint order; that is why every tab stop in the app uses index 0 and still
  walks in visual order.
- **The alignment spine.** The row metrics are rems, and they are the one place
  the sidebar's leading edges are written down. `SIDEBAR_ROW_GUTTER`
  (`rems(0.25)`) is the row's outer margin — what insets the hover/selection
  fill from the sidebar edge — and `SIDEBAR_ROW_PADDING` (`rems(0.75)`) its
  inner padding. These were one shared 6px number on the theory that margin and
  padding should match; they no longer do, because the margin's job (keep the
  fill off the edge) needs far less room than the padding's job (keep the label
  off the fill's edge), and at rem 14 the shared value was too much for one and
  too little for the other. `SIDEBAR_CONTENT_INSET` is their sum — the lane the
  row *text* starts in — and is what non-row content (`sidebar_empty_note`)
  aligns to so a note and a row title share one left edge. The point of all of
  that is a single spine: four siblings describe the same level of the sidebar
  — the New-session button, the section headers, the rows and the empty note —
  and each used to invent its own leading edge from its own literal. They now
  all resolve from this one pair, so changing the gutter moves every one of
  them together instead of starting a hunt. `SIDEBAR_CONTENT_INSET` is
  *computed* (`rems(GUTTER.0 + PADDING.0)`) rather than typed a second time;
  `Rems` is a tuple struct and `rems` is a `const fn`, so the sum costs nothing
  at runtime and cannot drift. `SIDEBAR_ROW_GUTTER` is the one metric that
  leaves this module — `workbench/sidebar.rs` reads it so the New-session
  button's own padding completes the same inset — and it is exported for that
  single caller; the padding and the inset stay private now that
  `section_header` sits in the same file as the rows it aligns to, which is
  what the merge bought.
- Row padding is `py_2` (was `py_1`), matching the standard-tier step the rest
  of the chrome took: a session row carries two lines of `text_sm` and at
  `py_1` the stacked rows ran together into one grey block.
- `SIDEBAR_STATUS_LANE` (`rems(1.)`) is a fixed lane the status dot sits in,
  and the same lane the `detail` second line is indented by. It replaced a
  derived `STACKED_ROW_DETAIL_INDENT` that had to be recomputed by hand every
  time the dot diameter or the dot/title gap changed (it was "6px + gap_2").
  Declaring the lane instead of deriving it means the two can never disagree,
  and it is why the detail line hangs under the *title* rather than under the
  dot. The dot sits at the lane's leading edge and the row carries no outer gap
  of its own, so the dot's diameter is free to change without moving a single
  character of text.
- `sidebar_row` is a `ListItem` base, not a `div`: the library owns the
  hover/selected/disabled treatments, and machine rows rely on `disabled`
  gating the row while still rendering a live suffix button (see
  `render_missing_machine_row`). It sets a `tab_index` so rows are reachable by
  keyboard; a caller that must *not* be a tab stop (the machine rows, which are
  not clickable) opts out with `tab_stop(false)` — still focusable
  programmatically, just skipped by Tab, because a row with no click handler is
  a dead end to land on — and a caller that is opts into the ring with
  `keyboard_focus_ring`. The ring cannot live inside `sidebar_row` itself: it
  needs `cx` for the theme's ring color and this builder takes only an id, so
  the split is deliberate — the row owns reachability, the call site owns the
  ring. It no longer restates a text size — the ambient body size is correct
  now that nothing overrides it. The size was *removed* rather than read from
  the theme for the same reason the ring stayed outside: both roots that can
  host a row (the workbench root and the gallery's chrome root) set the theme's
  font size themselves, so the row renders at exactly the size it used to
  declare with one fewer place to drift. A row mounted under some third root
  wants the size restored at that root, never back on the row.
- `sidebar_stacked_row` takes a **`Tone`, never a shape**: the shape is derived
  by `Tone::is_hollow` inside `status_dot_for_tone` (see `ui/core.rs`), so no
  caller picks solid-versus-hollow any more. It is one shape for both lists: status lane + title on the first line,
  an optional inline `note` (transient status, e.g. "opening…") after the
  title, and an optional `detail` second line (the row's context — provider, or
  folder · machine). Both secondary texts are `text_sm` and resolve their color
  through `Tone::Muted` rather than `cx.theme().muted_foreground` directly, so
  `ui` keeps its rule that a component asks for a meaning, never a theme field
  (SPEC C11).
- `status_lane` is a private builder rather than a bare dot because the dot has
  to *say* something: it carries `Tone::label` as both its `aria_label` and its
  tooltip. A color alone is not a status for a screen reader, and it is not one
  for a sighted user who has not memorized six shades either.
  `STATUS_LANE_ELEMENT_ID` is the bare string `"status"` for every row, which
  is correct rather than lazy: gpui element ids are paths, and every row
  already carries a unique id, so a leaf only has to be unique among its
  siblings.
- `sidebar_empty_note` is the "no sessions yet" line. It is a plain note, not a
  row: an empty list has nothing to select, so it must not look clickable.
- The statusbar band is `h_8` (2rem, 28px) — one line of `text_sm` with real
  breathing room, still short enough to read as a status strip rather than a
  second toolbar. There is no `STATUSBAR_HEIGHT` const any more: the rem step
  *is* the number, and restating it in pixels would just be a second place for
  it to be wrong. Its text is `text_sm`.
- `collapsible_section` is the whole disclosure control — header, chevron,
  click target, `Role::Button`/`aria_label`/`aria_expanded`, tab stop and
  focus ring in one call — because a header that toggles nothing is not a
  thing the app has, and splitting it left every caller re-wiring the same
  pieces (the sidebar did it twice, identically; the gallery then re-applied
  the aria attributes a third time, which is what moved them in here). It
  takes `expanded` (not `collapsed`) so the parameter reads the same way as
  the glyph it selects, plus the caller's own click listener; the collapse
  *state* stays with the screen that owns it, since `ui` holds no state. The
  header text brightens in two steps — `faint` at rest, `dim` on hover, full
  foreground on press — so the disclosure answers pointer-down like every
  other control.
- `section_header` is private for that reason: it is half of
  `collapsible_section`, and a caller reaching for it alone would be building
  a disclosure control that cannot disclose. The chevron is the shared
  `disclosure_chevron` icon from `core.rs`, which inherits the header's faint
  color from the ambient text style.
- `section_header`'s box is aligned to the sidebar's rows, not to itself: it
  takes the same `mx(SIDEBAR_ROW_GUTTER)` / `px(SIDEBAR_ROW_PADDING)` pair
  `sidebar_row` does and rounds `rounded_md`, so its focus ring draws the same
  pill in the same lane as the row underneath it. That shared pair is the whole
  argument for one file: a sidebar-shaped header that the gallery's dock
  happens to reuse has to resolve from the row's geometry, and reading it
  across a module boundary meant publishing a padding constant to one caller.
  Its padding doubled with the rest of the chrome, `pt_2`/`pb_1` →
  `pt_4`/`pb_2`, which keeps the deliberate 2:1 asymmetry (more air above a
  heading than below it, so the heading binds downward to the rows it labels)
  while giving the header a box with real room in it. That box used to be
  invisible; now it rounds and takes a focus ring, so its shape is something
  the user actually sees, and at `pb_1` there was under 4px between the text
  and the ring's lower edge.
- `chat_header` is `py_3` for the same reason the statusbar grew, and its
  subtitle is `text_sm`. The subtitle slot has a rule: it names the next action
  or the target, never a restatement of the title. The chat pane pairs "New
  session" with "Pick a machine and a repository below", or a live session's
  title with `<folder> on <machine>`; the gallery pairs a story name with its
  group, which is the story's location. A subtitle that only echoed its title
  would be the header charging twice for one line of information.
- Pointer conventions, app-wide: `cursor_pointer()` is not how this app says
  "clickable". Nothing in the chrome navigates to an external resource, so
  nothing takes the pointing hand — hover fills, press fills and the focus ring
  carry affordance instead. The library agrees by default: `Button` sets
  `cursor_default()` and only opts into a pointer for its link and text
  variants.

## crates/harvester/src/ui/theme.rs

Harvester's palette — "Graphite Dark", a cool near-monochrome (every surface
is the same hue-240/5%-saturation gray at a different lightness) replacing
`gpui-component`'s pure-neutral dark theme. Installed by
`shell_bootstrap::install_widget_layer` right after `Theme::change(Dark)`.

- It ships as a `ThemeConfig` applied via `Theme::apply_config`, *not* as
  field-by-field mutation of `Theme::global_mut`, because `apply_config` is
  what derives every slot we don't specify (button variants, tokens like
  `list_hover`, table colors) from the ones we do. Mutating colors directly
  would leave `ThemeTokens` stale. The config is also stored back into
  `theme.dark_theme`, so any future `Theme::change(Dark)` re-applies graphite
  instead of reverting to the library default — and the eventual light theme
  slots in beside it as a second config.
- The lightness consts are an elevation ladder, darkest = furthest back:
  `CHROME` 7% (sidebar, title bar, status bar, control dock),
  `CANVAS` 9% (content background), `SURFACE` 12% (popovers, tool-call
  cards, story panels), `HOVER` 14% (row hover fill), `INPUT_WELL` 15%
  (prompt box fill and input borders — the `input` slot serves both roles in
  the library), `RAISED` 16% (message bubbles, tags, secondary buttons —
  `muted`/`secondary`),
  then `RAISED_HOVER` 19% / `RAISED_ACTIVE` 22% for interaction states.
  Depth comes from this ladder plus hairline borders, not from decoration.
- Borders: `HAIRLINE` 18% against content surfaces, `CHROME_HAIRLINE` 13%
  where chrome meets chrome (sidebar/status-bar edges, window border) —
  chrome is darker, so its separators must be too or they read as lines
  instead of edges. `EDGE` 30% is the "visibly outlined" tier: the selected
  list row's border and the text-selection fill (selection stays monochrome
  on purpose; the default theme's blue selection was the one saturated
  surface in an otherwise gray app).
- Text: `TEXT_BRIGHT` 94% and `TEXT_DIM` 63%; the third (faint) tier remains
  `core.rs`'s `FAINT_TEXT_OPACITY` over `TEXT_DIM`. `PRIMARY` stays near-white
  (93%, hover 97%, press 86%) — filled controls are light-on-graphite, and
  status color keeps coming exclusively from `Tone`.
- `CONTROL_RADIUS` 6 / `PANEL_RADIUS` 10: controls stay tight, large
  surfaces (dialogs, notifications) round more — the density lives in
  spacing, not in sharp corners. Both reach `ui` as `cx.theme().radius` /
  `radius_lg`, and the surfaces that carry a *deliberate* elevation tier (the
  prompt box, a tool card, the dialog option list, a user bubble) round
  through those slots rather than through gpui's fixed `rounded_md`/
  `rounded_lg` steps, so the theme stays the one place either radius is set.
  That is not only about ownership: gpui's rounding helpers are rem steps, so
  they would *grow with the font size*, and a corner that opens up when you
  zoom the type is wrong — a radius is a property of the frame, not of the text
  inside it. The two values stay bare numbers here because the config's radius
  fields are plain integers; they are theme definitions, and the rule they
  carry is on the consumer side.
- `BASE_FONT_SIZE` (14) is set as **both** `font_size` and `mono_font_size`.
  This is the single most load-bearing number in the visual layer, because in
  gpui the base font size *is* the rem unit: every `px_3`, `gap_2`, `h_8`,
  `max_w_56` and `rems(...)` in `ui` is a multiple of it. Changing it rescales
  the whole app's density in one step, which is exactly why no component is
  allowed to restate a size in pixels that could have been a rem. Setting the
  mono size to the same value keeps a code run in a tool block on the same
  line box as the prose around it. It also fixes the app's secondary text
  tier: `text_sm` is 12.25px at rem 14 and `text_xs` would be 10.5px, under
  the ~12px typography floor, which is why `text_xs` appears nowhere in
  `crates/harvester`.
- The mechanism behind that is `gpui_component::Root::render` calling
  `window.set_rem_size(cx.theme().font_size)`: this one theme slot *is* the
  window's rem unit, for the library's own controls as much as for ours. The
  config used to leave it unset — the library's default is 16 — while the app
  root painted its text at 13, so type and geometry ran on two scales and the
  geometry was around a fifth too airy for the text it framed. Owning the slot
  is what turned the `px()` literals scattered through `ui` from
  convert-by-hand into delete-and-read-the-scale.
- `mono_font_size` is set explicitly even though the library ships a mono
  default of its own that would often agree. The whole point of the axis is
  that *one* const drives it, and an unset field would silently desync the
  transcript's mono runs the next time either upstream's default or
  `BASE_FONT_SIZE` moves; the cost of stating it is one line. Watch item: a
  mono family reads optically larger than Inter at the same nominal size, so if
  the transcript ever feels heavy the fix is a separate `MONO_FONT_SIZE` const
  one step down — not dropping the field and going back to inheriting a number
  nobody here chose.
- Hex literals are confined to this file. No other module in `ui` contains a
  color literal of any kind; `core.rs` in particular returns theme roles
  exclusively, which is what makes SPEC C11 checkable by reading one file.
- `graphite_theme_colors` fills the config field-by-field through
  `slot(hex)` (= `Some(hex.into())`), so each line is the slot name and the
  palette constant and nothing else. A single struct-update expression
  (`ThemeConfigColors { …, ..Default::default() }`) was tried and does not
  compile: the struct has private `blue`/`red`/… palette fields upstream, and
  functional record update is rejected (E0451) when any field is private.
- `UI_FONT` "Inter" / `MONO_FONT` "Geist Mono": both set as theme slots
  (`font_family`, `mono_font_family`) so `Root` styles the whole window and
  gpui-component widgets agree with our own mono runs (`core.rs` reads the
  slot back). gpui matches families by exact name; if a family is missing,
  the platform text system substitutes, so a machine without these installed
  degrades instead of failing.

## crates/harvester/src/ui/transcript.rs

Transcript presentation: messages, markdown, the tool card's chrome, tool
labeling/bodies, and expansion state, layered on top of
`harvester-client-core`'s `Transcript` (the verbatim-derived conversation
replica). This module adds *only* visual policy and keys all of it by stable Pi
tool-call ids, so a history repair can replace transcript items wholesale
without moving expansion state onto the wrong block.

- `TranscriptPresentation` owns exactly the state that belongs to rendering:
  which stable `toolCallId`s the user expanded. Session lifecycle, Pi events,
  prompt state, and scrolling all remain in `Chat` — this type is deliberately
  narrow.
- The scroll handle passed into `render` is owned by `Chat`: following new
  events as they stream in is lifecycle/adaptation policy, while the
  scrollable message surface itself is presentation policy — the split keeps
  those concerns in their respective owners.
- The message column is `gap_4` / `py_4` / `px_3`. The transcript is the one
  surface the user reads continuously, so it takes the standard tier
  vertically ahead of everything else; at rem 14 the compact spacing let a
  tool card and the paragraph under it touch. Horizontally it stays at `px_3`
  because that is the chat column's shared leading edge — `ui::chat_header`
  above it and `ui::prompt_strip` below it are both `px_3`, and three siblings
  in one pane with three different left edges is the kind of drift nobody sees
  individually and everybody feels. Vertical rhythm is the transcript's own
  business; the leading edge is not. The transcript owns this padding
  outright: neither the chat pane that hosts it nor the gallery's
  `transcript_panel` (explicitly `p_0`) adds any of its own, so there is one
  place to change it and no chance of doubling it.

The `tool_block_*` builders are the tool card's reusable visual primitives —
frame, typography, status treatment. They all carry the `tool_block_` prefix,
which they did not need while they lived behind a `tool_block::` module path.
They were merged in from a separate `ui/tool_block.rs` because this module was
their only consumer and already held half the render (`render_tool_body`,
`render_diff`, `render_written_content`, `diff_line_kind`); the prefix is what
keeps `tool_block_body` from reading as "the body" and `tool_block_header` from
colliding with the chat pane's header in a reader's head. Nothing outside `ui`
draws a tool block — the gallery reaches one only through
`TranscriptPresentation::render` — so every one of them is module-private, as
is `DiffLineKind`.

- `tool_block_frame` renders on the `popover` slot with a hairline border,
  rounded through `theme.radius_lg`: in the graphite elevation ladder a tool
  card is a raised surface over the canvas (it was previously the `sidebar`
  slot, which the graphite ladder repurposed as the *darkest* tier — keeping it
  would have sunk the cards below their page). It carries **no** shadow. A
  transcript can hold a dozen of these in a row, and a dozen stacked
  `shadow_sm` cards read as clutter rather than depth; the fill and the
  hairline already place the card on the ladder, which is what a shadow was
  there to do. The header chevron is `core::disclosure_chevron`, tinted `faint`
  like every disclosure marker.
- `tool_block_header` is `py_2` (up from a bare `px(6.)`, i.e. the compact
  tier), takes a `tab_index`, and carries `Role::Button` with `aria_expanded`
  and an `aria_label` the caller supplies (see `tool_label` /
  `tool_accessible_name` below). It takes its `index` rather than an
  `ElementId` and builds `(TOOL_BLOCK_HEADER_ELEMENT_ID, index)` itself, so the
  id scheme lives in one place with the status slot's matching
  `(TOOL_BLOCK_STATUS_ELEMENT_ID, index)` — the row needs a stateful id for
  focus and click, the indicator needs its own for the tooltip, and letting the
  caller invent either one invites a collision. `TOOL_BLOCK_HEADER_TAB_INDEX`
  (0) makes every tool header an ordinary tab stop in document order, and that
  single call is the whole keyboard story: the existing `on_click` toggle
  answers Enter and Space with no key handling written here (see `ui/shell.rs`
  for the gpui mechanics).
- Focus on `tool_block_header` is a transparent 1px border that turns
  `theme.ring`, not the shared `keyboard_focus_ring` shadow. The card clips its
  children (`overflow_hidden`), so a shadow drawn outside the header's box
  would be cut off; reserving the border at `transparent_black()` means gaining
  focus changes a color and never a layout.
- `tool_block_header` has both a hover fill (`list_hover`) and a press fill
  (`muted`): press feedback lands on pointer-down, matching every other
  clickable surface in the app.
- `tool_block_status_indicator` is the dot in a fixed `size_4` slot carrying
  the caller's status word as its tooltip — same rule as the sidebar's status
  lane: a color is not a status unless something says what it means. The word
  is *not* `Tone::label` here; it is `tool_status_label`, because the tones
  speak the machine- and session-liveness vocabulary and a finished tool call
  is "Completed", not "Neutral". The slot is `size_4` rather than hugging the
  6px dot for two reasons: a 6px tooltip target is not something a pointer can
  reliably hit, and a fixed-width indicator gives a stack of repeated rows one
  stable icon column, which is what keeps every header's label starting on the
  same x.
- `tool_block_stats` is one faint run (`+3 −1`), not a green `+` beside a red
  `−`. Diff counts are metadata about an edit, not a verdict on it, and
  spending the success/danger colors on them made a routine edit look like a
  test result — the two colors that mean "it worked" and "it failed" have to
  stay scarce. Semantic color is legitimate where it *is* the data, which in
  the tool card is the diff lines in the body and nowhere else.
  `tool_block_stats` also gave up its own smaller size, which is what lets the
  `items_baseline` label row line up exactly instead of nearly.
- The card's emphasis budget is three text tiers and no more: full foreground
  for the subject (the file name or the command), `dim` for the verb and the
  output body, `faint` for the stats, the context line, the chevron and the
  truncation notice. Together with `tool_tone` mapping a settled call to
  `Tone::Muted`, that leaves a finished tool block with exactly one focal point
  and, in the header, exactly one colored element — the dot. Color in the
  transcript means "running" or "broken" and nothing else.
- Header and body deliberately sit at the same `text_sm` size. They are two
  halves of one secondary-tier card, told apart by a hairline and a font
  change, not by size; the body used to be smaller than its own header, which
  inverted the label/content relationship. One step below the message prose is
  the right place for the whole card: prose is the content, tool rows are the
  scannable log of how it was produced, and under the old two-scale mismatch
  the log rendered *larger* than the work it described.
- `tool_block_body` is `text_sm` on the mono family with an explicit
  `line_height(relative(CODE_LINE_HEIGHT))` (1.45). Code needs tighter leading
  than the 1.7 prose uses — it is the *ratio* between the two that carries the
  meaning (code reads as one block, prose wants air), not either number on its
  own — and `TextViewStyle` has no line-height field, so the wrapper is where
  it is set — the same technique `message_frame` uses for prose.
  `tool_block_code_lines` is a bare unpadded column so a run of diff or written
  lines stacks with no gap between them; the `gap_1` on `tool_block_body`
  separates the *sections* of a body (context line, content, output), not the
  lines inside a section. Equal gaps claim equal relationships, and consecutive
  lines of one file are not siblings with space between them — their leading is
  what should separate them, which is exactly what `CODE_LINE_HEIGHT` is for.
- `tool_block_truncation_notice` says "Output truncated" as a sentence, not "…
  output truncated": it is chrome stating a fact about the block, and the
  leading ellipsis made it read like part of the output it is describing. No
  final period either — a short state is a label, not a sentence to be closed.
- `tool_tone`: `ToolStatus::Running` maps to `Tone::Active` rather than
  `Tone::Pending` — a running tool is the live thing the user is watching on
  screen right now, not a "just working, outcome unknown" state. `Ok` maps to
  `Tone::Muted`, not `Tone::Idle`: a settled tool call is ordinary secondary
  content, and `Idle`'s extra fade (0.6 over an already-dim gray) pushed a
  perfectly legitimate result under the contrast floor. `Idle` is now reserved
  for things that are genuinely *unknown* — an unprobed machine — which is what
  also makes `Tone::is_hollow` render its dot hollow.
- `ToolSummary` is the one description of what a tool call *is* — verb,
  subject, whether the subject is monospace, and any line stats — computed once
  by `tool_summary` and consumed twice: `tool_label` renders it, and
  `tool_accessible_name` speaks it. Before it existed, `tool_label` built the
  header inline per tool name, which meant a screen reader could only be given
  the whole rendered `Div` (i.e. nothing). Deriving both from one struct is
  what makes the spoken name and the drawn label unable to disagree.
- `accessible_verb` is separate from `verb` because two of them are not words.
  `bash` draws `$` and speaks "Run"; everything else draws and speaks the same
  string. A `$` is a perfectly good visual verb and a useless spoken one.
- `tool_accessible_name` is `"{verb} {subject}, {status}"` ("Read main.rs,
  Completed"). Status is included because the dot that carries it is invisible
  to a screen reader, and it comes last because the action is what identifies
  the row — a list of blocks all beginning "Completed" would be unnavigable.
  Both shapes are pinned by tests, and the reason they are worth a test is that
  this string is the selector surface agents drive the gallery through over the
  a11y tree — it is an interface, not a nicety.
- `tool_status_label` is the tool block's own status vocabulary
  ("Running"/"Completed"/"Failed") rather than `Tone::label`'s: the tones are
  named for what they *mean* across the app, and a settled tool call is
  "Completed", not "Neutral". The same string feeds both the status dot's
  tooltip and the header's accessible name, so what the block shows on hover
  and what it says out loud cannot drift apart. The *shape* channel still comes
  from the tone, through `Tone::is_hollow`.
- `output_text` centralizes the empty-output case: "Running…" while the call is
  in flight, "No output" once it has settled. It was `"(no output)"` —
  parenthesised asides are how a UI mumbles, and a tool that genuinely printed
  nothing is a fact worth stating plainly. Pinned by a test. Its match spells
  `ToolStatus::Ok | ToolStatus::Error` rather than a `_` arm, so adding a fourth
  status makes the compiler ask what the placeholder should say instead of
  silently answering "No output".
- `render_tool_body` is now a router over named builders (`render_tool_content`
  over `render_diff` / `render_written_content` / `render_written_line`, and
  `output_text`) rather than a chain of early returns; the behavior is
  unchanged, but every branch has a name and the early-return shape is stated
  once instead of being inferred: `render_tool_content` decides whether this block has
  *content* (a diff, or a `write`'s written lines rendered as added lines —
  neither offered for a failed call, because that is where the error message
  has to win), and only when it has none does the body fall through to the
  output snapshot. The truncation notice moved to *after* the output rather
  than before it: it describes what the user just finished reading, and above
  the output it read as a warning about text they had not seen yet.
- `markdown()`: assistant prose renders through `TextView::markdown`, whose
  state lives in the window under `id`. Unchanged settled text returns early;
  only the one currently-streaming partial reparses per chunk — this is a
  performance/architecture note tied to SPEC C11.
- `markdown_style()`: the app is pinned dark, but the library's default
  highlight theme is light, so this builds the matching dark style once
  (`OnceLock`) rather than repeatedly.
- `render_message`: user text is rendered verbatim (never markdown); only
  Pi's own prose goes through the markdown renderer.
- `message_frame` renders chat-app style with no author labels: the roles
  are told apart by shape alone. User messages are a filled bubble (`muted`
  bg, `px_3 py_2`, rounded through `theme.radius` so the bubble tracks the
  theme's control radius) floated right (`justify_end`) and capped at
  `USER_BUBBLE_MAX_WIDTH` (70% of the pane) so they read as "sent"; the
  agent's prose flows flush left at full width with no padding of its own.
  Every body sets `line_height(relative(MESSAGE_LINE_HEIGHT))` (1.7):
  `TextViewStyle` has no line-height field, but gpui text style inherits, so
  the wrapper is where transcript leading is set for both plain text and
  markdown. With the "pi" label gone, `render_failure` carries its
  `Tone::Failed` color on the text itself.
- `render_failure`: a failed turn is Pi-authored but intentionally *not*
  rendered as markdown, because provider error strings must preserve literal
  `*` and `#` characters rather than have them interpreted as formatting. It
  paints a whole paragraph in `Tone::Failed`, which is a great deal of color by
  this module's own standards — kept deliberately, because a failed turn is
  precisely where the emphasis budget should be spent and it is rare.
- `render_streaming`: shows the in-flight turn's partial markdown (suffixed
  with a `▍` cursor), or a bare "…" activity placeholder before the first
  partial chunk arrives, or nothing if the transcript isn't busy.
- `tool_summary`: header policy for Pi 0.83.0's built-in tools (`bash`, `read`,
  `write`, `edit`, `grep`, `find`, `ls`), one small named builder per family
  (`bash_summary`, `file_summary`, `search_summary`, `list_summary`). Any tool
  name Pi doesn't ship falls through to `unknown_tool_summary` — the raw name
  plus a compact raw-args JSON summary — instead of inventing bespoke label
  vocabulary for an unknown payload shape.
- `render_tool_body`: expanded-body policy is context line first, then either
  the edit/write diff or a plain output snapshot. Errors always show output
  regardless of diff availability, because that's where the error message
  lives.
- `context_line`: the faint first line of an expanded tool body carries just
  enough data to identify the call in full (path for read/write/edit, command
  for bash, raw args otherwise).
- `diff_line_kind`: classifies Pi's own numbered diff line format —
  `" 1 ctx"` / `"-2 old"` / `"+2 new"` — by first byte.
- `compact_args`: caps the raw-JSON summary for an unknown tool at
  `ARGS_SUMMARY_MAX_CHARS` (80) *characters*, ellipsis included, by handing the
  serialized JSON to `ui::core::truncated_to_max_chars` — the same cut the
  prompt strip's control labels take. It used to do the cut itself, with a bare
  `'…'` literal appended *after* taking a full 80 characters, so its results
  were 81 characters long and its own test asserted `MAX + 1`; the budget now
  means the same thing in both places. Counting characters rather than bytes is
  unchanged and still the point: a Unicode argument is never split
  mid-codepoint.
- `file_summary`'s subject is `harvester_client_core::folder_name(path)`. That
  was a private `basename` here until the app's *other* "last path component"
  (`workspaces.rs`'s `folder_name`, which returned `""` for `/repo/` and for
  `/`) was found to disagree with it. This module had the correct body and the
  pinned tests; both moved down to client-core, which is the lowest crate the
  lib-side `ui/` and the bin-side sidebar/chat both already depend on — `ui/`
  cannot see `workspaces.rs` at all, so nothing shorter of a shared crate would
  have worked.

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

Both of those lines are decided by a window-free `present_*` function
returning one value, per the rule stated in the `models.rs` section:
`present_empty_pane` and `present_statusbar_link` each return a `ShellMessage
{ text, tone }`, and the two render methods only read the store and draw it.
They returned `(String, Tone)` tuples until then, which is the same shape a
presentation value has but with the fields unnamed and destructured at every
call site. `argv_machine_state(store)` is the one shared read (the argv
machine and its `MachineState`), passed in so neither presenter touches an
entity. A statusbar link falls back to the connected/linked counts both when
there is no argv machine and when `present_machine` has nothing to say about
the one there is.

Field-level invariants on `Workbench`:
- `store: Entity<FleetStore>` is the single observable store; the shell reads
  it for the empty pane/statusbar and for root-window chat orchestration.
- `modal: Entity<ModalCoordinator>` is the whole modal layer: the Pi
  obligation FIFO, the folder picker and the forget-machine confirmation
  behind one `ActiveModal` (see `workbench/modal.rs`). The shell holds it for
  its store subscription, to lend Root out on a chat's or a sidebar's request,
  and to hear the `ModalOutcome` events it must react to. The Root overlay
  itself is still placed by `Workbench::render`.
- `chat: Option<OpenChat>` is the current conversation, whether or not a
  session exists behind it yet (an "unstarted" chat has no sid). `OpenChat`
  holds the `Entity<Chat>` *and* its `_flow_requests` subscription (the chat's
  bridge traffic no longer comes through here; see `FleetStore::send_gated`).
  They were two `Option` fields set and cleared together at three sites, where
  a fourth site clearing one and not the other would have left a dropped chat's
  subscription alive; one struct makes "the subscription dies with the chat" a
  type fact rather than a convention. Every read costs one field hop
  (`open.chat`), which is the whole price.
- `_store_repaint` / `_store_events` / `_sidebar_intents` / `_modal_outcomes`
  are one named `Subscription` field each, directly on `Workbench`. They were
  a `WorkbenchSubscriptions` wrapper struct — a third shape for holding
  subscriptions, next to the arrays in `chat.rs` and `workbench/picker.rs` and
  the plain named fields in `workbench/sidebar.rs` and `workbench/modal.rs` —
  and the wrapper did not even hold all of them, since the chat's own
  subscription lived outside it. The whole crate is named fields now; a
  wrapper earns its place only when it pairs a subscription with something
  else that must die with it, which is what `OpenChat` above does and this did
  not. Repo-added and machine-forgotten used to be two event types subscribed
  separately; they are one `ModalOutcome` enum now, with
  `handle_modal_outcome` a pure router into the two unrelated handlers (house
  rule: match arms delegate).

Event-handling ordering rules (load-bearing, not incidental):
- `handle_store_event` is the only bridge event the shell owns; it builds or
  promotes a chat and therefore needs a `Window`. Dialog events are consumed
  by `ModalCoordinator`; chat answers are consumed by `Chat` itself.
- `StoreEvent` subscribers do **not** run while `FleetStore` is leased.
  `cx.emit` queues an effect that gpui dispatches from `flush_effects` after
  the emitting `update` has returned, so during a subscriber callback only the
  subscriber itself is leased and it may freely update its emitter.
  `handle_attached` therefore constructs or promotes the chat synchronously,
  straight from the match arm; the `defer_attached` hop that carried owned
  machine/sid/entries into `window.defer` is deleted, because the re-lease
  *inside* that deferred closure was the only thing in it that could have
  panicked. The property the hop was credited with is not lost: gpui's
  `SubscriberSet::retain` takes the emitter's subscriber list before
  dispatching and merges back whatever was added during it, so a chat that
  subscribes to the store while the store is mid-dispatch still cannot observe
  the event that created it, and is ready for every later one.
- Sidebar intents dispatch synchronously for the same reason:
  `handle_sidebar_intent` *is* the subscriber and the match, and its arms may
  freely update the store or the emitting `Sidebar` from inside that
  callback, which gpui allows.
- Ordering shift this created, recorded because it is real and was checked
  rather than assumed: `ModalCoordinator` subscribes to the store before
  `Workbench` does, so it still runs first on an `Attached` event, but its
  retirement work now queues `defer_advance` *behind* `handle_attached`
  instead of ahead of it. The two paths share no state — the coordinator
  touches the obligation FIFO and Root's dialog stack, while `handle_attached`
  touches the store catalog and the chat — so which runs first is not
  observable.
- `handle_attached`: the chat that minted a given sid is already on screen
  with the first prompt rendered by the time `Attached` arrives, so the
  handler promotes that existing chat in place (`promote_to_live`) rather
  than rebuilding it from scratch — this preserves scroll position and any
  local input state.
- `install_chat` subscribes to the chat's flow seam *before* asking it to
  report its prompt-strip state: an entity cannot emit to a subscriber that
  does not exist yet, so subscribe-then-notify ordering matters.

Machine-forgotten handling: once a machine is gone from the client's own
configuration, a chat still pointed at it has no bridge to send to and no
sidebar row to return to. Dropping the `Chat` entity is the entire teardown —
its subscriptions and pending requests die with it, exactly like discarding
an abandoned unstarted chat. The sidebar highlight needs no separate
teardown: selection lives in the catalog and is dropped there with the rows
that carried it.

Session selection is not mirrored here. The shell used to push the current
chat's sid into the sidebar at four points; the sidebar now reads it from the
store, which learns it where it already learns everything else — a session is
selected when this client attaches to it, and `open_new_session_chat` clears
it because a chat with no session behind it selects no row. That is the one
push left, and it names what it does.

Repo-added handling: a repository the folder picker validated and stored is
routed back only to the chat that asked for it (matched by machine), which
then selects it — that's the entire purpose of the add-repository detour.

`open_new_session_chat`: New session is always available. It clears the
store's selected session first — an unstarted chat has no sid, so no row is
current while the user is composing it. The chat opens on
defaults the store derives (see docs/SPEC.md "Start session" under Flows),
and the user re-picks machine/model in the prompt strip if the defaults were
wrong. An already-open *unstarted* chat is left alone on a repeat click —
reopening it would only discard what the user already typed.

`handle_chat_flow_request`: the chat's repo picker asks the shell for a
folder walk because it needs Root's dialog stack, which only this shell can
lend out (through `modal`).

There is no `handle_chat_request` any more. A chat's bridge traffic used to
be emitted as `ChatRequest`, re-typed here into the identical
`bridge::Command`, gated for liveness and handed to `store.send`; the refusal
then had to be pushed *back* into the chat as `fail_pending_requests`. All of
it collapsed into `store.send_gated`, which the chat calls itself: the store
already owns liveness, already owns the notification path, and the chat
already owns the request state that a refusal has to unwind. What the shell
kept is what genuinely needs the window — building and promoting chats, and
lending out Root's dialog stack.

Render: Root owns the dialog/notification overlay stacks but does not render
them itself — placing `Root::render_dialog_layer` / `render_notification_layer`
remains this top-level shell's responsibility.

## crates/harvester/src/workbench/machine_presentation.rs

Gives one consistent presentation for a given machine state across three
different surfaces: the sidebar, the empty content pane, and the statusbar.

`MachinePresentation`: the strings differ per surface because they are
different *sentences* about the same state, not because the state means
something different in each place. Choosing the `tone` exactly once in
`present_machine` is what prevents a failed state from rendering red in one
surface and gray in another — tone and message are decided together instead
of being independently derived per call site.

### Constants

Every sentence this module owns is a named const (DESIGN.md §8), because this
is the module that *decides* what a machine state says and the words were
otherwise scattered across a nine-arm match: `CONNECTING_STATUSBAR_LINK`,
`CONNECTED_PANE_MESSAGE`, `CONNECT_FAILED_STATUSBAR_LINK`,
`CONNECTION_LOST_MESSAGE` and `MISSING_MACHINE_DETAIL`.

- `CONNECTION_LOST_MESSAGE` is deliberately one const read twice: the pane
  message and the statusbar link are the same two words for `ConnectionLost`,
  and the reason they match is that there is nothing more to say in either
  place, not that they happened to be typed the same.
- `MISSING_MACHINE_DETAIL` (`"no longer listed"`) is `pub(super)` and is
  `workbench/sidebar.rs`'s subtitle for a missing machine row. It was declared
  in both files. This one owns it because this is where `MachineState::Missing`
  becomes words; the sidebar is a second surface for the same verdict.
- The two interpolated sentences keep a `format!` and name only their fixed
  part, because a `format!` template cannot be a `const`:
  `CONNECTING_PANE_MESSAGE_PREFIX` (`"connecting to "`, with the machine and
  the in-progress ellipsis following it) and
  `MISSING_MACHINE_PANE_MESSAGE_SUFFIX` (`" is no longer listed by its
  provider"`, with the machine leading it).
- `ConnectFailed`'s pane message and `Connected`'s statusbar link have no
  const because neither is copy: one is the failure detail the fleet handed
  up, the other is the machine's own name.

The field is a `Tone`, never a dot shape. It used to be a hand-built `Dot`,
and every consumer threw the shape away and re-derived it from the tone, so the
shape recorded here was not what got drawn: a `Reachable` machine written as
solid-but-`Idle` rendered *hollow*, which is the treatment reserved for a
machine nobody has probed. Naming only the tone makes that class of
disagreement unrepresentable. `Reachable` is `Tone::Muted` for exactly this
reason — it is a settled verdict and must read as solid — while `Unknown`
keeps `Tone::Idle` and stays hollow. A unit test pins the two apart, asking
`Tone::is_hollow` now that the `Dot` type it used to compare against is gone.

`pane_message` / `statusbar_link` are both `None` for probe-only states
(`Unreachable`, `Reachable`, `Unknown`) — nothing about a daemon connection
can honestly be reported for a machine that has only been probed, not
connected to.

`Connected`'s `statusbar_link` is the machine name alone. It used to read
`<machine> · harvesterd <daemon_version>`, where `daemon_version` was the
protocol version the client had *just asserted* equals its own — so the
number was always "1" and said nothing a connected machine doesn't already
say. The payload is gone from `MachineState` entirely (see
harvester-client-core.md §fleet.rs); if a build identity is ever worth
showing here, `ping.build` is the real one and it is bootstrap's to publish.

`MachineState::Missing` handling: a "missing" listing is only authoritative
after its provider has actually spoken (i.e. this isn't inferred from mere
silence). The fleet/store layer owns that precedence decision; this
presentation function only names the verdict once the store has already
committed to it.

## crates/harvester/src/workbench/modal.rs

The single owner of `gpui-component`'s one global Root dialog stack. Root
stores every dialog in one vector and `close_dialog` always pops its *top*,
so Harvester models "what is on the modal layer" as one `Option<ActiveModal>`
inside one entity, rather than arbitrating independent owners. Two modal
flows at once is not rejected at runtime — it is unrepresentable.

`ActiveModal` is the whole rule:
- `PiQuestion { key }` — the `DialogKey` of the obligation currently on
  screen. This is also the old `displayed` field: what is shown and who owns
  Root are the same fact, so they are one field.
- `AddRepo { machine, picker, _picker_events }` — the folder walk
  (docs/SPEC.md §Flows "Add repository"). The machine is *not* chosen here:
  a repository is picked inside the chat that is about to run in it, and that
  chat already has a machine. Holding the picker entity here is what replaced
  the old generation-tagged `FlowId`: the subscription that could deliver a
  picker event is the one stored beside it, and it dies when the variant is
  replaced, so a late event from a previous walk has nobody to reach.
- `ForgetMachine { machine }` — the removal confirmation (docs/SPEC.md
  §Flows "Forget machine"). Only a machine its provider no longer lists ever
  reaches this flow (the sidebar puts the trash icon on no other row kind),
  so there is nothing to disconnect and nothing on the remote machine to
  delete; the removal is entirely about what this client still *claims* to
  know. Holding the machine here replaced the old `ConfirmationId`.

Why the generation counters are gone at all: `Root::close_dialog` pops
synchronously and never fires a dialog's callbacks, so a "late callback from
a dialog that is already closed" cannot happen — a closed dialog does not
speak. Every stale-callback guard those counters existed for was therefore
guarding an unreachable state, and the surviving guard is the cheap one: a
callback only does its work when `active` still names its own flow.

The Pi half (client-side implementation of docs/DAEMON.md §Dialogs):
- The obligation FIFO itself is `DialogObligations` from
  `harvester-client-core` — window-free, and it decides ordering, duplicate
  replay, answers and retirement on its own. This module is its GPUI adapter:
  subscribe to the one store, place the front obligation on Root, and send
  exactly one answer back through the same machine route.
- `enqueue` → `advance` unconditionally when the obligation was accepted.
  `advance` is idempotent (it returns immediately when something is already
  on screen, or when the FIFO is empty), which is why "this one is visible"
  and "this one is queued" is not a distinction the enqueue result has to
  make.
- `advance` builds the input entity at display time, through
  `dialogs::open_pi_dialog`. Queued obligations own nothing: there is no map
  of pre-created inputs to garbage-collect, because typed text can only exist
  in the obligation that is on screen and a displayed obligation is never
  displayed twice.
- `answer_front`: the dialog key is checked twice — once here against
  `ActiveModal::PiQuestion`, once inside the window-free
  `DialogObligations::answer_front` — specifically so a duplicate or delayed
  answer callback from an already-replaced modal can never consume its
  successor's answer.
- `apply_retirement` acts only when the *displayed* obligation was retired;
  retiring queued obligations is pure bookkeeping and must leave the screen
  alone. Its order is load-bearing: clear `active` first, then pop Root, then
  queue the advance — popping while `active` still names the retired question
  would let a synchronous advance push onto a stack that is about to be
  popped.

`defer_advance` is the one `window.defer` left in the client, and every
release path goes through it. `answer_front` and the two dismissal callbacks
run *inside* `gpui-component`'s own dialog callback, and the library pops the
top of the stack **after** that callback returns (`if on_ok(…) {
window.close_dialog(cx) }` in the library's `Dialog`; `select`'s button
handler performs an equivalent guarded pop). The pop is a stack pop, not a
keyed removal, so a *synchronous* advance would push dialog B and the library
would immediately pop B — leaving the answered dialog on screen with `active`
already cleared, which is a wedged coordinator: it believes nothing is shown
while a dead modal owns the screen, and no later advance can repair it.
`window.defer` queues onto the same FIFO effect queue as `cx.emit`, so the
advance lands after the pop instead of underneath it. The paths that pop Root
themselves (`apply_retirement`, `handle_workspace_picked`) use the same
helper so there is one rule rather than two, and because deferring costs
nothing there. This is also exactly what the deleted `ModalAvailable` event
did — a queued hop from "the modal is free" to "show the Pi front" — with
the entity round trip removed, since one owner can simply call itself.

`release` names the clear-active-then-queue-the-advance pair, and is used
only where the two steps are adjacent (`forget_machine`,
`decline_forget_machine`, `cancel_add_repo`). The remaining paths keep the
steps spelled out because something order-sensitive sits between them:
`answer_front` sends Pi's answer before queueing the advance, and
`apply_retirement`/`handle_workspace_picked` must pop Root between clearing
`active` and deferring (see the ordering note on `apply_retirement` above).
Folding those into `release` would be an ordering change to a slice that has
never been exercised on screen, so it was deliberately not done.

`ModalOutcome` is the coordinator's one event seam to the shell: `RepoAdded`
routes the validated path back to the chat that asked for the walk, and
`MachineForgotten` tells the shell whose chat is now homeless. They were two
event types with two `Workbench` subscriptions; one entity emitting one enum
costs one subscription and lets the shell's `handle_modal_outcome` route by
match arm instead.

Add-repository specifics:
- `open_add_repo` creates the picker, subscribes to it, takes the modal, then
  opens the dialog and only then tells the picker to `start`. The send cannot
  live inside `cx.new` (the picker needs its own entity to exist before it can
  route an answer), which is the only reason `start` is a separate method.
- `handle_workspace_picked` uses the machine from `ActiveModal::AddRepo` and
  the path the picker validated; the chat that asked for the walk selects that
  path *whether or not the store write survived* — the folder is a valid git
  repo either way (validation already confirmed it), and a failed
  `workspaces.json` write is separately reported through the store's own
  failure event.
- `cancel_add_repo` invalidates the picker's in-flight host request
  (`picker.cancel`) before dropping it. Cancellation comes from Root with the
  walk still in flight; the success path deliberately does not cancel,
  because the pending request was already consumed by the validation that
  produced the event.

Forget-machine specifics:
- `NOTHING_IS_DELETED` — the reassurance shown on every removal
  confirmation. A missing machine is unreachable by definition, so this string
  is a statement of fact about what forgetting does, not a promise being made
  to the user. `FORGET_ACTION_LABEL` is "Remove" over a danger accent; the
  decline button takes the shared `ui::dialog::CANCEL_LABEL` rather than
  redeclaring the word, which this file used to do as `FORGET_CANCEL_LABEL`.
- `open_removal_confirmation` builds nothing itself — it fills in
  `ui::dialog::Confirmation` and hands `ui::dialog::open_confirmation` two
  entity callbacks, so the confirmation shares the app-wide dialog width for
  free by not having a width of its own.
- `forget_machine`: the store owns the entire cascade behind one
  `workspaces.json` write — tracking removal, the ghost sidebar row
  disappearing, and the machine's sessions vanishing all fall out of that
  single write. This flow's only remaining job is to release the modal and
  tell the shell whose chat (if any) is now homeless. A failed write reports
  itself (via the store's own failure event) and forgets nothing, but the
  confirmation still closes — leaving it open would only offer the user the
  same failing button again.
- `removal_message`: the session count quoted is *sidebar rows*, which is all
  that a missing machine's sessions can still be to this client — their actual
  transcripts live on a disk this client will never be able to reach again.

Both user-initiated flows silently ignore a second request while any modal is
open: Root holds one dialog at a time, and the one that is up already has the
user's attention.

## crates/harvester/src/workbench/picker.rs

The add-repository folder picker (docs/SPEC.md §Flows "Add repository"): the
walk state machine, the host ops that drive it, and its rendering. A child
module of the composition shell, paired with the `AddRepo` arm of
`workbench/modal.rs`, so the add-repository path owns itself end to end.

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

Copy constants, one per user-facing string (DESIGN.md §8):
`ADD_REPOSITORY_TITLE` ("Add repository"), `ADD_REPOSITORY_ACTION_LABEL`
("Add"), `ENTRY_FILTER_PLACEHOLDER` ("Filter…"), `FINDING_HOME_NOTE`
("finding home directory…") and `LISTING_DIRECTORY_NOTE` ("loading…"). The
Cancel label is *not* declared here — it is `ui::dialog::CANCEL_LABEL`, shared
with every other dialog in the app. The two `EntryRow::label` literals (`"../"`
and the trailing `/` on a directory name) stay literals on purpose: they are
path syntax, not prose, and naming half of a path idiom is worse than naming
none of it.

`PickerRequest` — the one host op a folder picker has in flight, and what its
answer needs to be applied. It is the `Pending<T>` context, not a duplicate of
the wire command: it survives to the answer, which never echoes it back.
Exactly one at a time: rows and the Add button ignore clicks while a request
is out, so the modal never has two answers to reconcile. The correlating token
belongs to `Pending`, not to this enum.
- `Listing { path }`: the path is the *request's* value, never echoed back
  from the answer — nothing comes back over the wire that the walk should
  trust as the path, so it must track its own intent.

`Walk::new` builds an idle walk; `FolderPicker::start` is what asks for
`$HOME` — no connection/daemon involved (SPEC §Workspace: "no daemon or
provider involvement"). This means browsing a machine Harvester never
bootstrapped costs one round trip, not an install. The ask and the send are
one step: a walk is only "busy" once a command has actually been minted, so
the modal can never render its waiting note over a request that was never
sent. `start` exists as its own method only because the send needs the
picker's entity, which does not exist yet inside `cx.new`; there is no staged
command and no take-dance behind it.

There is no `active` flag. Cancellation invalidates the outstanding token
(`Walk::stop_waiting`), which is what makes every later answer a no-op, and
the owning `ActiveModal::AddRepo` drops the picker immediately afterwards.

`Walk::ask`: every request goes through this one method, which is what makes
"one at a time, matched by its token" hold by construction rather than by
convention.

`Walk::stop_waiting`: invalidate the outstanding token and stop gating the
modal. It has two callers with two reasons — user cancellation
(`FolderPicker::cancel`, from Root's dismiss path) and a command that never
reached the worker at all (`send_request`'s send failure, without which the
modal would stay gated forever on an answer that is never coming). They used
to be two identically-bodied methods (`cancel`/`request_failed`); one method
with the reasons named at the call sites removed the standing question of
whether the two were ever meant to diverge. `Walk::fail` is `stop_waiting`
plus the reason to display.

`Walk::fail`: the reason a host op could not even be started is shown inside
the modal rather than as a toast/notification, because the user is already
looking directly at the picker. `Render` draws it with
`ui::inline_note(Tone::Failed, …)` with `overflow_hidden().text_ellipsis()`
chained at the call site — the same rendering as before, but the color now
comes from the tone vocabulary instead of `cx.theme().danger`, which is the
rule every other screen already followed (SPEC C11).

`render_location` was the last place outside `ui/` that read a *non-structural*
theme slot: it painted its caption `cx.theme().muted_foreground` directly. It
asks for `Tone::Muted.color(cx)` now — the identical colour, stated as a
meaning (DESIGN.md §3). It cannot use `ui::inline_note`, which is a leaf `div`
with one child, because this is a two-child flex row (an ellipsised path beside
an optional waiting note). Removing that read left `gpui_component::ActiveTheme`
unused in this file, which is the mechanical proof there is nothing else: the
whole module now names colours only through `Tone`.

`Walk::accept_home`: `$HOME` landing immediately becomes a listing request
for that same directory — the walk always starts by listing home. `None`
return means the answer was stale (token mismatch) or the exec failed, with
the failure reason left in `error` for the modal to show.

`Walk::accept_listing`: on failure (permissions, vanished directory), the
walk deliberately keeps showing the *previous* directory rather than clearing
state, and only surfaces the reason via `error`.

`Walk::accept_validation`: on success it reports the path it validated and
nothing else — the machine is the modal's own (it chose it), so
`WorkspacePicked` carries only the path. The walk is spent, and
`workbench/modal.rs` drops it. It takes the worker's answer as
`Result<(), String>`, matching the shape of the other three machines-worker
answers; it was an `Option<String>` meaning "the error, if any", which read
backwards next to its `Ok`/`Err` siblings.

`send_request`'s host lookup: `store.host_for` returns client-core's own
error type now, so the walk's displayed reason is `error.to_string()` at this
call site rather than a `String` the store demoted on its behalf.

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

`_list_events` / `_store_events`: one named `Subscription` field each, which is
how every view in the crate holds them. They were a `[gpui::Subscription; 2]`,
where the field name counted them instead of naming them.

`handle_store_event`: the picker's three answers (`HomeResolved`, `DirListed`,
`WorkspaceValidated`) are matched and delegated straight to their handlers.
There is no `PickerAnswer` carrier and no `window.defer`: that pair existed
only to move an owned answer into a deferred callback, and the reentrancy it
was guarding against does not exist — store events are dispatched after
`FleetStore`'s own update has returned (see `workbench.rs` above), so
continuing the walk, which usually sends the next host command through
`store.send_host_command`, is a plain call. The `BranchResolved` machines
event is explicitly ignored here — it belongs to the chat's branch indicator,
not the picker.

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

## crates/harvester/src/workbench/sidebar.rs

Sidebar presentation and intent handling. The sidebar observes `FleetStore`
directly and owns only its local view state: the two section-collapse toggles
and the selected-session highlight. It emits product intents carrying stable
values (not row indexes) so a store repaint racing a click can never cause the
click to land on the wrong session. Its one subscription is the named field
`_store_repaint` — same vocabulary as `workbench.rs` and `chat.rs`, which hold
a field of that exact name for the same `cx.observe`.

Layout, top to bottom: New session button, machines listing, sessions list
(docs/SPEC.md §UI "Layout"). Notably there is **no workspaces section** — a
repository is picked inside the chat that is about to use it, and
`workspaces.json` is that picker's backing store rather than being surfaced
as a sidebar citizen with its own list.

The New session button is a plain `ghost` `small` `Button`, sized to its own
content rather than stretched to the sidebar. It used to ask for `w_full()`
and `justify_start()`, and rendered neither: gpui-component's `Button` builds
its label as an inner `h_flex().size_full().items_center().justify_center()`
added as a child *after* `refine_style` applies the caller's style, so the
button's own justify never reaches the label — a full-width button rendered a
centered label while the code claimed a left-aligned one. Between honouring
the claim and honouring the pixels, the claim won, because a left edge shared
with the rows below it is the point. The cost is the full-width hit target; if
that turns out to matter, the honest way back is a `ui::sidebar_row` carrying
`Role::Button`, not a style the library will keep ignoring.

Row chrome is not here: `ui::sidebar_row`, `ui::sidebar_stacked_row` and
`ui::sidebar_empty_note` own how a row *looks* (see `ui/shell.rs`), and
`ui::collapsible_section` owns the two section headers, so this module is
store data → row content → intent and nothing else. The two collapse booleans
stay here because they are this screen's state; the header component is told
`expanded` and handed the listener that flips it.

`SessionRow` stays in this module on the same split: it is the mapping from a
`MachineSession` to what a row should say (name-or-sid title, "folder ·
machine" subtitle, status tone, selected/opening flags), which is screen
policy, not presentation.

The sidebar's width is `w_64` (16rem), not a `SIDEBAR_WIDTH` pixel const. The
const is gone for the reason every pixel const in the visual layer went: a rem
step tracks `BASE_FONT_SIZE`, so the sidebar's proportion to the text inside it
survives a change to the base size, and there is nothing left to keep in sync
by hand. The gallery's story sidebar uses the same `w_64` for the same reason.

Constants:
- `SID_PREVIEW_CHARS = 8` — enough of a uuidv7 to tell sessions apart at a
  glance when a session has no name.
- `FORGET_MACHINE_TOOLTIP = "Forget this machine"` — deliberately "forget"
  rather than "delete": forgetting only edits Harvester's local record.
  Nothing on the machine itself is touched, and (being unreachable) there is
  nothing reachable to touch anyway.
- `MACHINES_SECTION_TITLE` / `SESSIONS_SECTION_TITLE` ("Machines",
  "Sessions"), `NO_SESSIONS_NOTE`, `OPENING_SESSION_NOTE`
  ("opening…") — every user-visible string in this screen is a named
  const rather than a literal at its call site, which is the house
  no-magic-values rule applied to copy. The two section titles are sentence
  case because `ui::section_header` no longer uppercases what it is given (see
  `ui/shell.rs`); `OPENING_SESSION_NOTE` stays lowercase because it is a
  transient status fragment trailing a title, not a heading.

Two strings this screen renders are declared elsewhere and imported, because
they are the *same* words a second surface already owns:
- `MISSING_MACHINE_DETAIL` ("no longer listed") comes from
  `workbench/machine_presentation.rs` — the subtitle shown for a machine its
  provider stopped listing, which is also the only row kind that can be
  forgotten. That module is where `MachineState::Missing` becomes words, and
  it puts the same string in the statusbar.
- `NEW_SESSION_LABEL` ("New session") comes from `chat.rs`, which titles the
  pane this button opens. Both were declared twice, in one case as a const and
  in the other as a bare literal, which is a rename waiting to half-land.

Keyboard and accessibility, which is screen policy and therefore lives here
rather than in `ui/shell.rs`:
- Session rows are `Role::ListItem` with `aria_label` (the row title) and
  `aria_selected`, and they take the shared `keyboard_focus_ring`. They are the
  sidebar's only clickable rows, so they are its only tab stops.
- Machine rows opt *out* with `tab_stop(false)`. They are `disabled` — nothing
  happens when you click one — and a focus stop that leads nowhere is worse
  than no focus stop at all. They still carry `Role::ListItem` and an
  `aria_label`, because being unclickable does not make them unreadable.
- Machine rows pass `present_machine(...).tone` to `ui::sidebar_stacked_row`,
  which derives the dot's shape itself (`Tone::is_hollow`). Shape follows
  meaning, and there is only one place that decides it.
- The New-session button aligns to `ui::SIDEBAR_ROW_GUTTER` rather than a local
  `px_2`, so it sits in the same lane as the rows and the section headers
  below it, and the forget-machine button moved from `.xsmall()` to `.small()`
  with the rest of the chrome's density step.

`SidebarIntent` — what a sidebar click asks the composition shell to do,
captured from the rendered row so a later store repaint cannot move the click
to a different session:
- `NewSession`: carries nothing. Where the session runs is the chat's own
  question — the prompt strip picks machine and repository (docs/SPEC.md
  §Flows "Start session").
- `OpenSession { sid }`.
- `ForgetMachine { machine }`: only reachable after the shell has asked the
  user to confirm (docs/SPEC.md §Flows "Forget machine").

Which row is highlighted is read from the store (`is_selected_session`), not
held here. The sidebar cannot see the chat, and the highlight used to be a
copy of the chat's sid pushed in by the shell — the stale-projection shape
docs/SPEC.md C12 exists to kill. The store already knows the answer: a
session becomes current when this client attaches to it, which is the same
event that builds or promotes the chat. The one thing attachment cannot say
is "the user is composing a new session", so the shell clears the selection
when it opens an unstarted chat.

Deliberate consequence: while a chat is `Starting` (its sid minted, its
`StartSession` in flight) nothing is highlighted, because no session row
exists for that sid yet. That was true of the pushed mirror too.

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

A workspace's on-screen name — the path's last component — is
`harvester_client_core::folder_name`, not this module's own helper any more.
There were two of these: this one, which returned `""` for `/repo/` and for
`/`, and `ui/transcript.rs`'s `basename`, which trimmed the trailing slash,
special-cased root and had four pinned tests. A stored repo path ending in
`/` therefore rendered as an empty sidebar subtitle and an empty repo-control
label. The stronger body won, and it moved to client-core because `ui/` is on
the lib side of this crate and cannot see this bin-side module at all.

`store_path()` returns `io::Result` so an unset `$HOME` is one failure story
(`NotFound`, "$HOME unset") instead of an `Option` each caller re-explains:
`save` propagates it like any other write error, `load` logs it and degrades.
The `$HOME` check stays distinct from a missing *file* below — the file being
absent is a first run and silent, the home directory being absent is worth a
stderr line.

`load()` treats a missing file as an empty list (first run, not an error).
An unreadable or corrupt file also degrades to an empty list, logging the
reason to stderr rather than failing the caller — the next successful save
will rewrite the file whole, self-healing the corruption.

`save()` reports errors to the *caller* rather than stderr, because the
statusbar (not stderr) is the right place for the user to see a save
failure. The write itself stages to a sibling file and renames it into place
— the same atomic-swap pattern the daemon install script and the
bundled-provider installer use. Writing the real file directly would truncate
it before the new content is fully written, so a crash mid-write would leave
torn JSON; `load` would then read that torn file as an empty list, and the
*next* save would persist that emptiness over the user's entire workspace
configuration. Using a sibling file (not a `/tmp` file) keeps the rename on
one filesystem, which is what makes the rename atomic.

The write itself is `harvester_proto::replace_atomically(&path, &json, None)`
— the whole stage-then-rename dance in one place, shared with the
bundled-provider installer (which passes `Some(mode)`; this file wants the
default permissions, hence `None`). `save` keeps its own `create_dir_all`,
because the helper deliberately does not make parents.

Why that helper is written the way it is, restated here because this is the
call site the property matters for: the staged name is per-writer
(`workspaces.json.next.<pid>`, the same discipline the daemon install script
already had in shell as `.next.$$`), since a single fixed staging name is not a
staging file at all when two clients are open — each writes the *other's*
half-written bytes into the same path and one of them renames a file it did not
finish, which is the torn-JSON case the staging exists to prevent. The staged
file is `sync_all`ed before the rename: a rename is only atomic with respect to
*the directory entry*, so without the fsync a power loss can leave the entry
pointing at a file whose contents never reached the disk. When any step fails
the staged file is removed (quiet on `NotFound`, stderr otherwise) — unique
names mean a leaked one is leaked forever rather than being overwritten by the
next save. The test that pinned the staged name lives with the helper now, and
covers both spellings it has to reproduce (`…/workspaces.json` and the
extensionless provider path).

What this does **not** buy is multi-client safety: `Catalog` reads
`workspaces.json` once at startup, mutates its in-memory list, and rewrites
the whole file on every change (see the catalog section). Two clients open
at once are therefore last-write-win at the *catalog* level — one client's
"add folder" silently erases another client's "forget", because the loser is
writing a list built from a snapshot taken before the winner's change. The
single-writer assumption is deliberate for now (workspaces are personal
configuration a user edits from the window in front of them, and the VM's
disk — not this file — remains the authority on sessions); the atomicity work
above only guarantees that whichever list wins is a *complete, well-formed*
list. Reload-on-change (inotify, or a re-read + merge before each save) is
the fix the day two windows becomes a real workflow.

## crates/harvester/tests/fixtures.rs

The captured-pi regression suite: it folds each `.jsonl` under
`crates/harvester/fixtures/` through client-core's `Transcript` and asserts
what the renderer is entitled to assume. It lives in `tests/` rather than
inline because it walks the filesystem (see the rule in the `no_comments.rs`
section below); provenance for the fixtures themselves is in the
`crates/harvester/fixtures/ (provenance)` section above.

What each test pins, and why it is worth a test rather than a doc sentence:
- `every_fixture_folds_into_a_non_empty_transcript`: every capture ends
  settled and not mid-stream, so a fixture that was cut off mid-turn cannot
  quietly become the baseline for "this is what a transcript looks like". It
  also re-asserts the LF-only rule on the files themselves.
- `the_streaming_fixture_grows_its_answer_before_it_lands`: pi's
  `message_update` carries the *whole* partial message, so successive frames
  must be non-shrinking and the final assistant text must start with the first
  frame. This is the fixture-side half of the streaming rule in SPEC §Rendering
  pi, and the one that would catch a fold that appended deltas instead of
  replacing.
- `the_tools_fixture_covers_every_label_path`: the capture is required to
  exercise each built-in tool name the header policy has a builder for, plus at
  least one unrecognized name (the fallback), one failed call, and both
  line-stat sources (an `edit` diff and a `write` body). It is a coverage
  assertion about the *fixture*, so shrinking the capture cannot silently
  un-cover `ui/transcript.rs`'s summary branches.
- `the_mixed_fixture_interleaves_tools_and_markdown_answers`: pins that the
  capture actually alternates messages and tool blocks in both directions, and
  carries fences, headings and numbered lists — the markdown surface the
  transcript renders.

`BUILTIN_TOOL_NAMES` is the same list `ui/transcript.rs`'s `tool_summary`
switches on; it is duplicated here on purpose, as the thing being checked
rather than the thing doing the checking.

`fixture_events` splits on `harvester_proto::LF` and asserts against
`harvester_proto::CR` rather than on bare `'\n'`/`'\r'` literals: it is
reading pi JSONL, so it is bound by the same LF-only framing rule as every
other reader in the tree, and the consts are where that rule is written down.
The gallery reads these same three files with its own loader
(`harvester-gallery/src/fixtures.rs`) — the two are not shared because the
gallery is out of the workspace and nothing importable sits below both, so
the framing consts are what keeps them from disagreeing. The remaining
duplication is recorded in the gallery's own notes.

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

Skipped directories (`is_skipped_directory`): `target` (build output) at any
depth, and `bin` **only directly under a crate's `src/`** — that is the
Cargo-defined home of spike binaries, and throwaway code is exempt from house
rules. The parent check is the whole point: matching the bare name `bin` at
any depth would silently exempt any future `src/anything/bin/` module from the
lint. The scan roots at the parent of `CARGO_MANIFEST_DIR`, i.e. `crates/`, so
every crate is covered from this one test regardless of which crate hosts it —
including `crates/harvester-gallery`, which is *not* a workspace member. The
walk is over the filesystem, not over `cargo metadata`, and that is what lets
the gallery's quarantine (see `docs/internals/harvester-gallery.md`) keep the
lint instead of escaping it.

The scanner is a `Scanner` struct (characters, cursor, line) whose `advance`
is the *only* place the cursor moves, and it counts every LF it steps over.
That centralization is the point: the previous shape — free functions
threading `&mut usize` line counters and returning new indices — required
each helper to remember to count its own newlines, and one once forgot the
string-literal line-continuation escape (backslash immediately followed by
LF). Miscounting does not change *whether* a comment is found, only the line
number reported for it — and every offender after the miss drifts by one,
which is exactly the kind of quietly-wrong output that wastes a reader's
time. With `advance` counting uniformly, that bug class is structural rather
than remembered (pinned by
`a_line_continuation_inside_a_string_still_counts_its_newline`).

## Recorded gaps (visual layer)

Known and accepted limitations that cut across modules; the gallery keeps its
own list under its section above. Each of these is a decision to *not* fix
something yet, with the reason it was not worth fixing blind.

- **The send button has no accessible name.** `Button` sets `aria_label` from
  its own label and from nothing else, and the send button is icon-only.
  Nothing can be done about it from this side: `aria_label` lives on
  `StatefulInteractiveElement`, which `Button` does not implement, and gpui's
  interactivity aria field is crate-private. Wrapping the button in a labelled
  `div` is theatre rather than a fix — a div with no role is filtered out of
  the accessibility tree, and a wrapper *with* a role is not the focusable
  node, so an assistive-technology user still lands on an unnamed button. The
  real fix is upstream.
- **Two optical insets drifted when the controls stepped up a size.** Both the
  prompt strip's controls row and the sidebar's New-session button were solved
  against the library's *extra-small* button padding and the sidebar's older
  row padding respectively. Both controls are `small` now and the row padding
  grew, so in each case the control's leading content sits a quarter-rem
  inboard of the text it is supposed to align with. The rule in both places is
  still "the row's own inset plus the library's padding equals the neighbour's
  inset"; what is missing is re-solving it, which is one step in each file and
  wants an eye on the rendered result rather than arithmetic alone.
- **`KeyboardFocusRing` has no inset variant.** A control inside a clipping
  container cannot use it, because an overflow-hidden parent masks a shadow
  drawn outside the child's box. The tool block header therefore hand-rolls the
  equivalent as a permanently-reserved transparent border recolored on
  `focus_visible`. One trait with an inset mode would let it share the app's
  one ring instead.
- **A long transcript is a long tab sequence.** Every tool header is a tab
  stop, which is what "every action is reachable by keyboard" demands and what
  chat UIs generally do, but the better end state is the transcript as a list
  with arrow-key traversal behind a single tab stop. That needs an owner for
  keyboard navigation across the whole shell, not a change inside one module.
- **Two ellipses are standing in for an indeterminate progress indicator**: the
  prompt strip's "Starting session…" and the streaming turn's bare "…"
  placeholder. Decorative dots are the weaker answer, but this revision of
  `gpui-component` ships no indeterminate indicator, and `inline_note` has no
  leading slot to put one in. Whoever adds the slot should retire both dots in
  the same change.
- **The select dialog is not searchable.** With a hundred options from a Pi
  extension, filtering would help; the library's list state has the mechanism.
  It is not done because a search input takes focus and changes what Enter
  means inside a dialog whose whole contract is that Enter answers Pi — which
  is a change to verify against a real Pi dialog, not to reason about.
- **The dialog geometry has never been seen driven by a real Pi.** The option
  row height, the three width rungs, and a virtual list inside an auto-height
  dialog body were all chosen by construction. They want one look at a genuine
  `select` from a real extension on a throwaway VM before they are treated as
  settled.
