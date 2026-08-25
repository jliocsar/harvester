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
- `Chat` builds no prompt-strip chrome of its own. `render_bottom_row`
  assembles a `ui::PromptStrip` value (labels, enabled flags, rows, branch,
  `can_send`, `focused`, notice) plus one `send` closure, and hands both to
  `ui::prompt_strip`. Everything visual — the ids, the icons, the "disabled or
  empty means no popover" rule, the notice's tone — lives in `ui/prompt.rs`;
  see that section for the contract. `chat.rs` keeps only what needs the store,
  the state machine or an `Entity`.
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
- `model_control` / `machine_control` / `repo_control` still build rows for
  a control that is disabled (a live session's machine and repository). That
  is a Vec of labels per repaint, deliberately traded for one code path; if a
  control ever grows an expensive row list, laziness belongs here in the
  caller, not back in three copies of the render tail.
- The lists are built even when a notice short-circuits the strip. They are
  pure store reads, and the alternative — an early return in `chat.rs` —
  would put the notice decision in two places, which is exactly what
  `PromptStripNotice` exists to prevent.
- `is_unstarted` is named that way rather than `unstarted` only because
  `Chat::unstarted` is already the constructor for a chat with no session
  behind it.
- The models-failed line is `ui::inline_note(tone, …)`, the same primitive
  the folder picker's error uses — a tone-colored `text_sm` note. It keeps
  its original wrapping behavior: a failure detail is information, and
  truncating it would hide the reason it exists to show. The exit/starting
  notice is now the same primitive reached through
  `ui::prompt_strip`.

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
  session reachable. Its ellipsis is the conventional one — the row opens a
  dialog rather than committing anything — unlike a progress ellipsis, which
  the app spends far more carefully.
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
  control capped at `max_w_56` would ellipsize away exactly the end that
  identifies it. The dropdown's own rows still carry the full paths, which is
  where two same-named checkouts are told apart.

## crates/harvester/src/desktop_shell.rs

What is *product-specific* about starting the Harvester application: the
window title, the window size, and the fact that the root view is a
`Workbench`. Everything generic about starting a gpui application (fonts,
assets, theme, quit binding, `Root`) moved to `shell_bootstrap` on the lib
side when the gallery binary appeared — see that section for the contract.
This module is the app-only half that stayed on the bin side, because
`Workbench` and everything under it is bin-only.

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
- `open_option_list` / `open_question` / `open_value_entry`: the whole Pi-facing
  half of dialog chrome — each takes the shared answer callback and hands
  `ui::dialog` two plain callbacks, translating a click into the right
  `DialogAnswer`. `ui::dialog` therefore never sees a `DialogAnswer`,
  a `DialogKind`, or anything else from `harvester-client-core`.
- Titles: `open` no longer computes one fallback title for every kind. Each
  arm names its own condition — `UNTITLED_SELECT_CONDITION` ("Choose an
  option"), `UNTITLED_INPUT_CONDITION` ("Enter a value"),
  `UNTITLED_EDITOR_CONDITION` ("Write a response"),
  `UNTITLED_CONFIRM_CONDITION` ("The agent needs a yes or no") — and hands it
  to `ui::dialog::title_or_named_condition`. This is why `Input` and `Editor`
  stopped sharing a match arm: they take the same *chrome* but they are not
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
  this client owns are the four untitled conditions and the two confirm button
  labels, and nothing else.
- The machine and session id are both available on the obligation at this point
  and are deliberately kept out of the title. The dialog is modal over a
  workbench that already shows which session is attached, and a title that
  restates the surrounding window is spending the user's most valuable line on
  context they can already see.
- `open_question`: dismissal (Escape / close) answers "no" rather than
  cancelling outright, because Pi resolves a cancelled `confirm` to `false`
  anyway — so treating Escape as a refusal loses no information and removes
  one more way for the agent to hang waiting for an answer. Its labels are now
  `CONFIRM_ACTION_LABEL` ("Confirm") / `CONFIRM_CANCEL_LABEL` ("Cancel"), not
  "Yes"/"No". They still pass through `Confirmation` rather than being baked
  into the shared chrome — the forget-machine flow needs "Remove"/"Cancel" —
  but Pi's own vocabulary is no longer what they say: "Yes"/"No" only reads
  correctly under a question, and a Pi `confirm` title is very often a
  statement. The wire settles the argument too: `DialogAnswer::dismissal_for` a
  `confirm` is `Confirmed(false)`, so the decline button, Escape and an overlay
  click all send exactly the same thing — calling that path "Cancel" makes the
  button say what Escape does, where "No" implied two distinct commitments over
  a protocol that has one commitment and one non-commitment.
- `open_value_entry` owns the "no input, no dialog" guard itself rather than
  the caller unwrapping first. Both `Input` and `Editor` reach it, and putting
  the guard in the callee is what let those two become separate match arms
  without duplicating the check.
- `open_option_list`: the option's *text itself* is the answer; Pi has no
  notion of an index here, so the chosen label round-trips straight back as a
  `DialogAnswer::Value`.

## crates/harvester/src/gallery/ — the component gallery

A dev-only second binary (`cargo run -p harvester --bin gallery`) that renders
Harvester's `ui` components in isolation, Storybook-style. Declared as an
explicit `[[bin]]` at `src/gallery/`, deliberately **not** under `src/bin/`:
the `no_comments` test skips any directory named `bin` (throwaway spikes such
as `bin/uispike.rs` were always exempt from house rules), and the gallery is
real, maintained code that must obey them. Putting it one directory over is
the whole trick.

Module map:

| module | role |
|---|---|
| `main.rs` | bootstrap, the `Gallery` root view, all mutable gallery state, replay scheduling |
| `chrome.rs` | the gallery's own screen: story sidebar, stage, control dock, statusbar |
| `story.rs` | the story/knob/preset model (pure data, no rendering) |
| `registry.rs` | the one hand-written list of stories |
| `stories/` | one module per `ui` submodule, each holding that group's render functions |
| `runtime.rs` | the live state stories share with the gallery (inputs, transcripts, replay) |
| `fixtures.rs` | runtime loading of `fixtures/*.jsonl` and folding them into `Transcript`s |

### The gpui-mcp bridge

`main.rs` calls `gpui_mcp_bridge::install(cx)` inside the `run_desktop_shell`
closure. This lets the gpui-mcp MCP server attach to a running gallery and
drive it — dump the accessibility tree, click via AT-SPI, inject keys — which
is how agents UI-test the gallery. Deliberately gallery-only: the client
binary never calls `install`, so shipping builds carry no bridge socket. The
bridge itself is inert unless the process is a debug build launched with
`GPUI_MCP=1`, so plain `cargo run --bin gallery` is unaffected. The bridge's
gpui dependency uses the same bare zed-git source as ours and both lockfiles
pin the same commit; a host on a mismatched gpui build is caught at attach
time by the bridge's `hello` handshake, not at runtime by silent weirdness.

`chrome.rs` carries the a11y adoption that makes the gallery agent-navigable:
a node reaches the accessibility tree only when it has **both** `.id()` and a
`.role()` (gpui rule; verified — without roles the whole gallery projected as
one Window node). The sidebar is `Role::List`, story rows `Role::ListItem`
with `aria_label(name)` + `aria_selected`, each knob a labelled `Role::Group`,
the stage a labelled `Role::Group`. gpui-component's `ListItem` implements
`StatefulInteractiveElement`, so the aria builders chain straight through it.
Two of these moved *out* of the gallery since: group and dock headers no longer
re-apply `Role::Button`/`aria_expanded` because `ui::collapsible_section` sets
them itself now (the gallery applying them a third time is what proved they
belonged in the component), and the bespoke chips that carried
`aria_selected` are gone entirely — a `ButtonGroup` and a `Button`
bring their own a11y. Story *content* is therefore no longer uniformly
invisible: whatever a11y a gpui-component widget has upstream now shows up in
a story that renders it. `scripts/gallery_walk.py` drives every story and a knob
toggle over the MCP as the regression check. Its selectors track the
accessibility tree, so a component swapped for a different library widget
breaks them: story rows are `ListItem`s reporting `sel`, knob controls are
`ButtonGroup` `Button`s reporting `toggled=`.

Two things about the walk are worth knowing before changing it. It expands
exactly one story group at a time and collapses the rest, because seventeen
stories plus seven headers do not fit in a 720px window and the MCP has no
scroll tool — its seven tools are `launch`, `attach`, `snapshot`, `detail`,
`click`, `input`, `status`, and the refusal for an off-screen node tells you
to bring it into view yourself. Walking group by group keeps every row on
screen by construction, so it does not care how tall a row gets. And every
click waits out `CLICK_SETTLE_SECONDS` first: two synthetic clicks in quick
succession arrive as a single multi-click and the second is silently
swallowed, which reads as "the click succeeded but nothing moved" and is very
hard to tell from a broken component. The knob search is scoped to nodes
after the dock header for the same class of reason — a story that happens to
render a toggled `Button` on the stage is not a knob.

Feel: story switching and knob changes are the two things a user does dozens
of times a minute, so both are **instant** — no animation anywhere in the
gallery chrome. The only motion in the binary is the streaming replay, which
is the subject of a story rather than a transition. Dark theme only (see the
recorded gap below), `cx.theme().font_size` at the root (the same slot the app
root reads — the gallery must never set its own text size, or a component
would be catalogued at a scale no real pane gives it), every color from `Tone`
or a theme slot, and the chrome itself is built from `ui::` components
(`sidebar_row`, `collapsible_section`, `chat_header`, `statusbar_frame`,
`inline_note`, `KeyboardFocusRing`) — the gallery dogfoods the library it
catalogs.

### crates/harvester/src/gallery/story.rs

The model, deliberately data-only so the registry reads as a table:

- `KnobValue` is the value union (`Bool` / `Choice(usize)` / `Text(String)`)
  and `KnobKind` is the *shape* (`Choice` carries its labels). They are
  separate types because a spec needs the labels once while a value is copied
  per story per frame; folding the labels into the value would clone them on
  every knob read.
- `KnobValues` is an association list, not a `HashMap`: knob counts are
  single-digit, order is the declaration order the dock renders in, and the
  lookups (`flag`, `choice`, `text`, `optional_text`) all degrade to a default
  instead of panicking, so a story that reads a knob it never declared renders
  rather than crashes.
- `Preset` is *a set of knob values with a name* — nothing more. That is what
  makes presets and knobs commute: clicking a preset button writes into the
  same `KnobValues` a knob control writes into, so a preset can always be
  adjusted afterwards. Presets set every knob they care about explicitly rather than
  diffing against a base, so preset order never matters.
- `Story` is name + group + knobs + presets + a plain `fn` pointer. A `fn`
  (not a boxed closure) is what keeps the registry a static list: a story
  cannot capture state, which is exactly the property that forces shared live
  state into `StoryRuntime` where the gallery can observe it.
- `StoryScene` is what the render `fn` receives: the resolved knob values plus
  the `Entity<StoryRuntime>`. This is the one deviation from the phase plan's
  sketched signature (`&KnobValues` alone): live stories (collapse, tool-block
  expansion, dropdown selection, replay, the real `InputState`) need state
  that survives across frames and can notify the view, and in gpui that means
  an entity handle, not a value.

### crates/harvester/src/gallery/registry.rs

`build_stories()` is the whole catalog: one `vec![]` of `Story::new(...)`, no
macros (a macro would hide exactly the thing a reviewer wants to read). Group
names are consts (`CORE_GROUP` … `DIALOG_GROUP`) because they are also the
sidebar's section titles and `collapsible_section` demands `&'static str`.
They are sentence case ("Core", "Prompt", "Flagship") rather than the lowercase
they used to be: `section_header` stopped uppercasing its title, so a lowercase
group name now renders lowercase, and a heading is user-visible text under the
app-wide wording convention. Knob and preset names stay lowercase — those
mirror code, not prose. *Story* names are deliberately not prose either:
`inline_note`, `sidebar_stacked_row` and `chat_header` are Rust identifiers,
the exact symbol a reader is being sent to go and read, and sentence-casing
them into English would break the one thing the catalog is for. Preset labels
stay lowercase for a different reason — they name the scenario a command
produces ("running session", "fast forward"), which is the register a gallery
wants. The gallery's `aria_label`s ("stories", "stage") are likewise kept
verbatim: they are accessibility-tree selectors that agent tooling keys on, not
visible copy to be styled.

- `STREAMING_TRANSCRIPT_STORY` is the replay story's name, and
  `story_index()` resolves it to an index once at start-up. The replay loop
  keys on that index instead of a name comparison per tick, and if the story
  is ever renamed the lookup returns `None` and the replay simply never runs —
  no panic, no silent tick on the wrong story.
- `group_names()` derives the sidebar's group order from the story order, so
  adding a story to a new group needs no second edit.
- `LIVE_TONE_INDEX` / `FAILED_TONE_INDEX` name the two `TONE_LABELS` positions
  the presets reach for; `DEFAULT_SPEED_INDEX` is the `1x` slot of
  `REPLAY_SPEED_LABELS`.

### crates/harvester/src/gallery/stories/

One module per `ui` module plus the three flagships, each exporting its knob
*names* as consts so `registry.rs` and the render function cannot disagree
about a string. `stories/mod.rs` holds what every story shares: `TONES`
(the tone order the whole gallery indexes into), `TONE_LABELS` derived from it
through `Tone::label`, and `tone_at` (the choice-index → `Tone` mapping used by
six stories). `TONE_LABELS` used to be its own lowercase list, which made it a
second tone vocabulary that had already drifted — it said "muted" and "active"
where the app says "Neutral" and "Running", so the catalog was labelling tones
with words no screen uses. `Tone::label` is a `const fn` so the derived list
stays a `const` and the two can never disagree again. Also here are
the layout primitives `story_column`, `story_row`, `caption`
(an `inline_note`, so captions obey `Tone`), `panel` and `transcript_panel`.
`panel` sits on the `popover` slot with `rounded_lg` and `shadow_sm` — the
raised-surface tier of the graphite ladder — so a story's framing matches
how real raised components (tool cards, popovers) sit on the canvas.
`PANEL_MIN_WIDTH` (320) keeps a one-line component from collapsing to its
text width on the stage; `TRANSCRIPT_PANEL_HEIGHT` (420) gives the transcript
stories a fixed viewport so their internal scroll behaves like the chat's.
`dot_at` is gone with the per-call-site shape choice it existed to serve.

- `core.rs`: the tone swatch grid (all six tones × the derived dot × the
  tone's own `label()` × colored text, `w_20` lanes aligning both label
  columns) and the single-dot story. Neither story picks a dot shape any more:
  the swatch shows the one dot `Tone::dot` derives, and the dot story *reports*
  the derived shape (`derived_dot_label`) instead of letting a knob set it —
  a catalog whose controls can produce a combination the app cannot render is
  lying about the component.
- `note.rs`: `inline_note` and `empty_pane_hint`; the hint gets an `h_48` box
  because it is a `size_full` component and would otherwise have no height to
  fill on the stage, and it is rendered *with* an `empty_pane_action` child so
  the story shows the trailing action slot rather than only the sentence.
- `shell.rs`: `chat_header`, `collapsible_section` (live — the expanded flag
  lives in `StoryRuntime`, so the story demonstrates the real click), and
  `statusbar_frame`. Section titles come from a *choice* knob rather than a
  text knob because the component's signature is `&'static str`.
- `sidebar.rs`: the three sidebar builders inside a `w_64` frame tinted with
  the sidebar theme slots (the app's own sidebar width — the gallery should not
  invent a second sidebar metric), because a row's selected state is only
  legible against the surface it really sits on. Both row stories chain
  `keyboard_focus_ring` so the story can be Tab'd into, and `row` mirrors the
  app's `tab_stop(!disabled)` rule. The stacked-row story carries a caption
  saying the dot shape is derived from the tone rather than chosen per row,
  because that is the question the retired `solid` knob used to answer.
- `prompt.rs`: `prompt_box_control`, `prompt_box_send` and a live
  `prompt_box_dropdown`. The dropdown is the component's real `PopupMenu` —
  arrow keys, Enter and Escape all work — and since picks now travel as
  actions, the story listens with `on_action(|action: &PickModel, …|)` on its
  own column and writes `action.index` into
  `StoryRuntime::dropdown_selection`. It passes `None` for the action context,
  which is the correct shape for a story: there is no prompt input to scope
  dispatch to, so the action rises to the listener the story just installed.
- `prompt_strip.rs` (flagship): builds the whole `PromptStrip` value struct
  from knobs, with the six presets from the plan (`ready`, `loading models`,
  `models failed`, `no machines`, `starting session`, `exited`). The dropdown
  rows are static label lists and `send` is a no-op closure: the story exists
  to show every *state* of the strip, and a working picker would need a store.
  It installs no `on_action` listener either, so a pick opens and dismisses the
  real menu and changes nothing — which is the honest rendering of a strip with
  no chat behind it. The input is a real `Entity<InputState>` from the runtime,
  so typing, focus ring and auto-grow are the genuine article.
- `transcript.rs` (flagships): the rendered story folds a fixture and renders
  it through `TranscriptPresentation` with live tool-block expansion, and its
  caption says out loud that a tool header is a tab stop — the story is the one
  place a reviewer can discover that without reading the code; the
  streaming story renders the replay transcript plus its control row (restart
  button, `applied / total` progress, and the paused/finished note). Play and
  speed are knobs; restart is a button because it is an action, not a state.
- `dialog.rs`: `Confirmation` + `button_row` + the `select`
  option list + both input kinds, rendered **inline on the canvas**, never
  through `window.open_dialog`. See the recorded gap below. Each surface is
  built at its real modal width by reading `ui::dialog`'s own
  `*_DIALOG_WIDTH_IN_REMS` consts, and the option list at `options_height` with
  real `option_row`s — the whole point of the story is the width ladder, so a
  story that guessed the widths would be showing you the wrong thing. The
  untitled-confirmation knob path goes through `title_or_named_condition` with
  the same "The agent needs a yes or no" condition `dialogs.rs` uses. The
  option list is painted as a plain column of `option_row`s rather than as a
  live `List`, with one row forced selected so the selected style is reviewable:
  a story render function runs on every frame and cannot own a list state
  without putting one in `StoryRuntime`, and a list state rebuilt per frame
  could never hold a selection to show. The rows are the dialog's own builder,
  so what the story shows is honest; what it cannot show is scrolling and
  keyboard traversal.

### crates/harvester/src/gallery/runtime.rs

`StoryRuntime` is the single entity holding everything a story needs to
survive a frame: the collapsible section's flag, the dropdown's selection, the
prompt strip's `InputState`, the dialog story's two inputs, the rendered
transcript, and the replay. The gallery `cx.observe`s it once, so any story
that calls `runtime.update(…, cx.notify())` re-renders the whole gallery —
that one subscription is why story render functions can stay plain `fn`s.

- `RenderedTranscript` caches the folded fixture keyed by `source_index`;
  `ensure_rendered_source` rebuilds only when the source knob actually
  changes, so switching *any other* knob doesn't re-parse a fixture, and it
  resets the expansion set with it (a tool-call id from another fixture would
  otherwise leave a random block open).
- `ReplayTranscript` owns its own copy of the streaming events, its cursor and
  its transcript. `restart()` rebuilds the transcript from scratch instead of
  rewinding, because `Transcript` has no undo — replay from event zero is the
  only faithful reset.
- Replay timing consts: `REPLAY_FRAME_INTERVAL_MS` (260) is the wall-clock gap
  between two fixture events at 1×; it is a *display* cadence, not the
  capture's real timing (pi's provider batches partials, so the fixture's own
  spacing is bursty). `REPLAY_SPEED_MULTIPLIERS` (0.5/1/4) divide it and
  `REPLAY_SPEED_LABELS` are the same three values as knob labels — the two
  arrays are index-parallel by construction. `REPLAY_IDLE_POLL_INTERVAL_MS`
  (200) is how often a *paused* or unselected replay re-checks whether it
  should start again; a paused loop must keep breathing because the knob it
  waits on lives in the gallery, not in the task.
- `PROMPT_INPUT_ROWS` (1–8) and the dialog placeholders mirror the app's own
  values so the story is the real component, not a lookalike.

### crates/harvester/src/gallery/fixtures.rs

Fixtures are loaded **at runtime**, from
`env!("CARGO_MANIFEST_DIR")/fixtures/*.jsonl`, not `include_str!`'d. The
gallery is a dev tool that only ever runs from this repo, and runtime loading
means capturing a new fixture is visible on the next `cargo run` without a
rebuild — and, more importantly, a missing or malformed file degrades instead
of failing the build for everyone.

- Every failure path produces a `String` naming the offending path (and line
  number for a parse failure) that the story renders through
  `ui::empty_pane_hint(Tone::Failed, …)`, and the statusbar carries the first
  failure of any fixture at all times. Nothing panics: the whole point of a
  catalog is that the other 16 stories still work when one fixture is gone.
- Parsing follows the house LF-only framing rule (`harvester_proto::{LF, CR}`,
  split on `LF`, strip one trailing `CR`), the same as everything else that
  reads pi JSONL.
- **The user-prompt rule**: `Transcript::apply` deliberately ignores
  `message_end` events whose `message.role` is `"user"` — in the app, user
  text enters a transcript only through `push_user_prompt`, because the app
  already knows what the user typed. A replayer has no such knowledge, so
  `apply_event` intercepts exactly that event shape and calls
  `push_user_prompt` with the concatenated text parts. Without it the mixed
  fixture renders assistant turns and tool blocks with no prompts above them.
  This is the one place the gallery is allowed to interpret a pi payload, and
  it interprets it into the same call the app makes.
- `TranscriptSource` maps the four preset/knob choices onto fixtures:
  `markdown-heavy answer` → `streaming.jsonl`, `tool run` → `tools.jsonl`,
  `mixed session` → `mixed.jsonl`, and `failed tool` → `tools.jsonl` filtered
  to the tool-call ids whose `tool_execution_end` carried `isError: true`.
  The filter exists so the error path gets a story of its own without a
  second capture: it keeps every event that names a failing call and drops
  the rest, which is enough for the block to render exactly as it does in a
  full session.

### crates/harvester/fixtures/ (provenance)

Three checked-in pi session captures drive the transcript stories and
`tests/fixtures.rs`. Each file is LF-only JSONL, one **raw pi event object**
per line — the inner event exactly as `Transcript::apply` consumes it, never
the harvesterd envelope.

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

### crates/harvester/src/gallery/main.rs

`Gallery` owns all mutable gallery state: the story list, one `KnobValues` per
story (so switching stories never loses your knob edits), the collapsed group
list, the selected story, the dock flag, the runtime entity, and the text-knob
inputs.

- `GALLERY_WINDOW_WIDTH`/`HEIGHT` (1100×720) match the app's window rather
  than being sized to the catalog, so a component rendered in a story is laid
  out at the width it will really get. They stay bare pixels while the rest of
  the gallery moved onto the rem scale, because they are the OS window's
  physical bounds rather than layout — the one place a pixel is the honest
  unit.
- Text knobs are backed by real `Entity<InputState>`s keyed by
  `(story_index, knob_name)` and created lazily in `sync_text_knob_inputs`
  during render (the first frame that shows the knob is the first frame that
  has a `Window`). Each gets a `cx.observe` subscription, which is what makes
  typing in a knob update the story on the same keystroke.
  `selected_knob_values` then reads the *input* rather than the stored value
  for text knobs — the input is the source of truth while it exists, so there
  is no mirror to keep in sync. Applying a preset writes both, since that is
  the one direction the input cannot learn about by itself.
- Replay scheduling: `schedule_replay` spawns one `cx.spawn` loop for the
  gallery's lifetime. Each iteration reads the delay *before* sleeping
  (`replay_tick_delay`), so flipping the speed knob takes effect on the next
  tick rather than at the end of a long sleep, then advances exactly one
  event. The loop returns when the entity is gone (`update` returns `Err`),
  which is the task's only exit — there is no cancel flag to keep in sync.
  `replay_is_playing` gates on three things: the replay story is the selected
  one, its `playing` knob is on, and the replay has not finished. Gating on
  selection is what keeps a background replay from silently running to
  completion while you are reading another story.
- The knob-name const `PLAYING_KNOB`/`SPEED_KNOB` import is the only place the
  gallery shell knows anything about a specific story; it is the price of
  driving a story's animation from the shell's async task.

### crates/harvester/src/gallery/chrome.rs

The gallery's own screen, kept out of `main.rs` so state and layout stay
separable. It is a sibling module of the crate root, which is why it can read
`Gallery`'s private fields through accessors without those fields becoming
`pub`.

- Layout: the story sidebar spans the full window height; stage, dock, and
  statusbar stack in a column to its right, so the dock and statusbar read as
  controls *for the canvas*, not window-level chrome. The sidebar is `w_64`
  with `min_w_56`/`max_w_80` and is allowed to shrink (`flex_shrink_1`) — the
  `w_64` matches the app's sidebar exactly, because the gallery should not
  invent a second sidebar metric, and the bounds are what keep a narrow window
  squeezing the sidebar rather than the stage. `STAGE_MEASURE_IN_REMS` (64) is
  the only measure left as a named const here: it caps the stage's content
  column so a component never stretches to a width no real pane would give it,
  and it is in rems so that cap scales with the type it is capping.
- Each region is described by three constraints — a minimum, a comfortable
  default, and a rule for surplus — rather than by one number back-solved from
  a screenshot. The sidebar's minimum is the width at which the *task* still
  works: below it the two transcript stories truncate to the same string and
  picking a story stops being possible. Its default is comfortable for the
  longest name in the catalog. Its surplus rule is `flex_grow_0`, meaning every
  extra pixel goes to the stage — which is what "visibly subordinate to the
  work area" means in layout terms — while `flex_shrink_1` makes it yield down
  to its minimum before the stage gives up anything. `max_w_80` is inert at the
  current default and exists to bound the region if it is ever given a basis of
  its own. The stage yields nothing and absorbs surplus up to its measure,
  after which the canvas centers and the extra becomes symmetric gutters. The
  dock is `flex_none`: it never grows, and the knob grid scrolls inside
  `max_h_56` instead — a bound deliberately set just under a quarter of the
  default window height, because the controls must never outweigh the thing
  being controlled. `min_w_0` on the right-hand column is a flexbox requirement
  so descendants can ellipsize, not a design constraint.
- `STAGE_MEASURE_IN_REMS` is chosen as **four times the sidebar's preferred
  width**, not as a translation of the pixel constant it replaced: the work
  area reading as four sidebars wide is the relationship the proportion is
  actually about. It is written as `rems(...)` rather than a `max_w_*` helper
  because gpui's box scale tops out well short of a canvas measure; it rides
  the same zoom axis either way.
- **No resizable split**, and this was considered rather than overlooked. The
  library ships one. A draggable split earns its state when the two regions
  hold content the user must trade off against each other; the story list is a
  fixed, bounded catalog of known names, so there is no more or less of it to
  want. A handle plus a resize entity in `Gallery` would add state and an
  affordance that answers no question. Revisit if story names ever become
  user-authored.
- The text knob's `w_56` fits roughly thirty characters, which covers every
  default knob string in the registry except the long dialog message (the
  `Input` scrolls that one), and it is also what keeps three to four knobs on a
  dock row at the default window width instead of collapsing the grid into a
  column.
- Every other former sizing const is now a rem step read straight off the
  library scale: the knob grid caps at `max_h_56` (it must not eat the stage
  when a story has eleven knobs — the prompt strip does — and scrolls past
  that), a text knob is `w_56`. `STORY_SIDEBAR_WIDTH`, `STAGE_MAX_WIDTH`,
  `DOCK_KNOBS_MAX_HEIGHT` and `TEXT_KNOB_WIDTH` are all gone. Naming a pixel
  number that a rem step already names is a second source of truth for a
  measurement, and the gallery had four of them.
- `with_content_inset` / `with_dock_row_inset` are generic over `Styled` rather
  than being consts, because the thing being shared is a *decision* (this is a
  content edge / this is a dock row) applied to differently-typed elements —
  a `Div`, a `Stateful<Div>`. Dock rows share one vertical rhythm through the
  latter: presets, the no-presets/no-knobs captions, and the knob grid are all
  `px_3 py_2p5` under the shared section header, so the dock's left edge and
  row spacing never jitter as stories swap knob sets in and out. `py_2p5` is
  the density step the rest of the chrome took; at `py_1p5` a dock row of
  medium-tier controls had less air than the controls themselves. Four sibling
  regions used to carry four leading edges here; the stage canvas, the preset
  row, the knob grid and both empty notes now all resolve from the one
  function, and `chat_header` / `statusbar_frame` land on the same edge by
  taking the same step directly. The stage canvas keeps a taller *vertical*
  rhythm (`py_4`) on purpose — it is a canvas, not a row, and only the leading
  edge is the shared decision.
- **The gallery has no bespoke widgets left.** The chip — a hand-rolled
  `div` with its own `primary`/`secondary` fills, hover and press states, and
  `text_xs` — is gone, along with `BOOL_ON_SLOT`/`BOOL_OFF_SLOT`, the id slots
  its on/off pair occupied. A bool knob and a choice knob are both a
  `ButtonGroup` of selectable `Button`s, a preset is an `outline` `Button`,
  and a text knob is an `Input` with an `aria_label`.
  All of them sit at the library's medium default: no `.small()`, no
  `.compact()`. The old argument was that a dev tool should not grow the
  library — but the chip was never the library's to grow, it was three
  gpui-component widgets reimplemented worse, and it is the *dogfooding* rule
  that decided this: a catalog whose own controls are hand-rolled is not
  exercising the thing it catalogs. A `ButtonGroup` also gives a choice knob
  the segmented-control semantics a row of independent chips only imitated.
  A bool knob was briefly a `Switch`, on the argument that a knob takes
  effect immediately and that is what a switch means. It went back to a
  two-segment `ButtonGroup` (`BOOL_KNOB_LABELS`, "Off"/"On") for one
  disqualifying reason: `Switch` in this revision tracks no focus handle,
  sets no tab stop and fires from `on_mouse_down` only, so it was the single
  control in the dock unreachable by keyboard. Wrapping it in a focusable div
  would have produced a tab stop that toggles nothing. A bool is a two-way
  choice, so rendering it as one deletes the special case instead of adding a
  workaround — `render_knob` no longer needs to know that one kind of control
  carries its own label, and every knob in the dock now has the same shape:
  caption above, focus-ringed control below. A choice knob is a `ButtonGroup`, which toggles
  its children itself — the segments advertise their toggled state without the
  gallery asking, and single-select mode guarantees exactly one is on. A preset
  is an `outline` `Button` because a preset is a command, not a state. The one
  thing lost in the trade is `aria_selected` on the segments, since `Button`
  is not a stateful interactive element; the toggled state it does report is
  the richer signal anyway, because it says `false` as well as `true`.
- The story rows are the one hand-rolled surface left, and they take the app's
  shared `keyboard_focus_ring` rather than a ring of their own. An earlier
  version grew one out of a transparent resting border recolored on focus: it
  worked, but it cost two pixels of row height and it double-drew against the
  library list item's own inset active-highlight whenever a row was both
  selected and focused. A spread box shadow has neither problem. One app, one
  ring.
- The statusbar reports the story count and any fixture failure, so a broken
  fixture is visible from every story, not only from the one that needs it. It
  no longer repeats the selected `group · name`: the stage header directly
  above it is a `chat_header` carrying exactly that, and the same two words in
  two bands eight pixels apart is noise, not redundancy. What survives passed a
  test each line has to pass: name the object, name the scope, and say why it
  is worth a permanent band. The story count is the only place the catalog's
  size shows when every group is collapsed, and it is how you notice a story
  you just added never registered; the fixture failure is the reason the stage
  looks wrong, and it is scoped to every flagship story rather than to the one
  that happens to need the file.

### Recorded gaps

- **Dialog stories are chrome only.** `dialog chrome` renders the
  confirmation body, the button row and both inputs inline on the canvas; it
  never calls `window.open_dialog`. That is deliberate (plan Q10a): the dialog
  visual redesign is a follow-up, and it will be done *inside* the gallery,
  against these stories. Modal behavior (focus, escape, the Root dialog stack)
  is `modal.rs`'s business and is not what a component catalog is for.
- **Light mode is blocked.** `ui/transcript.rs` memoizes its `TextViewStyle`
  in a `OnceLock` with `is_dark: true` and a dark highlight theme, so a theme
  toggle would leave the transcript rendering dark inside a light window. The
  gallery is therefore dark-only for now (`shell_bootstrap` sets `Dark`), and
  the fix and the gallery's theme toggle land together, later — a toggle that
  half-works is worse than none in a tool whose job is to tell you what a
  component looks like.

## crates/harvester/src/lib.rs

The lib target exists for exactly one reason: two binaries (`harvester` and
`gallery`) share a component layer and a window bootstrap, and Cargo has no
way to share modules between binaries other than a lib.

What lives lib-side is only what both binaries need:

- `ui` — the whole visual layer. It was already dependency-free with respect
  to the rest of the crate (no `crate::` path in it pointed outside `ui`
  before this split, and the only one added since is
  `assets::GIT_BRANCH_ICON_PATH` for the prompt strip's branch indicator),
  so moving it cost no rewiring.
- `assets` — `HarvesterAssets` is the one `AssetSource` gpui accepts per
  application, and both binaries install it.
- `shell_bootstrap` — the shared application/window start-up.

Everything else stays declared on the bin side (`main.rs`): `chat`,
`workbench`, `dialogs`, `store`, `catalog`, `session_state`, `models`,
`modal`, `workspaces`, `desktop_shell`. Those are the *application*, not a
component library; the gallery must not be able to reach them, and keeping
them bin-side is what enforces that.

`main.rs` re-exports the two lib modules at the bin crate root
(`pub use harvester::{assets, ui};`) so every existing `crate::ui::…` /
`crate::assets::…` path in the app keeps resolving. This is deliberate: the
alternative was rewriting several hundred import paths for no behavioral
gain. The re-export is not a compatibility shim to be removed later — the
app's modules genuinely treat `ui` as part of its own crate root, and the
lib boundary is a packaging fact, not a layering statement.

`build.rs` is unaffected by the split: it writes `harvesterd` into `OUT_DIR`
once per *package*, and only `main.rs` reads it (`include_bytes!`). The lib
and the gallery bin never touch it.

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
- `pub use harvester::{assets, ui};` is the bin crate root adopting the two
  lib modules under the paths the app already uses (`crate::ui`,
  `crate::assets`). See the `lib.rs` section for why the split exists and why
  this re-export is the intended shape rather than a migration step.

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

## crates/harvester/src/shell_bootstrap.rs

Everything about starting a gpui application that is true for *any*
Harvester window, hoisted out of `desktop_shell.rs` when the gallery binary
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
  modals, notifications and the dialog stack work at all (see `modal.rs`).
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
  unfaded `muted_foreground` for secondary text/subtitles.
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
- `Tone::dot` derives the *shape* from the meaning: `Idle` and `Pending` are
  `Hollow`, everything else `Solid`. An outline says "no verdict yet" —
  unprobed, or working with the outcome still unknown — without spending a
  color the theme doesn't have. Making this a function of the tone is what
  retired the last per-call-site shape choice; `status_dot_for_tone(tone, cx)`
  is now how every caller draws a dot, and nobody picks solid-versus-hollow by
  hand. In one sentence: hollow means the dot is a placeholder for work that
  has not begun; solid means it stands for something real — running or
  finished, good or bad. That is why `Failed` is solid: a settled terminal
  state must not read as "not started".
- `STATUS_DOT_DIAMETER` (6px) is the dot itself, and it is one of the few
  pixel numbers in `ui` that is deliberately **off** the rem axis. A 6px disc
  is a raster boundary rather than product geometry: it is the smallest circle
  that still antialiases into a circle, and the rem steps on either side land
  it either visibly mushy or off the half-pixel grid on a 1× display. The dot
  should stay the same physical size when `BASE_FONT_SIZE` moves; the lane it
  sits in (`SIDEBAR_STATUS_LANE`, the tool block's `size_4` slot) is what
  scales instead, which is also why the diameter can change without moving any
  text.
- `Dot` (`Solid(Tone)` / `Hollow(Tone)`) is the shape vocabulary `Tone::dot`
  answers in, and nothing hand-builds one any more: every caller names a tone
  and lets the shape follow. `Dot` is therefore an output type, not an input
  one, which is what keeps the shape rule in a single place.
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

## crates/harvester/src/ui/dialog.rs

Every piece of modal chrome Harvester draws: dialog geometry, titles, the
cancel/primary button pattern, the two dialog input boxes, and the three
`open_*` builders that assemble them. Deliberately dumb — it knows nothing
about Pi, obligations, modal leases, or machines, and takes plain strings plus
callbacks that return "did this answer stick?". The three modal owners
(`dialogs.rs`, `workbench/forget_machine_flow.rs`, `workbench/picker.rs`) all
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
  Harvester renders at most one modal at a time (see `modal.rs`), so the two
  confirmation call sites deliberately share one pair of ids rather than
  threading per-flow ids through the builder. `select` and `text` carry their
  own pairs because they are different dialogs, not two callers of the same one.
- Commit-button wording is per dialog kind, not a generic "OK": `Reply` for a
  text entry, `Confirm` for a question, and `Cancel` everywhere for the
  refusal. A `select` has no commit button at all — see below. The button should say what it is about to do, so
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
used solely by other `ui` modules (`section_header`) is reached through
`super::core` / kept private to its module instead, so the flat `ui::`
surface stays a list of what the screens actually draw with. The dot builders
are the exception the gallery bought: no screen calls `status_dot` or
`status_dot_for_tone` directly (screens reach a dot through
`sidebar_stacked_row` or a tool block), but a component catalog has to be able
to render one on its own, and the gallery is a screen in every sense that
matters here. `empty_pane_action` is exported on the same footing — it is the
trailing button slot `empty_pane_hint` grew, catalogued before the first
screen adopts it.

Two of the re-exports are not builders at all and are worth naming for that
reason. `KeyboardFocusRing` (from `shell`) is a trait with a blanket impl, so
it has to be imported by name at the call site to be usable —
`use crate::ui::{self, KeyboardFocusRing as _}` in `workbench/sidebar.rs` is
the shape. `SIDEBAR_ROW_GUTTER` (from `sidebar`) is a metric rather than a
component: the sidebar's own screen needs it to line its New-session button up
with the rows below, which is the one case where a caller genuinely has to
know a number `ui` owns.
`dialog` is the one submodule declared `pub mod` instead of being re-exported
flat: its names are generic on purpose (`open_text`, `button_row`,
`action_button`), and they only read correctly qualified as
`ui::dialog::open_text`.

`ui` lives on the lib side of the crate (see the `lib.rs` section) so the
`harvester` and `gallery` binaries share one copy. That makes this flat
re-export surface the gallery's catalog as well as the app's import path:
anything a story renders must be exported here. `prompt_strip` and its value
types (`PromptStrip`, `PromptStripControl`, `PromptStripNotice`) are the first
composite — not a primitive but a whole assembled strip — and the rule that
only names a *screen* calls get re-exported still holds, since the chat screen
calls exactly that one. The three picker *actions* (`PickModel`,
`PickMachine`, `PickRepo`) are exported alongside them because a gpui action is
half a contract: `ui` dispatches them and the screen listens for them, so both
halves need the same type (see `ui/prompt.rs`).

## crates/harvester/src/ui/note.rs

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
  `truncated_control_label` keeps `CONTROL_LABEL_MAX_CHARS` (24) *characters*
  (never bytes, so a multibyte name is never split mid-codepoint), trims the
  trailing space so the ellipsis does not float, and appends one. All three
  real label sources (a folder name, a fleet machine label, a model name) are
  short in practice, so the budget should rarely bite; the unit tests pin the
  boundary, the trim and the multibyte case.
- `prompt_box` rounds through `theme.radius_lg` and takes the full theme
  `ring` on focus, plain `border` at rest. The old half-opacity resting border
  plus `shadow_md` are gone: with the graphite ladder the input well already
  reads as raised against the canvas, and a shadow under a control that also
  fades its own border was two devices doing one job. The half-opacity border
  was also a pseudo-role invented at the call site: the resting state is the
  plain `border` role now, for the same reason `faint()` became one named
  accessor instead of an inline `0.6` in two files.
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
  (`None` when there is nothing to report), `can_send`, `focused`. Nothing
  here is an `Entity`, a `Window` or a callback — the one live handle
  `prompt_strip` takes is the `&Entity<InputState>` for the text box, which
  gpui owns and cannot be reduced to a value.
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
- `prompt_strip(strip, send, input, cx)` takes exactly one callback. `send` is
  `Fn(&mut Window, &mut App)`: the click event carries nothing a caller needs,
  and dropping it keeps `gpui::ClickEvent` out of the surface. The three picks
  are actions instead (above), which is why there is no handler struct any
  more — a strip is a value plus one closure plus one live `InputState`.
- Element-id consts (`MODEL_CONTROL_ID` = `"chat-model"`, and the
  machine/repo/send counterparts) kept their original `chat-*` names from when
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

## crates/harvester/src/ui/shell.rs

Window chrome that is not the prompt, the transcript, or a modal: section
headers, the chat pane's header, the statusbar frame, and the app's one
keyboard focus ring.

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
  takes `mx(SIDEBAR_ROW_GUTTER)` / `px(SIDEBAR_ROW_PADDING)` from
  `ui/sidebar.rs` and rounds `rounded_md`, so its focus ring draws the same
  pill in the same lane as the row underneath it. Its padding doubled with the
  rest of the chrome, `pt_2`/`pb_1` → `pt_4`/`pb_2`, which keeps the deliberate
  2:1 asymmetry (more air above a heading than below it, so the heading binds
  downward to the rows it labels) while giving the header a box with real room
  in it. That box used to be invisible; now it rounds and takes a focus ring,
  so its shape is something the user actually sees, and at `pb_1` there was
  under 4px between the text and the ring's lower edge.
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

## crates/harvester/src/ui/sidebar.rs

The sidebar's row vocabulary — every machine and session row in the app is
built from these three, so the two lists cannot drift apart visually. Only
presentation lives here; which rows exist, what they say, and what a click
means belong to `workbench/sidebar.rs`.

- The row metrics are rems, and two of them are `pub`/`pub(super)` because
  other code has to line up with them. `SIDEBAR_ROW_GUTTER` (`rems(0.25)`) is
  the row's outer margin — what insets the hover/selection fill from the
  sidebar edge — and `SIDEBAR_ROW_PADDING` (`rems(0.75)`) its inner padding.
  These were one shared 6px number on the theory that margin and padding
  should match; they no longer do, because the margin's job (keep the fill off
  the edge) needs far less room than the padding's job (keep the label off the
  fill's edge), and at rem 14 the shared value was too much for one and too
  little for the other. `SIDEBAR_CONTENT_INSET` is their sum — the lane the
  row *text* starts in — and is what non-row content (`sidebar_empty_note`)
  aligns to so a note and a row title share one left edge. `section_header`
  in `ui/shell.rs` reads the same two constants, and `workbench/sidebar.rs`
  reads `SIDEBAR_ROW_GUTTER` for the New-session button.
  The point of all of that is a single alignment spine. Four siblings describe
  the same level of the sidebar — the New-session button, the section headers,
  the rows and the empty note — and each used to invent its own leading edge
  from its own literal. They now all resolve from this one pair, so changing
  the gutter moves every one of them together instead of starting a hunt.
  `SIDEBAR_CONTENT_INSET` is *computed* (`rems(GUTTER.0 + PADDING.0)`) rather
  than typed a second time; `Rems` is a tuple struct and `rems` is a `const fn`,
  so the sum costs nothing at runtime and cannot drift. `ui/shell.rs` reaching
  into `ui/sidebar.rs` for geometry is the intended direction: the sidebar owns
  the inset, and `collapsible_section` is a sidebar-shaped header that the
  gallery's dock happens to reuse.
- Row padding is `py_2` (was `py_1`), matching the standard-tier step the rest
  of the chrome took: a session row carries two lines of `text_sm` and at
  `py_1` the stacked rows ran together into one grey block.
- `SIDEBAR_STATUS_LANE` (`rems(1.)`) is a fixed lane the status dot sits in,
  and the same lane the `detail` second line is indented by. It replaced a
  derived `STACKED_ROW_DETAIL_INDENT` that had to be recomputed by hand every
  time the dot diameter or the dot/title gap changed (it was "6px + gap_2").
  Declaring the lane instead of deriving it means the two can never disagree,
  and it is why the detail line hangs under the *title* rather than under the
  dot. The dot sits at the lane's leading edge and the row carries no outer
  gap of its own, so the dot's diameter is free to change without moving a
  single character of text.
- `sidebar_row` is a `ListItem` base, not a `div`: the library owns the
  hover/selected/disabled treatments, and machine rows rely on `disabled`
  gating the row while still rendering a live suffix button (see
  `render_missing_machine_row`). It sets a `tab_index` so rows are reachable
  by keyboard; a caller that must *not* be a tab stop (the machine rows, which
  are not clickable) opts out with `tab_stop(false)` — still focusable
  programmatically, just skipped by Tab, because a row with no click handler is
  a dead end to land on — and a caller that is opts into the ring with
  `keyboard_focus_ring`. The ring cannot live inside `sidebar_row` itself: it
  needs `cx` for the theme's ring color and this builder takes only an id, so
  the split is deliberate — the row owns reachability, the call site owns the
  ring. It no longer restates a text
  size — the ambient body size is correct now that nothing overrides it. The
  size was *removed* rather than read from the theme for the same reason the
  ring stayed outside: both roots that can host a row (the workbench root and
  the gallery's chrome root) set the theme's font size themselves, so the row
  renders at exactly the size it used to declare with one fewer place to drift.
  A row mounted under some third root wants the size restored at that root,
  never back on the row.
- `sidebar_stacked_row` takes a **`Tone`, not a `Dot`**: the shape is derived
  by `Tone::dot` (see `ui/core.rs`), so no caller picks solid-versus-hollow any
  more. It is one shape for both lists: status lane + title on the first line,
  an optional inline `note` (transient status, e.g. "opening…") after the
  title, and an optional `detail` second line (the row's context — provider, or
  folder · machine). Both secondary texts are `text_sm` and resolve their color
  through `Tone::Muted` rather than `cx.theme().muted_foreground` directly,
  so `ui` keeps its rule that a component asks for a meaning, never a theme
  field (SPEC C11).
- `status_lane` is a private builder rather than a bare dot because the dot has
  to *say* something: it carries `Tone::label` as both its `aria_label` and its
  tooltip. A color alone is not a status for a screen reader, and it is not one
  for a sighted user who has not memorized six shades either.
  `STATUS_LANE_ELEMENT_ID` is the bare string `"status"` for every row, which is
  correct rather than lazy: gpui element ids are paths, and every row already
  carries a unique id, so a leaf only has to be unique among its siblings.
- `sidebar_empty_note` is the "no sessions yet" line. It is a plain note, not
  a row: an empty list has nothing to select, so it must not look clickable.

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
- `UI_FONT` "Inter" / `MONO_FONT` "Geist Mono": both set as theme slots
  (`font_family`, `mono_font_family`) so `Root` styles the whole window and
  gpui-component widgets agree with our own mono runs (`core.rs` reads the
  slot back). gpui matches families by exact name; if a family is missing,
  the platform text system substitutes, so a machine without these installed
  degrades instead of failing.

## crates/harvester/src/ui/tool_block.rs

Reusable visual primitives (frame, typography, status treatment) for one
transcript tool-call block; tool-specific labeling and body policy live in
`transcript.rs` instead, keeping this module purely presentational.

- `block` renders on the `popover` slot with a hairline border, rounded through
  `theme.radius_lg`: in the graphite elevation ladder a tool card is a raised
  surface over the canvas (it was previously the `sidebar` slot, which the
  graphite ladder repurposed as the *darkest* tier — keeping it would have
  sunk the cards below their page). It carries **no** shadow. A transcript can
  hold a dozen of these in a row, and a dozen stacked `shadow_sm` cards read as
  clutter rather than depth; the fill and the hairline already place the card
  on the ladder, which is what a shadow was there to do. The header chevron is
  `core::disclosure_chevron`, tinted `faint` like every disclosure marker.
- The header is `py_2` (up from a bare `px(6.)`, i.e. the compact tier), takes
  a `tab_index`, and carries `Role::Button` with `aria_expanded` and an
  `aria_label` the caller supplies (see `tool_label` / `tool_accessible_name`
  in `transcript.rs`). It takes its `index` rather than an `ElementId` and
  builds `(HEADER_ELEMENT_ID, index)` itself, so the id scheme lives in one
  place with the status slot's matching `(STATUS_ELEMENT_ID, index)` — the row
  needs a stateful id for focus and click, the indicator needs its own for the
  tooltip, and letting the caller invent either one invites a collision.
  `HEADER_TAB_INDEX` (0) makes every tool header an ordinary tab stop in
  document order, and that single call is the whole keyboard story: the
  existing `on_click` toggle answers Enter and Space with no key handling
  written here (see `ui/shell.rs` for the gpui mechanics).
- Focus on the header is a transparent 1px border that turns `theme.ring`,
  not the shared `keyboard_focus_ring` shadow. The card clips its children
  (`overflow_hidden`), so a shadow drawn outside the header's box would be cut
  off; reserving the border at `transparent_black()` means gaining focus
  changes a color and never a layout.
- The header has both a hover fill (`list_hover`) and a press fill (`muted`):
  press feedback lands on pointer-down, matching every other clickable
  surface in the app.
- `status` is the dot in a fixed `size_4` slot carrying the caller's status
  word as its tooltip — same rule as the sidebar's status lane: a color is not
  a status unless something says what it means. The word is *not* `Tone::label`
  here; it is `transcript::tool_status_label`, because the tones speak the
  machine- and session-liveness vocabulary and a finished tool call is
  "Completed", not "Neutral". The slot is `size_4` rather than hugging the 6px
  dot for two reasons: a 6px tooltip target is not something a pointer can
  reliably hit, and a fixed-width indicator gives a stack of repeated rows one
  stable icon column, which is what keeps every header's label starting on the
  same x.
- `stats` is one faint run (`+3 −1`), not a green `+` beside a red `−`. Diff
  counts are metadata about an edit, not a verdict on it, and spending the
  success/danger colors on them made a routine edit look like a test result —
  the two colors that mean "it worked" and "it failed" have to stay scarce.
  Semantic color is legitimate where it *is* the data, which in this module is
  the diff lines in the body and nowhere else. `stats` also gave up its own
  smaller size, which is what lets the `items_baseline` label row line up
  exactly instead of nearly.
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
- `body` is `text_sm` on the mono family with an explicit
  `line_height(relative(CODE_LINE_HEIGHT))` (1.45). Code needs tighter leading
  than the 1.7 prose uses — it is the *ratio* between the two that carries the
  meaning (code reads as one block, prose wants air), not either number on its
  own — and `TextViewStyle` has no line-height field, so the
  wrapper is where it is set — the same technique `transcript.rs` uses for
  messages. `code_lines` is a bare unpadded column so a run of diff or written
  lines stacks with no gap between them; the `gap_1` on `body` separates the
  *sections* of a body (context line, content, output), not the lines inside a
  section. Equal gaps claim equal relationships, and consecutive lines of one
  file are not siblings with space between them — their leading is what should
  separate them, which is exactly what `CODE_LINE_HEIGHT` is for.
- `truncation_notice` says "Output truncated" as a sentence, not "… output
  truncated": it is chrome stating a fact about the block, and the leading
  ellipsis made it read like part of the output it is describing. No final
  period either — a short state is a label, not a sentence to be closed.

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
- `tool_tone`: `ToolStatus::Running` maps to `Tone::Active` rather than
  `Tone::Pending` — a running tool is the live thing the user is watching on
  screen right now, not a "just working, outcome unknown" state. `Ok` maps to
  `Tone::Muted`, not `Tone::Idle`: a settled tool call is ordinary secondary
  content, and `Idle`'s extra fade (0.6 over an already-dim gray) pushed a
  perfectly legitimate result under the contrast floor. `Idle` is now reserved
  for things that are genuinely *unknown* — an unprobed machine — which is what
  also makes `Tone::dot` render it hollow.
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
  from the tone, through `Tone::dot`.
- `output_text` centralizes the empty-output case: "Running…" while the call is
  in flight, "No output" once it has settled. It was `"(no output)"` —
  parenthesised asides are how a UI mumbles, and a tool that genuinely printed
  nothing is a fact worth stating plainly. Pinned by a test.
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
something different in each place. Choosing the `tone` exactly once in
`present_machine` is what prevents a failed state from rendering red in one
surface and gray in another — tone and message are decided together instead
of being independently derived per call site.

The field is a `Tone`, never a `Dot`. It used to be a hand-built `Dot`, and
every consumer threw the shape away and re-derived it from the tone, so the
shape recorded here was not what got drawn: a `Reachable` machine written as
`Dot::Solid(Tone::Idle)` rendered *hollow*, which is the treatment reserved
for a machine nobody has probed. Naming only the tone makes that class of
disagreement unrepresentable. `Reachable` is `Tone::Muted` for exactly this
reason — it is a settled verdict and must read as solid — while `Unknown`
keeps `Tone::Idle` and stays hollow. A unit test pins the two apart.

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
`ui::sidebar_empty_note` own how a row *looks* (see `ui/sidebar.rs`), and
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
- `MISSING_MACHINE_DETAIL = "no longer listed"` — the subtitle shown for a
  machine its provider stopped listing; this is also the only row kind that
  can be forgotten.
- `FORGET_MACHINE_TOOLTIP = "Forget this machine"` — deliberately "forget"
  rather than "delete": forgetting only edits Harvester's local record.
  Nothing on the machine itself is touched, and (being unreachable) there is
  nothing reachable to touch anyway.
- `MACHINES_SECTION_TITLE` / `SESSIONS_SECTION_TITLE` ("Machines",
  "Sessions"), `NO_SESSIONS_NOTE`, `NEW_SESSION_LABEL`, `OPENING_SESSION_NOTE`
  ("opening…") — every user-visible string in this screen is now a named
  const rather than a literal at its call site, which is the house
  no-magic-values rule applied to copy. The two section titles are sentence
  case because `ui::section_header` no longer uppercases what it is given (see
  `ui/shell.rs`); `OPENING_SESSION_NOTE` stays lowercase because it is a
  transient status fragment trailing a title, not a heading.

Keyboard and accessibility, which is screen policy and therefore lives here
rather than in `ui/sidebar.rs`:
- Session rows are `Role::ListItem` with `aria_label` (the row title) and
  `aria_selected`, and they take the shared `keyboard_focus_ring`. They are the
  sidebar's only clickable rows, so they are its only tab stops.
- Machine rows opt *out* with `tab_stop(false)`. They are `disabled` — nothing
  happens when you click one — and a focus stop that leads nowhere is worse
  than no focus stop at all. They still carry `Role::ListItem` and an
  `aria_label`, because being unclickable does not make them unreadable.
- Machine rows pass `present_machine(...).tone` to `ui::sidebar_stacked_row`,
  which derives the dot's shape itself (`Tone::dot`). Shape follows meaning,
  and there is only one place that decides it.
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
- **`Dot::tone` and the `Dot`-taking `status_dot` have no callers.** Nothing
  hand-builds a `Dot` any more, so `Dot` is only ever produced by `Tone::dot`
  and consumed by `status_dot`. The tidy-up is to fold `Tone::dot` into
  `status_dot_for_tone` and delete both, leaving `Dot` as a private detail of
  `ui/core.rs`. Small, and it wants doing before someone reads the public
  export as an invitation to build one by hand again.
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
