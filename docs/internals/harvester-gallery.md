# harvester-gallery — implementation notes

The dev-only component catalog for `harvester::ui`, and the notes that would
be comments if this codebase allowed any. Organized by module, same as
`docs/internals/harvester.md`: one `##` heading per module path, so a
`grep '^## '` audit across `docs/internals/` sees this crate's modules the
way it sees every other crate's. (The three `###` subsections under the
crate heading are the quarantine story, not modules.) The gallery lived
inside `crates/harvester` until the quarantine described below, so its
history is in that file's git log.

## crates/harvester-gallery/ — the component gallery

A dev-only binary that renders Harvester's `ui` components in isolation,
Storybook-style, and the regression surface the design pass was done against.

```sh
cd crates/harvester-gallery && cargo run          # the catalog
python3 scripts/gallery_walk.py                   # the walk, from the repo root
```

### Why it is a crate of its own, and why it still lives under `crates/`

The gallery `path`-depends on `gpui-mcp-bridge`, which lives in a sibling
`gpui-mcp` checkout that no clean clone of this repo has. While the gallery
was a `[[bin]]` of `crates/harvester`, that dependency broke the repo's own
gate: `cargo check --workspace` failed anywhere but the machine with the
sibling checkout. Three arrangements were measured in a scratch workspace with
the path absent, and only the last one works:

| arrangement | result |
|---|---|
| `optional = true` + a feature the walk enables | **fails** — cargo loads every path-dep manifest before features are resolved |
| `[target.'cfg(any())'.dependencies]` | **fails**, same reason |
| a crate with its own `[workspace]`, `exclude`d from the root manifest | **passes** |

So `crates/harvester-gallery` carries its own `[workspace]` table and its own
`Cargo.lock`, and the root manifest lists it under `exclude`. It stays
*under `crates/`* deliberately: `tests/no_comments.rs` walks the filesystem
from `crates/`, not the workspace member list, so quarantining the crate keeps
the zero-comment lint covering it. The gallery is real, maintained code that
must obey the house rules; only its build graph is separate.

The measured costs, recorded so nobody rediscovers them:

- **Its own lockfile.** The gpui/gpui-component pins are duplicated from
  `crates/harvester/Cargo.toml` and must be bumped together with it. A drift
  is not caught by the compiler — it shows up as a gpui-mcp handshake refusal
  or as a story rendering differently from the app.
- **Its own `target/`.** Building the gallery is an independent gpui build.
- **`--workspace` commands do not cover it.** The gate is
  `cargo check`/`cargo test` run *inside* `crates/harvester-gallery`, plus the
  walk. `cargo test --workspace` from the root still lints its comments,
  because that walk is filesystem-based.

### What it is allowed to see

`harvester`'s lib target (`ui`, `assets`, `shell_bootstrap`) plus
`harvester-client-core::transcript` and `harvester-proto`'s framing
constants — nothing else. The application modules (`chat`, `workbench`,
`store`, …) stay bin-side of `crates/harvester` precisely so a story cannot
reach them: a catalog that could read the app's state would stop being a
catalog. Anything a story renders must therefore be exported from `ui`, which
is what makes `ui/mod.rs`'s flat re-export list the catalog's contract as well
as the app's import path.

Module map:

| module | role |
|---|---|
| `main.rs` | bootstrap, the `Gallery` root view, all mutable gallery state, replay scheduling |
| `chrome.rs` | the gallery's own screen: story sidebar, stage, control dock, statusbar |
| `story.rs` | the story/knob/preset model (pure data, no rendering) |
| `registry.rs` | the concatenation of the per-module story lists, and the two derivations over them |
| `stories/` | one module per `ui` submodule, each declaring *and* rendering its own stories |
| `runtime.rs` | the live state stories share with the gallery (inputs, transcripts, replay) |
| `fixtures.rs` | runtime loading of `crates/harvester/fixtures/*.jsonl` and folding them into `Transcript`s |

### The gpui-mcp bridge

`main.rs` calls `gpui_mcp_bridge::install(cx)` inside the `run_desktop_shell`
closure. This lets the gpui-mcp MCP server attach to a running gallery and
drive it — dump the accessibility tree, click via AT-SPI, inject keys — which
is how agents UI-test the gallery. Deliberately gallery-only: the client
binary never calls `install`, so shipping builds carry no bridge socket. The
bridge itself is inert unless the process is a debug build launched with
`GPUI_MCP=1`, so a plain `cargo run` is unaffected. The bridge's
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
`ButtonGroup` `Button`s reporting `toggled=`. `EXPECTED_STORY_COUNT` (14) and
`EXPECTED_GROUP_COUNT` (6) in the script are floors, not equalities: adding a
story must not fail the walk, but a story or a whole group disappearing
should. `GALLERY_BINARY` points into `crates/harvester-gallery/target/debug/`,
because the quarantined crate builds into its own target dir.

Two things about the walk are worth knowing before changing it. It expands
exactly one story group at a time and collapses the rest, because fourteen
stories plus six headers do not fit in a 720px window and the MCP has no
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

## crates/harvester-gallery/src/story.rs

The model, deliberately data-only so a story declaration reads as a table:

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
- `Story` is name + group + knobs + presets + a plain `fn` pointer, built as
  a bare struct literal — the five-argument positional constructor was
  dropped so every declaration reads by field name instead of by comma
  position. A `fn`
  (not a boxed closure) is what keeps the registry a static list: a story
  cannot capture state, which is exactly the property that forces shared live
  state into `StoryRuntime` where the gallery can observe it. It is also what
  lets each `stories/` module build its own list and the registry merely
  concatenate them.
- `StoryScene` is what the render `fn` receives: the resolved knob values plus
  the `Entity<StoryRuntime>`. This is the one deviation from the phase plan's
  sketched signature (`&KnobValues` alone): live stories (collapse, tool-block
  expansion, dropdown selection, replay, the real `InputState`) need state
  that survives across frames and can notify the view, and in gpui that means
  an entity handle, not a value.

## crates/harvester-gallery/src/registry.rs

The registry is a *concatenation*, not a catalog: `build_stories()` chains the
seven per-module `stories()` functions in sidebar order and flattens them. It
used to be one 400-line `vec![]` of `Story::new(...)` living a file away from
the render functions it named, which forced every knob name to be a `pub`
const so the two files could not disagree about a string — about thirty-five
of them. Declaring a story next to the function that renders it deletes that
whole coupling: knob names are private consts again, and the only thing the
registry knows is the order groups appear in.

- `group_names()` derives the sidebar's group order from the story order, so
  adding a story to a new group needs no second edit. Two modules
  (`prompt_strip.rs`, `transcript.rs`) share `FLAGSHIP_GROUP`, and the
  concatenation keeps them adjacent, which is what keeps that derivation
  honest.
- `story_index()` resolves a story *name* to its index once at start-up. Its
  one caller is the replay loop, which keys on the index instead of comparing
  names per tick; if `transcript::STREAMING_TRANSCRIPT_STORY` is ever renamed
  the lookup returns `None` and the replay simply never runs — no panic, no
  silent tick on the wrong story.
- The `tests` module pins the walk's two floors in-process:
  `EXPECTED_STORY_COUNT` (14) and `EXPECTED_GROUP_COUNT` (6) mirror the
  same-named constants in `scripts/gallery_walk.py`, and the two tests assert
  `build_stories()` and `group_names()` stay at or above them. Same floor
  semantics as the walk — adding a story passes, losing a story or a whole
  group fails — but checked by `cargo test` inside the crate on a headless
  machine, not only when a display can run the walk. When the floors move,
  bump both files together.

Naming conventions the catalog holds itself to (they outlived the file split):
group names are sentence case ("Core", "Flagship") because `section_header`
stopped uppercasing its title and a heading is user-visible text under the
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

## crates/harvester-gallery/src/stories/

One module per `ui` module plus the two flagships. Each module declares its
own stories in a `pub fn stories() -> Vec<Story>` at the top of the file and
renders them below, so a knob name is a private const read by exactly the two
places that can disagree about it — the spec and the render function — and
they sit twenty lines apart.

`stories/mod.rs` holds what every story shares. The group-name consts
(`CORE_GROUP` … `DIALOG_GROUP`) live here rather than in the modules because
`FLAGSHIP_GROUP` is shared by two of them and `collapsible_section` demands
`&'static str`. The tone vocabulary is derived rather than restated:
`harvester::ui::Tone::ALL` is the variant order, `tone_labels()` maps it
through `Tone::label`, `tone_at` is the choice-index → `Tone` mapping six
stories use, and `tone_index` is its inverse, so a preset says
`tone_index(Tone::Live)` instead of a bare `4`. The gallery used to keep its
own `TONES` array *and* a parallel `TONE_LABELS` list; both were the variant
order written down a second time, and an earlier lowercase labels list had
already drifted from the app ("muted"/"active" where the app says
"Neutral"/"Running"). `Tone::ALL` is the app's own const, so the catalog can
no longer disagree about either the order or the words.

Also here are the layout primitives `story_column`, `story_row`, `caption`
(an `inline_note`, so captions obey `Tone`), `panel` and `transcript_panel`.
`panel` sits on the `popover` slot with `rounded_lg` and `shadow_sm` — the
raised-surface tier of the graphite ladder — so a story's framing matches
how real raised components (tool cards, popovers) sit on the canvas.
`PANEL_MIN_WIDTH` (320) keeps a one-line component from collapsing to its
text width on the stage; `TRANSCRIPT_PANEL_HEIGHT` (420) gives the transcript
stories a fixed viewport so their internal scroll behaves like the chat's.

- `core.rs`: the tone swatch grid (all six tones × the derived dot × the
  tone's own `label()` × colored text, `w_20` lanes aligning both label
  columns) and the single-dot story. Neither story picks a dot shape any more:
  the swatch shows the one dot `status_dot_for_tone` draws, and the dot story
  *reports* the shape `Tone::is_hollow` decides (`derived_dot_label`) instead
  of letting a knob set it — a catalog whose controls can produce a
  combination the app cannot render is lying about the component. The story is
  still named "Dot" after the concept; the `ui::Dot` type it used to match on
  is gone, and one bool is all the derivation ever was.
- `note.rs`: `inline_note` and `empty_pane_hint`; the hint gets an `h_48` box
  because it is a `size_full` component and would otherwise have no height to
  fill on the stage, and it is rendered *with* an `empty_pane_action` child so
  the story shows the trailing action slot rather than only the sentence.
- `shell.rs`: `chat_header`, `collapsible_section` (live — the expanded flag
  lives in `StoryRuntime`, so the story demonstrates the real click), and
  `statusbar_frame`. Section titles come from a *choice* knob rather than a
  text knob because the component's signature is `&'static str`. The
  statusbar's trailing caption is a bare machine name (`exe-vm-7fa2`,
  matching the header fixture) because that is what the app's
  `machine_presentation` renders for a connected machine — an earlier
  `"harvesterd · connected"` fixture was gallery drift from a link text the
  app never shows.
- `sidebar.rs`: the three sidebar builders inside a `w_64` frame tinted with
  the sidebar theme slots (the app's own sidebar width — the gallery should not
  invent a second sidebar metric), because a row's selected state is only
  legible against the surface it really sits on. Both row stories chain
  `keyboard_focus_ring` so the story can be Tab'd into, and `row` mirrors the
  app's `tab_stop(!disabled)` rule. The stacked-row story carries a caption
  saying the dot shape is derived from the tone rather than chosen per row,
  because that is the question the retired `solid` knob used to answer.
- `prompt_strip.rs` (flagship): builds the whole `PromptStrip` value struct
  from knobs, with seven presets (`ready`, `turn running`, `loading models`,
  `models failed`, `no machines`, `starting session`, `exited`). The dropdown
  rows are static label lists and both `send` and `stop` are no-op closures:
  the story exists to show every *state* of the strip, and a working picker
  would need a store. `turn running` is the one preset with the Stop button
  on screen; its knob is named `turn running` rather than `can stop` because
  what the app decides is whether a turn is in flight — `can_stop` is the
  consequence, and a knob that states the consequence would let the story
  show a Stop button in a state the app cannot reach.
  It installs no `on_action` listener either, so a pick opens and dismisses the
  real menu and changes nothing — which is the honest rendering of a strip with
  no chat behind it. The input is a real `Entity<InputState>` from the runtime,
  so typing, focus ring and auto-grow are the genuine article.
  The two rows that are not sample data — the repository list's trailing
  `ui::ADD_REPO_ROW` and the model list's leading `ui::DEFAULT_MODEL_ROW` —
  are the app's own consts, not copies. They used to be copies, and they had
  drifted inside the pass that shipped them: the catalog said "Add a folder…"
  and offered an "Add a model…" row the app has never had, which is exactly
  the failure mode a preview surface is supposed to prevent. Three narrower
  prompt stories (`prompt_box_control`, `prompt_box_send`, a live
  `prompt_box_dropdown`) were deleted with the same change: the strip renders
  all three components in the composition a screen actually uses, and their
  group was the catalog's only group that showed parts no screen assembles by
  hand. The `prompt_box_*` builders stopped being exported from `ui` with
  them.
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
  The cancel button's word is `ui::dialog::CANCEL_LABEL`, the app's own const,
  for the same reason `ADD_REPO_ROW` and `DEFAULT_MODEL_ROW` are: the gallery
  constructs no component *and no copy* the app constructs itself. Its
  neighbour `CANCEL_LABEL_KNOB` stays local — that is a knob name, which is
  gallery chrome, not user-facing copy.
  The option list is painted as a plain column of `option_row`s rather than as
  a live `List`, with one row forced selected so the selected style is reviewable:
  a story render function runs on every frame and cannot own a list state
  without putting one in `StoryRuntime`, and a list state rebuilt per frame
  could never hold a selection to show. The rows are the dialog's own builder,
  so what the story shows is honest; what it cannot show is scrolling and
  keyboard traversal.

## crates/harvester-gallery/src/runtime.rs

`StoryRuntime` is the single entity holding everything a story needs to
survive a frame: the collapsible section's flag, the prompt strip's
`InputState`, the dialog story's two inputs, the rendered transcript, and the
replay. `dropdown_selection` left with the three prompt sub-stories — nothing
else ever drove a live menu. The gallery `cx.observe`s it once, so any story
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
- The prompt strip's input is `harvester::ui::create_prompt_input(window, cx)`
  — the app's own factory, not a lookalike. The gallery used to build it here
  from a `PROMPT_INPUT_PLACEHOLDER`/`PROMPT_INPUT_ROWS` pair it named itself
  while `chat.rs` hardcoded the same four builder calls, which is the
  reconstruct-the-component-you-preview failure the catalog exists to prevent;
  both consts moved into `ui/prompt.rs` beside the factory and are private
  there. Only the two dialog strings are still declared here
  (`DIALOG_SINGLE_LINE_PLACEHOLDER`, `DIALOG_MULTI_LINE_PREFILL`): they are
  *sample content* for `ui::dialog`'s two input factories, which take their
  text as an argument, so there is nothing on the app side to agree with.
- `RenderedTranscript::transcript` and `ReplayTranscript::transcript` both
  answer `Result<&Transcript, &str>`. The failure half used to be an owned
  `String`, which cost a `clone` on every frame that had a broken fixture
  purely to satisfy the signature; the caller only ever hands it to
  `ui::empty_pane_hint`, which copies it into a `SharedString` anyway.

## crates/harvester-gallery/src/fixtures.rs

Fixtures are loaded **at runtime**, from
`env!("CARGO_MANIFEST_DIR")/../harvester/fixtures/*.jsonl`, not
`include_str!`'d. The gallery is a dev tool that only ever runs from this
repo, and runtime loading means capturing a new fixture is visible on the next
`cargo run` without a rebuild — and, more importantly, a missing or malformed
file degrades instead of failing the build for everyone.

`APP_CRATE_FIXTURES_DIR` reaches across to `crates/harvester/fixtures/`
rather than the gallery keeping a copy: the same three captures are the
subject of `crates/harvester/tests/fixtures.rs`, which is the transcript
layer's LF-framing and folding contract test and belongs to a workspace
member. One copy on disk, read by a test that parses it independently and by
a dev tool that renders it, is the arrangement that keeps them honest — see
the provenance section in `docs/internals/harvester.md`. The `..` in the path
is the price of the quarantine; it is joined onto `CARGO_MANIFEST_DIR`, which
cargo expands to an absolute path at compile time, so the working directory
the gallery is launched from still does not matter.

- Every failure path produces a `String` naming the offending path (and line
  number for a parse failure) that the story renders through
  `ui::empty_pane_hint(Tone::Failed, …)`, and the statusbar carries the first
  failure of any fixture at all times. Nothing panics: the whole point of a
  catalog is that the other stories still work when one fixture is gone.
- Parsing follows the house LF-only framing rule (`harvester_proto::{LF, CR}`,
  split on `LF`, strip one trailing `CR`), the same as everything else that
  reads pi JSONL. `read_events` is only the `read_to_string` half; the framing
  and the line-numbered failure message live in `parse_events`, which takes
  the document as a `&str`. That split is what lets the framing be unit-tested
  without a fixture on disk, which is the whole reason the module has tests
  now.
- **The user-prompt rule**: `Transcript::apply` deliberately ignores
  `message_end` events whose `message.role` is `"user"` — in the app, user
  text enters a transcript only through `push_user_prompt`, because the app
  already knows what the user typed. A replayer has no such knowledge, so
  `apply_event` intercepts exactly that event shape and calls
  `push_user_prompt` with the text `harvester_client_core::transcript::message_text`
  reads out of it. Without it the mixed fixture renders assistant turns and
  tool blocks with no prompts above them. This is the one place the gallery is
  allowed to interpret a pi payload, and it interprets it into the same call
  the app makes.
- `message_text` is **client-core's**, not a copy. The gallery had its own for
  one release and it had already drifted: it joined text blocks with `""`
  instead of a blank line and handled only the array shape of `content`, so a
  multi-block pi message previewed without its paragraph breaks and a
  bare-string prompt previewed as nothing at all — in the one surface whose
  job is to be an honest preview. Client-core made the function `pub`; the
  copy is gone. Any further pi-payload reading the gallery needs should arrive
  the same way rather than be written here.
- The tests are inline (`#[cfg(test)] mod tests`) and cover the pure half of
  the module: LF splitting, the trailing-`CR` strip, the one-based line number
  in a parse failure, the three `user_prompt_text` rejections, both content
  shapes a prompt can arrive in (the two the drifted copy got wrong), the
  failed-tool filter, and `TranscriptSource::at`'s fallback. Nothing here
  touches the disk, so they run on a headless machine alongside
  `registry.rs`'s two walk floors.
- `TranscriptSource` maps the four preset/knob choices onto fixtures:
  `markdown-heavy answer` → `streaming.jsonl`, `tool run` → `tools.jsonl`,
  `mixed session` → `mixed.jsonl`, and `failed tool` → `tools.jsonl` filtered
  to the tool-call ids whose `tool_execution_end` carried `isError: true`.
  The filter exists so the error path gets a story of its own without a
  second capture: it keeps every event that names a failing call and drops
  the rest, which is enough for the block to render exactly as it does in a
  full session.

## crates/harvester-gallery/src/main.rs

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
- Text knobs are backed by real `Entity<InputState>`s held in one map per
  story — `text_inputs` is a `Vec` indexed by story, each element a
  `HashMap` keyed by knob name — and created **eagerly**, in
  `create_text_knob_inputs` from `Gallery::new`: every text knob of every
  story gets its input before the first frame. They used to be created lazily
  during `render`, on the theory that only a shown knob has a `Window` — but
  `Gallery::new` has one too (it is called inside `open_window`), and the lazy
  version cost a mutable pass over the selected story's specs on every single
  frame plus a `text_inputs` lookup per knob to find the ones already made.
  Creating them once is fewer moving parts and makes `render` free of state
  mutation. The catalog is a fixed, single-digit-per-story list, so there is
  no growth to defer. Each input gets a `cx.observe` subscription, which is
  what makes typing in a knob update the story on the same keystroke.
  `selected_knob_values` then reads the *input* rather than the stored value
  for text knobs — the input is the source of truth, so there is no mirror to
  keep in sync. Applying a preset writes both, since that is the one direction
  the input cannot learn about by itself.
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
- The `PLAYING_KNOB`/`SPEED_KNOB`/`STREAMING_TRANSCRIPT_STORY` import from
  `stories::transcript` is the only place the gallery shell knows anything
  about a specific story; it is the price of driving a story's animation from
  the shell's async task. Those three are the only `pub` items a story module
  still exports beyond its `stories()` function.

## crates/harvester-gallery/src/chrome.rs

The gallery's own screen, kept out of `main.rs` so state and layout stay
separable. It is a sibling module of the crate root, which is why it can read
`Gallery`'s private fields through accessors without those fields becoming
`pub`.

Naming here follows `ui/transcript.rs`'s rule rather than a blanket prefix:
**`render_x` when the function takes a domain value, a noun when it does
not.** `render_gallery`, `render_story_sidebar`, `render_story_group`,
`render_story_row`, `render_stage`, `render_control_dock`, `render_knob`,
`render_preset_button` and the three knob-kind builders are all handed a
story, a spec or a knob value and are named for the act of rendering it;
`stage_canvas`, `preset_buttons`, `knob_grid` and `statusbar` are frames and
are named for the thing they are. The four used to carry the prefix too,
which made the prefix mean nothing — it was on all fifteen functions in the
file. `with_content_inset` / `with_dock_row_inset` sit outside the rule
entirely: they are decorators, and their names say so.

Element ids built from a knob name are handed to `.id()` / `ButtonGroup::new`
as the `format!`'s `String`. `ElementId: From<String>` and
`ElementId: From<SharedString>` are the same constructor, so the explicit
`SharedString::from` wrapper the file used to carry named a conversion the
signature already asks for.

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
  when a story has a dozen knobs — the prompt strip does — and scrolls past
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

## Recorded gaps

- **Dialog stories are chrome only.** `dialog chrome` renders the
  confirmation body, the button row and both inputs inline on the canvas; it
  never calls `window.open_dialog`. That is deliberate (plan Q10a): the dialog
  visual redesign is a follow-up, and it will be done *inside* the gallery,
  against these stories. Modal behavior (focus, escape, the Root dialog stack)
  is `workbench/modal.rs`'s business and is not what a component catalog is
  for.
- **Light mode is blocked.** `ui/transcript.rs` memoizes its `TextViewStyle`
  in a `OnceLock` with `is_dark: true` and a dark highlight theme, so a theme
  toggle would leave the transcript rendering dark inside a light window. The
  gallery is therefore dark-only for now (`shell_bootstrap` sets `Dark`), and
  the fix and the gallery's theme toggle land together, later — a toggle that
  half-works is worse than none in a tool whose job is to tell you what a
  component looks like.
