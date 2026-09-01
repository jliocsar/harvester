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



## crates/harvester/src/chat.rs

`Chat` is a window-local GPUI adapter identified by `ChatId`; application truth stays in `ClientSnapshot`. A draft and its promoted live session use the same entity. `set_id` changes the key in place, preserving focus, scroll position, input, tool-expansion state and outstanding submission ownership.

The pane subscribes to `FleetStore` for immutable snapshots and semantic runtime events. `transcript_signature` uses `SessionReplica::revision`, not collection length, so cumulative stream replacement and tool updates scroll and repaint correctly. Transcript presentation and tool expansion remain window-local because neither affects Pi or another window.

Prompt submission reserves a `SubmissionId`, records the input under that id, and only then sends the intent. Until Pi acknowledges one pending submission, duplicate Enter presses are ignored. `PromptAccepted` clears the editor only when its text is still the submitted text; later typing is preserved. `PromptRejected` restores only the matching submission when the editor is empty, and `PromptCompleted` retires its local record. Equal prompt strings remain independent because all three events correlate by id.

Model, machine, repository, stop and send actions are typed runtime intents. Draft-only machine/repository changes cannot mutate a live session's fixed launch target. A folder-picker request is the only flow emitted upward, because Root's modal surface is window-owned by `Workbench`; bridge and machine traffic never passes through the view.

`render_transcript` reads the live session's derived transcript directly from the snapshot. A promoted/opening session whose runtime value has not reached the current frame renders `OPENING_SESSION_HINT` rather than fabricating empty history. `render_bottom_row` delegates all content policy to `models::present_prompt_strip` and all chrome to `ui::prompt_strip`.

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

`main` constructs `ClientRuntimeOwner` before starting GPUI. Runtime construction failure exits with a concrete diagnosis; the owner then moves into the root-view closure, keeping the process runtime alive for the window while `Workbench` receives only its cloneable handle.

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

This module is the pure presentation layer for prompt controls. It reads `ClientSnapshot` plus `ChatId` and returns `ui::PromptStrip`; no GPUI entity, runtime command or Pi payload is owned here. Model extraction and state transitions belong to client-core.

`present_prompt_strip` decides notice, send/stop availability and model, machine, repository and branch controls in one window-free value. `can_send` requires a resolved draft target or an attached live session and is additionally gated by the view's submission-acceptance state. `can_stop` comes from `LiveSession::can_stop`.

Machine candidates exclude only `Missing` and `Unreachable`. A fresh connectable `Unknown`, a retryable failed/lost bridge and connecting/connected machines remain selectable; selecting one is what asks the runtime to connect or retry. Repository candidates are the stored workspaces for the draft's selected machine. The special `ADD_REPO_ROW` remains last and therefore resolves to no stored candidate in `Chat::handle_pick_repo`.

Draft model choice includes Pi's default row; live model picks represent `set_model`. Labels are derived from `ModelsState` and `ModelSelection`, while machine and repository labels come from stable typed ids. Tests pin unresolved, loading, starting, attached, exited and candidate-filter behavior without a window.



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

`FleetStore` is the thin GPUI boundary around process-owned `ClientRuntime`. It owns only a cloneable runtime handle and the latest immutable `Arc<ClientSnapshot>`; fleet, workspaces, drafts, sessions, dialogs, request tokens and async ownership stay in client-core.

One detached GPUI task drains this window's `RuntimeEvents` subscription. For each event it refreshes the snapshot first, notifies observers, then emits the semantic/runtime event. Views therefore never observe a newer event against an older store snapshot. Runtime shutdown closes the subscription; fatal shutdown state is published before closure.

Every remaining method is a typed, non-blocking intent forwarder. There is no generic JSON send path and no domain transition in this module. Prompt ids are reserved through the runtime before submission, and browser, draft, dialog and session identities remain typed across the seam.

## crates/harvester/src/ui/core.rs

`core` is the one place a semantic meaning becomes a color (SPEC C11) —
everything else in `ui` should ask for a `Tone`, never a raw `Hsla`.

- `core` holds **no text-size constant of its own any more**. Body text is the
  theme's `font_size` slot (`BASE_FONT_SIZE`, see `ui/theme.rs`), which the
  window root restates as `text_size(cx.theme().font_size)` so every
  descendant inherits it, and which is simultaneously the rem unit every
  rem-based padding, gap and height in `ui` scales off. Sidebar rows and
  medium `Button`s inherit that 14px body tier. Anything wanting *smaller*
  text asks for `text_sm`, the library's own token — **not** `text_xs`, which
  at rem 14 renders 10.5px and falls under the gpui-component design guide's
  ~12px typography floor. `text_sm` (12.25px at rem 14) is the app's one
  secondary tier.
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
- `text_input(input)` is the one renderer for an `InputState` at the body
  tier. gpui-component's medium `Input` defaults to `text_sm` even though a
  medium `Button` defaults to `text_base`; wrapping `Input::new(input)` with
  `text_base` here closes that hidden mismatch once for the prompt, dialogs,
  and the gallery's own text controls. The state factories stay with the
  surface that decides input behavior; this factory decides only typography.
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
- `empty_pane_action` is that action: a medium primary `Button`, one per empty
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
- `create_single_line_input` and `create_multi_line_input` decide the input
  state and behavior; `core::text_input` decides how either state is rendered.
  `open_text` and the gallery's dialog story both call that same renderer, so
  the library's 12.25px medium-input default cannot drift back into one surface
  while dialog buttons and body copy remain at 14px.
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
flat `ui::` surface did not move a builder in either merge — `sidebar_row`,
`sidebar_stacked_row`, and `sidebar_empty_note` are
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
screen adopts it. `text_input` is the shared visual factory that keeps direct
`Input` renderers at the 14px body tier; prompt and dialog state construction
remain in their own modules. `create_prompt_input` is the third of the widget
factories (`ui::dialog`'s two inputs are the others) and is exported for the
reason those are: the chat screen and the gallery must build the same
`InputState`, and the export is what makes that a fact rather than a
convention.

Two of the re-exports are not builders at all and are worth naming for that
reason. `KeyboardFocusRing` (from `shell`) is a trait with a blanket impl, so
it has to be imported by name at the call site to be usable —
`use crate::ui::{self, KeyboardFocusRing as _}` in `workbench/sidebar.rs` is
the shape. `SIDEBAR_EDGE_GUTTER` (from `shell`) is a metric rather than a
component: the sidebar's own screen needs it to line its New-session button up
with the section spine, which is the one case where a caller genuinely has to
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

- `COMPACT_CONTROL_HEIGHT_IN_REMS` (1.875, 26.25px at rem 14) is the prompt
  selector frame. The three pickers first use `.small()`, which changes text,
  icon, caret, gap and horizontal padding together, then replace only the
  library's cramped 21px height with this one-half-step reduction from the 28px
  standard action. Their text is `text_sm` (12.25px), the same 1.75px step down
  from body text. The library fixes a small button's icon/label gap at `gap_1`,
  so `prompt_box_control` gives the icon `mr_0p5`; the effective `gap_1p5`
  (5.25px) is the middle rung between cramped and two separate controls. Send
  and stop remain medium 14px/28px actions. The pickers cap
  at `max_w_56` (14rem — a long model or repo name still cannot push the rest
  of the row off the strip), and send stays
  `Button::primary().rounded_full()`.
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
  `pb_2`) and nothing else. Its child is rendered through `core::text_input`,
  because authored composer text stays at the 14px body tier even though the
  compact selection context below it uses `text_sm`. It is private to this
  module: `prompt_strip_box` is its only caller and no screen or story draws a
  bare input well, so it fails the `ui/mod.rs` re-export rule (only names a
  *screen* calls are published). It stays a named builder rather than an inline
  `div()` because the controls row's inset is solved *against* this one — the
  two numbers are a pair, and the next reader has to be able to see both.
- `prompt_box_controls()`: `pl_2` + `pr_3`, `gap_2`. The left inset is a visible
  7px margin for the first picker's hover surface; aligning its icon to the
  input character had put the trigger box itself against the prompt edge, which
  looked cramped even though the arithmetic was exact. The icon-only send
  button has no horizontal padding around a label, so `pr_3` still lands its
  trailing edge on the input's opposite margin.
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
- `branch_indicator` is a `Tag::secondary()`, not a disabled picker. It never
  opens anything, and rendering it as a dead dropdown trigger made it look like
  a control the user had failed to earn. A tag reads as a label about the
  session, which is all it ever was. `Tag` is this library's chip —
  `gpui_component::Badge` is a notification overlay dot, not the same thing —
  and the `secondary` variant is chosen because a branch name carries no
  success, warning or danger meaning; a strip with a row of colored badges in
  it is a hierarchy decision nobody made. The library renders Tag text as
  `text_xs` at every nominal size, so this instance explicitly takes the same
  `COMPACT_CONTROL_HEIGHT_IN_REMS`, `text_sm`, `size_3p5` icon, and `gap_1p5`
  as the compact picker treatment: its semantics differ from the pickers, but its
  visible baseline does not. It is still the only place in `ui` that names an
  asset path (`assets::GIT_BRANCH_ICON_PATH`), and still the reason `assets`
  and `ui` had to land on the same side of the lib/bin split.

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
- **The alignment spine.** The sidebar metrics are rems, and they are the one
  place its horizontal edges are written down. `SIDEBAR_EDGE_GUTTER`
  (`rems(0.25)`) keeps section-header and New-session hit areas off the window
  edge; `SIDEBAR_ROW_PADDING` (`rems(0.75)`) keeps content off a control's own
  edge. `SIDEBAR_CONTENT_INSET` is their computed 1rem sum and has two jobs:
  it is where a section's title starts and chevron ends, and it is the outer
  margin of every `sidebar_row`. A selected row's left and right borders
  therefore align with the title and chevron above it, while the row's own
  padding indents its contents one level further so rows still read as children
  of the section. `sidebar_empty_note` starts at the same 1rem section spine.
  The inset is computed (`rems(EDGE_GUTTER.0 + PADDING.0)`) rather than typed a
  second time; `Rems` is a tuple struct and `rems` is a `const fn`, so the sum
  costs nothing at runtime and cannot drift. `SIDEBAR_EDGE_GUTTER` is the one
  metric that leaves this module — `workbench/sidebar.rs` reads it so the
  New-session button's `px_3` completes the same inset — and it is exported for
  that single caller.
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
  `keyboard_focus_ring`. Its `mx(SIDEBAR_CONTENT_INSET)` applies to every row,
  not only a selected one, so selection never changes layout and the selected
  border always lands on the section's title/chevron spine. The ring cannot
  live inside `sidebar_row` itself: it needs `cx` for the theme's ring color and
  this builder takes only an id, so the split is deliberate — the row owns
  reachability, the call site owns the ring. It no longer restates a text size
  — the ambient body size is correct
  now that nothing overrides it. The size was *removed* rather than read from
  the theme for the same reason the ring stayed outside: both roots that can
  host a row (the workbench root and the gallery's chrome root) set the theme's
  font size themselves, so the row renders at exactly the size it used to
  declare with one fewer place to drift. A row mounted under some third root
  wants the size restored at that root, never back on the row.
- `sidebar_stacked_row` takes a **`Tone`, never a shape**: the shape is derived
  by `Tone::is_hollow` inside `status_dot_for_tone` (see `ui/core.rs`), so no
  caller picks solid-versus-hollow any more. It is one shape for both lists:
  status lane + title on the first line, an optional inline `note` (transient
  status, e.g. "opening…") after the title, and an optional `detail` second
  line (the row's context — provider, or folder · machine). Both secondary
  texts start from `secondary_text()` and resolve their color through
  `Tone::Muted` rather than `cx.theme().muted_foreground` directly, so `ui`
  keeps its rule that a component asks for a meaning, never a theme field
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
- `secondary_text()` is the shell's one 12.25px (`text_sm`) constructor. The
  stacked row's "opening…" note and detail, the empty-list note, section
  headers, the chat-header subtitle, and `statusbar_frame` all begin there;
  grep `secondary_text()` in `ui/shell.rs` proves the shared source. Color and
  truncation remain specific to each role, but their type size cannot drift.
- The statusbar band is `h_8` (2rem, 28px) — one secondary line with real
  breathing room, still short enough to read as a status strip rather than a
  second toolbar. There is no `STATUSBAR_HEIGHT` const any more: the rem step
  *is* the number, and restating it in pixels would just be a second place for
  it to be wrong. Peers inside the band use `gap_2`, the same compact-row
  relationship as headers and dialog actions.
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
- `section_header`'s content defines the selectable-row boundary: its
  `mx(SIDEBAR_EDGE_GUTTER)` + `px(SIDEBAR_ROW_PADDING)` place the title and
  chevron at `SIDEBAR_CONTENT_INSET`, and `sidebar_row` uses that computed inset
  as its own outer margin. A sidebar-shaped header that the gallery happens to
  reuse and an app row therefore cannot disagree about where a selected border
  belongs.
  Its padding doubled with the rest of the chrome, `pt_2`/`pb_1` →
  `pt_4`/`pb_2`, which keeps the deliberate 2:1 asymmetry (more air above a
  heading than below it, so the heading binds downward to the rows it labels)
  while giving the header a box with real room in it. That box used to be
  invisible; now it rounds and takes a focus ring, so its shape is something
  the user actually sees, and at `pb_1` there was under 4px between the text
  and the ring's lower edge.
- `chat_header` is `py_3` for the same reason the statusbar grew, and its
  subtitle comes from `secondary_text()`. The subtitle slot has a rule: it
  names the next action or the target, never a restatement of the title. The chat pane pairs "New
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
  line box as the prose around it. It also fixes both typography tiers:
  medium controls and body text are 14px, `text_sm` is 12.25px, and `text_xs`
  would be 10.5px under the ~12px typography floor, which is why `text_xs`
  appears nowhere in `crates/harvester`. The inline theme test pins both
  `font_size` and `mono_font_size` to this one const.
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

`Workbench` is one window's composition shell. It owns selection, open `Chat` entities, Sidebar and ModalCoordinator entities and GPUI subscriptions; it does not own application truth. The process runtime may be shared by future windows, while this map and selected id remain independent per window.

Open chats are keyed by `ChatId`, so several sessions on one machine remain alive and subscribed concurrently even though only one is rendered. Opening a session asks the runtime to attach and then selects or creates its view. Starting a draft produces `DraftPromoted`; `promote_draft` rekeys the existing `Chat` entity instead of replacing it, preserving unsent input and presentation state.

Sidebar intents and modal outcomes are the only window-level routing. Runtime failures become notifications, successful machine deletion removes affected local chat entities, and fatal runtime state is also visible in the empty pane and status bar. `present_empty_pane` and `present_statusbar_link` are pure snapshot projections tested beside the module.

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
  in both files. This one owns it because this is where `MachineStatus::Missing`
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
say. The payload is gone from `MachineStatus` entirely (see
harvester-client-core.md §fleet.rs); if a build identity is ever worth
showing here, `ping.build` is the real one and it is bootstrap's to publish.

`MachineStatus::Missing` handling: a "missing" listing is only authoritative after its provider has actually spoken; it is not inferred from silence. Client-core runtime owns that precedence decision, and this presentation function only names the verdict once the snapshot has committed to it.

## crates/harvester/src/workbench/modal.rs

`ModalCoordinator` is the one owner of Root's global dialog surface for a window. `ActiveModal` makes Pi questions, repository browsers and machine-removal confirmation mutually exclusive; `awaiting_dialog` prevents a just-answered Pi obligation from reopening before the runtime snapshot advances.

Dialog obligations come from the process runtime's global FIFO, but presentation is window-local. An answer is sent with the exact `DialogKey`; the coordinator waits for Pi acknowledgement to remove that obligation from the snapshot before advancing. `DialogAnswerFailed` clears the local wait and reopens the unchanged obligation for retry. Dismissal semantics are defined by `DialogAnswer::dismissal_for` and rendered through `dialogs.rs`.

Repository flows retain both `DraftId` and `WorkspaceBrowserId`. `WorkspaceAdded` closes and emits `RepoAdded` only when its browser id matches the active picker, so another window or a stale run cannot consume the completion. Closing the dialog cancels the matching runtime browser.

Machine removal remains open after confirmation until the matching `MachineForgotten` arrives. A matching failure resets the request guard and leaves the dialog available for retry; unrelated machine outcomes are ignored.

## crates/harvester/src/workbench/picker.rs

`FolderPicker` is a GPUI projection of one runtime-owned `WorkspaceBrowser`, keyed by `WorkspaceBrowserId`. Home resolution, path state, request tokens, navigation validation, Git validation and persistence live in client-core; this module owns only the searchable list, dialog chrome and click-to-intent mapping.

`EntriesDelegate` presents directories before files and supports local filtering. Files remain visible for orientation but disabled. Parent and directory confirmation send typed browser intents. The runtime accepts a directory only when its name exists in the current authoritative listing, so a stale or forged view action cannot construct an arbitrary remote path.

`shown_listing` fingerprints path and `(name, is_dir)` pairs to avoid resetting the list's filter and selection on unrelated snapshot changes. Waiting and failure notes derive from `WorkspaceBrowserStatus`; confirming sends validation without closing Root. Only the matching `WorkspaceAdded` event, handled by ModalCoordinator, closes the dialog.

`ENTRY_LIST_HEIGHT_IN_REMS` keeps modal geometry stable while loading and filtering. Copy constants name the title, action, filter and each progress state. Cancel sends `cancel_browser` before releasing the modal, invalidating all outstanding runtime tokens.

## crates/harvester/src/workbench/sidebar.rs

Sidebar is a read-only snapshot projection plus window-local collapse and selection state. It renders the new-session action, runtime machine snapshots and keyed session summaries; repositories are selected inside draft chats rather than represented as a separate sidebar citizen.

`SidebarIntent` carries stable `SessionKey` or `MachineId` values, never row indexes. Session rows combine the summary's persisted status with the exact live-session lifecycle for the opening indicator. Selection is the Workbench's `ChatId`, so draft and same-sid/different-machine chats cannot collide. Missing names fall back to an eight-character Unicode-safe sid preview.

Machine rows use `present_machine` for one tone vocabulary. Only authoritative `MachineStatus::Missing` rows expose Forget; unreachable, failed and lost machines remain retryable through ordinary selection elsewhere and are not deletable. The row itself is disabled and absent from the tab sequence; the explicit forget button remains accessible.

Collapsible state, selected chat and keyboard focus styling are intentionally window-local. Application state is observed through `FleetStore`; no sidebar action mutates a snapshot directly.

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
