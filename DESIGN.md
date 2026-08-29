# Harvester — Design

> This is the product contract for how Harvester looks and how it decides what
> things look like. It describes what shipped, not what is planned.
>
> - The **code is the authority**. Where this document and `crates/harvester/src/ui/`
>   disagree, the code is right and this document is stale.
> - Implementation notes — why a particular function is shaped the way it is,
>   what a const means, what breaks on a library bump — live in
>   `docs/internals/harvester.md`, not here. This file states rules; that file
>   states mechanics.
> - The external standard is the **gpui-component design guide**. Harvester
>   conforms to it. Where it deliberately diverges, the divergence is named
>   below with its reason.

Harvester is a desktop shell for watching coding agents work on remote machines.
The screen is a sidebar of machines and sessions, a chat pane, and a status
band. The user reads a transcript for minutes at a time and acts on it rarely.
That is what the visual system is tuned for: **quiet, cool, near-monochrome,
dense but not cramped, with colour spent almost nowhere so that the few places
it appears mean something.**

---

## 1. The rem axis

`BASE_FONT_SIZE = 14.` in `ui/theme.rs` is the single scaling number in the
visual layer.

In gpui the base font size *is* the rem unit: `gpui_component::Root::render`
calls `window.set_rem_size(cx.theme().font_size)`, so the theme's `font_size`
slot drives every rem helper — `px_3`, `gap_2`, `h_8`, `max_w_56`, `size_3p5`,
`text_sm`, `rems(...)` — for the library's own controls as much as for ours.
`mono_font_size` is set to the same const so a code run and the prose around it
share one line box.

**The rule: application layout does not call `px(...)`.** Product geometry —
padding, gaps, control frames, icon sizes, dialog widths, list heights — is
expressed on the rem scale so that changing one number rescales the whole app
coherently. This is why the app has no per-component sizing constants: the scale
*is* the constant.

The exceptions are physical boundaries, not layout, and each is named:

| Value | Where | Why it is pixels |
| --- | --- | --- |
| `FOCUS_RING_SPREAD` 1.5px, `FOCUS_RING_OFFSET` 0 | `ui/shell.rs` | A focus ring is a hairline, in the same class as a 1px border. It must stay one crisp ring at any base font size. |
| `STATUS_DOT_DIAMETER` 6px | `ui/core.rs` | A raster boundary: 6px is the smallest disc that still antialiases into a circle, and the rem steps on either side land it mushy or off the half-pixel grid. The dot should keep its physical size when the type grows; the lane it sits in scales instead. |
| `MAIN_WINDOW_WIDTH/HEIGHT` 1100×720 | `main.rs`, mirrored by the gallery | The OS window's physical bounds, not layout. |

A fourth row used to sit here for the flexbox shrink constraint, written
`min_w(px(0.))` / `min_h(px(0.))` in `workbench.rs` and `chat.rs`. It was never
an exception, only a second spelling: gpui ships `.min_w_0()` / `.min_h_0()` for
exactly that constraint, six other sites already used them, and the three
stragglers now do too. A shrink constraint reaches the layout engine as the
keyword it is, and nothing in the app writes `px` for it.

Product and gallery framing stays on the same axis too: the folder picker's
entry list is `20rem`, the gallery's story panel is at least `23rem`, its
transcript viewport is `30rem`, and the prompt-strip story is `61rem` — the
inner width of the chat pane at the default window size. Beyond the raster
boundaries and OS-window bounds in the table above, visual layout is not
measured in pixels.

Corner radii are the deliberate counter-case: they come from the theme
(`cx.theme().radius` / `radius_lg`) rather than from gpui's `rounded_md`/
`rounded_lg` helpers, precisely *because* those helpers are rem steps. A radius
is a property of the frame, not of the text inside it, and a corner that opens
up when you zoom the type is wrong.

---

## 2. Typography

**Two sizes.**

| Tier | Token | Size at rem 14 | Used for |
| --- | --- | --- | --- |
| Body | inherited theme `font_size` / `text_base` | 14px | message prose, row titles, empty-pane hints, dialog bodies, composer text, standard action labels |
| Secondary | `text_sm` | 12.25px | section headers, pane subtitles, sidebar detail lines, inline notes, the status band, the whole tool-call card, prompt selectors and branch |

`text_xs` is not used anywhere in the crate. At rem 14 it renders 10.5px, under
the guide's ~12px typography floor, and the smallest text the app draws is also
the text that most needs to stay legible (code, diffs, status). `text_sm` is the
app's one secondary tier; something that needs to recede further than that
should leave the surface or move behind a disclosure.

The body tier is also the **standard control baseline**. `Button`'s medium
default already renders `text_base`, and Harvester's `text_input` factory
corrects the library's medium `Input`, which otherwise renders `text_sm`.
Prompt selectors are the one compact control tier: model, machine, repository
and the read-only branch use `text_sm`, while the branch `Tag` explicitly
corrects the library's fixed `text_xs` treatment upward. Editable text and
commit actions therefore stay at 14px; compact selection context sits one tier
below, never under the typography floor. Shell chrome reaches that secondary
tier through one `secondary_text` builder: a sidebar's transient “opening…”
note and detail, the chat-header subtitle, and every statusbar item therefore
resolve to the exact same 12.25px.

**Three brightness tiers, and the set is closed.**

| Tier | Accessor | Resolves to | Used for |
| --- | --- | --- | --- |
| Primary | `cx.theme().foreground` | `TEXT_BRIGHT` (94%) | body text, a tool block's subject |
| Muted | `dim()` / `Tone::Muted` | `muted_foreground` (63%) | secondary labels, subtitles, a tool block's verb and output |
| Faint | `faint()` / `Tone::Idle` | `muted_foreground` at `FAINT_TEXT_OPACITY` (0.6) | tertiary run: context lines, stats, disclosure chevrons, section header at rest |

`FAINT_TEXT_OPACITY` is the single number behind the third tier and the lowest
step that still clears comfortable legibility against the graphite canvas. **Do
not add a fourth tier.**

**Weight is not part of the ladder.** `FontWeight::MEDIUM` appears exactly once
in the app, on the chat pane's title. Level is carried by size, brightness and
spacing; adding weight on top of them is spending a third device on a job two
already do.

**Line height carries the prose/code distinction.** Message bodies set
`line_height(relative(1.7))`; tool-block bodies set `1.45`. It is the *ratio*
that means something — prose wants air, code reads as one block — not either
number alone. Both are set on a wrapper because `TextViewStyle` has no
line-height field and gpui text style inherits.

**Fonts are theme slots, never resolved at a call site.** `UI_FONT` is Inter and
`MONO_FONT` is Geist Mono, both set on the `ThemeConfig` so `Root` styles the
window and gpui-component's widgets agree with our own mono runs. `ui/core.rs`
reads the slot back (`mono_font_family`) rather than naming a family. gpui
matches families by exact name; a machine without them installed substitutes and
degrades rather than failing. Monospace is for code, identifiers, paths and
commands — nothing else.

---

## 3. Colour

The theme is **Graphite Dark**: a cool near-monochrome where every surface is
the same hue-240, 5%-saturation gray at a different lightness. It replaces
gpui-component's pure-neutral dark theme.

**Hex literals exist in exactly one file: `ui/theme.rs`.** No other module in
the crate contains a colour literal of any kind. Everything else speaks in theme
slots or in `Tone`. That is what makes the rule checkable by reading one file.

The theme ships as a `ThemeConfig` applied through `Theme::apply_config` — not
as field-by-field mutation — because `apply_config` derives every slot we do not
specify (button variants, `list_hover`, table colours) from the ones we do.

### The elevation ladder

Darkest is furthest back.

| Const | Lightness | Theme slot | Surface |
| --- | --- | --- | --- |
| `CHROME` | 7% | `sidebar`, `title_bar`, `status_bar` | window chrome, the gallery's control dock |
| `CANVAS` | 9% | `background` | the content background |
| `SURFACE` | 12% | `popover` | popovers, menus, tool-call cards, story panels |
| `HOVER` | 14% | `accent`, `sidebar_accent` | row hover fill |
| `INPUT_WELL` | 15% | `input` | the prompt box fill, input borders |
| `RAISED` | 16% | `muted`, `secondary`, `list_active` | message bubbles, tags, secondary buttons |
| `RAISED_HOVER` / `RAISED_ACTIVE` | 19% / 22% | `secondary_hover` / `secondary_active` | interaction states on raised controls |

Borders sit on their own two-step ladder: `HAIRLINE` (18%) against content
surfaces, `CHROME_HAIRLINE` (13%) where chrome meets chrome. Chrome is darker,
so its separators have to be darker too, or they read as lines rather than
edges. `EDGE` (30%) is the visibly-outlined tier — the selected list row's
border and the text-selection fill. Selection stays monochrome on purpose: the
default theme's blue selection was the one saturated surface in an otherwise
gray app.

`PRIMARY` stays near-white (93%, hover 97%, press 86%). Filled controls are
light-on-graphite; status colour never comes from a filled control.

### `Tone` is the status vocabulary

Components ask for a **meaning**, never a hue. `Tone` is the enum that meaning
is expressed in, and it is the only path from a state to a colour.

| Tone | Means | Colour role | Dot shape | Word |
| --- | --- | --- | --- | --- |
| `Muted` | neutral, no lifecycle meaning | `muted_foreground` | solid | Neutral |
| `Idle` | genuinely unknown — never probed, never connected | `muted_foreground` faded | hollow | Idle |
| `Active` | the live thing the user is watching right now | `info` | solid | Running |
| `Pending` | asked for, not begun; outcome unknown | `warning` | hollow | Pending |
| `Live` | confirmed good — connected daemon, running session | `success` | solid | Live |
| `Failed` | failed, and it must be visible | `danger` | solid | Failed |

Three invariants hold here:

1. **No arm of `Tone::color` may name a palette entry.** `info`, `warning`,
   `success`, `danger` and `muted_foreground` are roles; `blue`, `red`, `green`
   are hues. Only a role survives a theme swap. (`Active` was `theme.blue`; that
   was the bug this rule was written for.)
2. **Shape is derived from the tone, never chosen per call site.**
   `Tone::is_hollow` owns the mapping and `status_dot_for_tone` is the only
   builder that reads it, so there is nothing to construct and nothing to pass. Hollow means the dot is a placeholder for work that has not
   begun; solid means it stands for something real — running or finished, good
   or bad. That is why `Failed` is solid: a settled terminal state must not read
   as "not started".
3. **Every tone carries a word.** `Tone::label` is the tone's one user-facing
   string, and it is what a dot's tooltip and accessible name say. Two of the
   six words are not the variant's own name on purpose: `Active` speaks as
   "Running" because that is the word the transcript and the session list
   already use, and `Muted` speaks as "Neutral" so the tone with no lifecycle
   meaning cannot be confused with `Idle`, which has one.

Where a surface needs a *structural* colour — a border, a popover fill, a
selection — it reads the theme slot directly. `Tone` is for status, not for
paint.

That splits cleanly by module, and the split is greppable. Inside
`crates/harvester/src/ui/` a component may read any theme slot it needs: that is
where paint is decided. Outside it, `cx.theme()` is for structure only —
`sidebar`, `sidebar_border`, `background`, `foreground`, `border`, `radius`,
`font_size` — and every *text* colour comes from a `Tone`. The folder picker's
location line was the last exception, reading `cx.theme().muted_foreground` for
a caption; it asks for `Tone::Muted` now, which resolves to the same slot and
states the meaning instead of the hue. That file had already been corrected once
for the same class of mistake (its error line moved off `cx.theme().danger` onto
`Tone::Failed`) and one call site was missed, which is why the rule is written
as a grep rather than as a habit.

### Colour is a scarce budget

Semantic status colour is spent only where the status **is** the data. In the
transcript that means the diff lines inside an expanded tool body, and nowhere
else: line-count stats are one faint `+3 −1` run rather than a green `+` beside
a red `−`, because counts are metadata about an edit, not a verdict on it. A
settled tool call maps to `Tone::Muted`, so the common case paints no colour at
all, and a finished tool card has exactly one focal point and exactly one
coloured element in its header — the dot. Colour in the transcript means
"running" or "broken" and nothing else.

The one deliberate splurge is `render_failure`, which paints a whole paragraph
in `Tone::Failed`. A failed turn is precisely where the emphasis budget should
be spent, and it is rare.

### Dark only

The app is pinned to dark mode, and this is a blocker rather than a preference.
`ui/transcript.rs` memoizes its `TextViewStyle` in a `OnceLock` with
`is_dark: true` and a dark highlight theme, so a theme toggle today would leave
the transcript rendering dark inside a light window. The theme config is stored
back into `theme.dark_theme`, so a future light theme slots in beside it as a
second config — but the memoized style has to be fixed in the same change as the
toggle. A toggle that half-works is worse than none.

---

## 4. Density

Harvester sits at the guide's **standard** tier and treats it as the default.

The chrome used to be deliberately compact — a fixed 24px status band,
`pt_2`/`pb_1` section headers, `py_1p5` on the chat header — on the theory that
chrome must never compete with content for height. At rem 14 that stopped being
tight and started being cramped: a 24px band around a 12.25px line leaves under
6px of air on each side, which reads as a clipped strip rather than a deliberate
one. Density now comes from the rem scale, not from squeezing individual bands.

What the standard tier looks like in practice:

- the status band is `h_8` (2rem) with `text_sm` content;
- the chat pane header is `py_3`, its subtitle `text_sm`;
- section headers are `pt_4`/`pb_2` — a deliberate 2:1 asymmetry, so a heading
  binds downward to the list it labels;
- sidebar rows are `py_2`, enough that a two-line stacked row does not run into
  the row below it;
- the transcript column is `gap_4` / `py_4`, because it is the one surface the
  user reads continuously.

**Density is a property of hierarchy before it is a property of size.** The
composer and actions keep the 14px body baseline. The three prompt selectors
are compact context about where the message will go, so they and the read-only
branch take the secondary tier without shrinking below it.

Standard actions use the library's **medium default**. At rem 14 that means a
28px (`h_8`) button with 14px text: send, stop, New session, dialog actions,
empty-state actions, row actions, and gallery controls share one baseline.

Prompt selectors use `.small()` for coordinated 12.25px text, icon, padding and
caret, then correct only the library's overly short 21px frame to `1.875rem`
(26.25px). The result is one rem half-step — 1.75px — below the standard
control in both type and height. Icon and label use `gap_1p5` (5.25px): the
picker adds the final half-step as icon margin because the library fixes small
buttons at `gap_1`, while the branch can set the gap directly. The branch `Tag`
uses the same frame and `text_sm`; its icon is `size_3p5`. This explicit text
correction is necessary because the library renders Tag text as `text_xs` at
every nominal size.

The folder picker's modal search field is the deliberate size exception. It
uses the library's large `List` because that is the only supported size that
renders the built-in filter at `text_base`; medium silently falls to
`text_sm`. A primary search field may carry the larger frame, but not smaller
text.

**The product gap ladder is closed:**

| Token | Size at rem 14 | Relationship |
| --- | --- | --- |
| `gap_1` | 3.5px | lines or fragments inside one code body |
| `gap_1p5` | 5.25px | icon and label inside one compact selector |
| `gap_2` | 7px | peers in one row: controls, header parts, dialog actions |
| `gap_3` | 10.5px | related blocks inside one compact surface |
| `gap_4` | 14px | distinct content items or groups |

Padding uses the same rem steps. The prompt controls row starts at `pl_2`, a
visible 7px margin between the prompt frame and the first picker's hover
surface; its trailing edge remains `pr_3` so the send button aligns with the
input above. Use the component's own size API wherever it changes typography,
icon and padding together. A frame correction may follow that complete size
change, as the compact prompt controls do; a custom height that leaves the
other metrics behind is incomplete.

---

## 5. Elevation, radius and structure

**Depth comes from the ladder plus hairline borders, not from decoration.** The
base window surface is flat. Fill and a hairline are what place a surface on the
ladder; a shadow is reserved for something that genuinely floats.

Nothing in the app's own in-flow content carries a shadow. A tool-call card sits
on the `popover` slot with a hairline border and no shadow at all — a transcript
can hold a dozen of them in a row, and a dozen stacked `shadow_sm` cards read as
clutter rather than as depth. The prompt box likewise dropped its `shadow_md`:
with the graphite ladder the input well already reads as raised against the
canvas, and a shadow under a control that also fades its own border was two
devices doing one job. Elevation stays with the popup families the library
already themes — menus, popovers, dialogs — which share one popover surface so
they cannot drift apart.

**Two radii, both from the theme.** `CONTROL_RADIUS` (6) is `cx.theme().radius`:
buttons, user message bubbles, the dialog option list. `PANEL_RADIUS` (10) is
`cx.theme().radius_lg`: the prompt box, tool-call cards, dialogs. Controls stay
tight, large surfaces round more, and the density lives in spacing rather than
in sharp corners.

**Alignment is structural.** A region's siblings resolve their leading edge from
one owner rather than each inventing a literal:

- The chat column is `px_3` throughout — `chat_header`, the transcript, and
  `prompt_strip` all take the same step, so three siblings in one pane cannot
  drift apart. The transcript owns that padding outright; neither the chat pane
  nor the gallery's transcript panel adds any of its own. Inside the prompt
  box, the input is `px_3`; the controls row deliberately uses `pl_2` so the
  first compact picker's hover surface has a visible left margin, while the
  icon-only send button's trailing edge resolves directly from `pr_3`.
- The sidebar resolves from one pair: `SIDEBAR_EDGE_GUTTER` (0.25rem) and
  `SIDEBAR_ROW_PADDING` (0.75rem). Their computed sum,
  `SIDEBAR_CONTENT_INSET` (1rem), is the section title/chevron spine and the
  outer margin of every selectable row, so a selected pill's two borders land
  on those content edges. Rows keep their own inner padding and therefore read
  as children of the section rather than competing with its title. The New
  session wrapper reads the edge gutter while its medium button supplies
  `px_3`; together they complete the same 1rem inset exactly.
- `SIDEBAR_STATUS_LANE` (1rem) is a fixed functional lane. The status dot sits
  at its leading edge and the stacked row's detail line is indented by it, so
  the dot's diameter can change without moving a character of text. The New
  session action is a medium control inside symmetric `py_2`, so the sidebar's
  primary entry point has the same type and frame as every other action without
  losing the row spine.
- The gallery's stage, preset row, knob grid and empty notes all pass through
  one `with_content_inset` function.

**Regions are specified by three constraints**, not by one number back-solved
from a screenshot: a minimum at which the task still works, a comfortable
default, and a rule for surplus. The app's sidebar is `w_64`, `flex_none` — it
never grows or shrinks, and the chat pane takes everything else. The gallery's
sidebar is `w_64` with `min_w_56` / `max_w_80` and `flex_shrink_1`: it yields
down to its minimum before the stage gives up anything, and consumes no surplus,
which is what "visibly subordinate to the work area" means in layout terms. Its
stage measure is `rems(64)` — four times the sidebar's preferred width, chosen
as a relationship rather than as a pixel count.

---

## 6. Component choice

**Use the library's widget. Do not hand-roll one.** This is the rule with the
best evidence behind it, in both directions.

The prompt strip's pickers were once styled `div`s: eight sizing constants, a
hand-rolled popover with its own row rendering and check-mark gutter, and a
`PlainTrigger` newtype whose entire purpose was to no-op `Selectable` so a plain
`div` could satisfy `Popover::trigger`'s bound. It reimplemented chrome the
library already ships, and reimplemented it subtly wrong: no keyboard operation,
no tooltips, no menu navigation, and a trigger that never looked open. `Button`,
`PopupMenu` and `Tag` deleted all of it and brought arrow-key/Enter/Escape
navigation, focus transfer and a visibly-pressed trigger with them.

The gallery made the same mistake and paid for it more visibly. Its knob
controls were a hand-rolled `chip()` — a `div` with its own primary/secondary
fills, hover and press states, and `text_xs` — plus a pair of id slots for the
on/off variant. The old argument was that a dev tool should not grow the
library. But the chip was never the library's to grow: it was three
gpui-component widgets reimplemented worse, and it was the *dogfooding* rule
that decided it. **A catalog whose own controls are hand-rolled is not
exercising the thing it catalogs.** A choice knob is now a `ButtonGroup`, which
brings real segmented-control semantics and reports its toggled state without
being asked; a preset is an `outline` `Button`, because a preset is a command
rather than a state; a text knob is an `Input`.

The same rule caught a subtler case. A bool knob was briefly a `Switch`, on the
correct-sounding argument that a knob takes effect immediately and that is what
a switch means. It went back to a two-segment `ButtonGroup` for one
disqualifying reason: `Switch` in this library revision tracks no focus handle,
sets no tab stop and fires from `on_mouse_down` only, so it was the single
control in the dock unreachable by keyboard. **Semantics lose to keyboard
reach.** A bool is a two-way choice, so rendering it as one deletes the special
case instead of adding a workaround.

Pick the component by its **semantic role**, not by its shape:

- A pi `select` dialog is a `List` with selectable rows, not a wall of full-width
  buttons. A stack of buttons has no selection, so there is nothing to arrow
  through and nothing for Enter to mean, and five branch names rendered as five
  buttons read as five actions rather than one question.
- The branch indicator is a `Tag`, not a disabled picker. It never opens
  anything, and rendering it as a dead dropdown trigger made it look like a
  control the user had failed to earn. `Tag` is this library's chip;
  `Badge` is a notification overlay dot and is not the same thing. Its visual
  metrics are normalized to the compact prompt selectors beside it because
  semantic difference should not produce a broken baseline.
- Sidebar rows are `ListItem`s, so hover, selected and disabled treatments come
  from the library rather than from three call sites.
- Button variants follow the guide: `primary` for the one emphasised commitment
  in a decision area, the plain default for the leave-without-committing action,
  `ghost` for quiet toolbar and strip controls, `outline` for a command in a
  dense control surface, `danger` only for an action the user cannot undo from
  the dialog. `danger` has exactly one caller — forgetting a machine — and that
  is not an accident: destructiveness cannot be inferred from a pi payload, so
  every pi confirmation is `primary`. `outline` has one caller too: the prompt
  strip's Stop button, a command sharing a row with three `ghost` pickers and
  the `primary` send. It is deliberately not `danger` — stopping a session
  ends pi, but the session file survives and the sidebar resumes it, so the
  hue that means "no way back" would be lying.
- **A control that can only act sometimes is either disabled or absent, and
  which one is a statement about the control.** Send is disabled when it
  cannot fire: it is the prompt box's permanent commitment and keeps its
  place in the row. Stop is *absent* unless the session has a turn running,
  because there is no such thing as an interrupt for work that is not
  happening, and a permanently greyed Stop would advertise an action the
  session never offers. Both live at the right end of the strip, Stop
  immediately left of send, so the turn's arrival and departure move nothing
  else in the row.

What stays in `ui/` is only what the library does not know: which icon a control
wears, what its rows say, and what a pick *means*. `ui/` is Harvester's visual
architecture, grouped by responsibility — not a palette and not a parallel
widget library.

---

## 7. Interaction and accessibility

**Every state gets a treatment.** Rest, hover, press, open, focus-visible,
selected, disabled. Press feedback lands on pointer-down, matching every other
clickable surface: a section header brightens in two steps (faint at rest, dim
on hover, full foreground on press); a tool header has both a hover fill
(`list_hover`) and a press fill (`muted`). A trigger that owns a dropdown stays
visibly selected while its menu is up, which the library does for free once the
trigger is a real `Button`.

**One focus ring, app-wide.** `KeyboardFocusRing` is a blanket trait over every
`InteractiveElement`, so any element chains `.keyboard_focus_ring(cx)`. It draws
the theme's `ring` colour as a 1.5px-spread box shadow through `focus_visible` —
*visible* focus, so a Tab press leaves a ring and a mouse click does not. It is
a shadow rather than a border because a border changes an element's layout box
and would shift a row by a pixel the moment it gained focus. Harvester's ring
differs from the library's in exactly one respect: the library draws its ring
colour at 0.2 alpha and we draw it at full strength, because a keyboard focus
ring is the one thing on screen that must not be subtle.

**Keyboard reach is a requirement, not a nicety.** Every actionable surface is a
tab stop: sidebar rows, section headers, tool-call headers, prompt controls,
dialog buttons and option lists. Almost none of it is hand-written — `tab_index`
sets both `focusable` and `tab_stop`, gpui creates the `FocusHandle` in element
state, and gpui synthesises a keyboard `ClickEvent` from Enter or Space on a
focused element that has click listeners, so an existing `on_click` *is* the
keyboard behaviour. Tab order resolves by `(path, insertion index)`, so equal
indices fall back to paint order; every tab stop in the app uses index 0 and
still walks in visual order. A row that is deliberately not clickable (a machine
row) opts out with `tab_stop(false)` rather than becoming a dead end to land on.

Commands that should work from the keyboard travel as **gpui actions**.
`PickModel`, `PickMachine` and `PickRepo` each carry an index into their
control's rows, dispatched with the prompt input's own `FocusHandle` as their
action context — so the dispatch provably lands on the chat pane that owns the
input, and focus is back in the prompt box when the menu closes. A direct
control with no index (send) stays a plain closure; routing a click through
`dispatch_action` would be indirection bought for nothing.

**Roles and accessible names.** A node reaches the accessibility tree only when
it has both an `.id()` and a `.role()` — without roles the whole gallery
projected as a single Window node. Disclosure controls carry `Role::Button`,
`aria_label` and `aria_expanded` from inside `collapsible_section`, so no caller
re-applies them. A status dot carries `Tone::label` as a tooltip and, in the
sidebar, as its lane's `aria_label`. A tool-call header folds its status into
its own accessible name instead — `"{verb} {subject}, {status}"`, e.g. "Read
main.rs, Completed" — so the dot does not announce itself twice. That string is
pinned by unit tests, because it is also the selector agents drive the gallery
through: it is an interface, not a nicety.

**Status must not be carried by colour alone.** Two non-colour channels exist:
the dot's shape (hollow = not begun, solid = something real) and the word that
every dot carries as its tooltip and accessible name. *This rule is currently
only partly satisfied* — see the gaps section.

**Pointer convention: `cursor_pointer()` is not how this app says "clickable".**
There is not one call to it in the crate. Nothing in Harvester's chrome
navigates to an external resource, so nothing takes the pointing hand; hover
fills, press fills and the focus ring carry affordance instead. The library
agrees by default — `Button` sets `cursor_default()` and only opts into a
pointer for its link and text variants.

**Motion is absent on purpose.** The app ships no transitions. In the gallery,
story switching and knob changes are the two things a user does dozens of times
a minute, so both are instant; the only motion in the binary is the streaming
transcript replay, which is the *subject* of a story rather than a transition.

---

## 8. Interface language

Words are part of the design system, and they follow the same discipline as the
tokens.

**Sentence case for everything user-visible.** "Machines", "Sessions",
"Controls", "No sessions yet", "Starting session…", "Loading models…". Section
headers used to `to_uppercase()` their title and set `SEMIBOLD`; both are gone.
The argument was mechanical rather than aesthetic: caps are only legible as a
compact shape when they come with measured tracking, and this gpui revision
exposes no letter-spacing API at all, so shipping caps would have meant shipping
half of the recipe at the smallest size the app draws. **No letter-spaced caps,
because there is no letter-spacing.** Level is carried by size and brightness
instead. `ui/shell.rs` uppercases nothing, so restoring caps later would be a
change to the callers' literals and nothing else.

**Lowercase is reserved for identifiers** — tool names, branch names, model ids
— and for the gallery's knob and preset vocabulary, which mirrors code. The
gallery's *group* names are sentence case like every other heading, and its
*story* names are deliberately Rust identifiers (`inline_note`,
`sidebar_stacked_row`): they are the exact symbol a reader is being sent to go
read, and sentence-casing them into English would break the one thing the
catalog is for.

**A button says what it is about to do.** Commit labels are per dialog kind, not
a generic OK: `Reply` for a text entry, `Confirm` for a question, `Cancel`
everywhere for the refusal. A user who reads nothing but the buttons still knows
what they are answering.

**One product term names one object.** `Reply` specifically replaced `Send`:
`Send` is already the chat composer's verb for handing a turn to the agent, and
answering a blocking question is not sending a turn. Likewise a dialog's decline
button is `Cancel` rather than `No`, because escape, the overlay click and that
button all send the same answer over the wire — labelling that path `Cancel`
makes the button say what escape does.

**Name the condition, not the category.** A titleless pi dialog does not fall
back to a generic "The agent is asking" — the user already knows the agent is
asking; what they need is what it is asking *for*. Each kind names its own
condition: "Choose an option", "Enter a value", "Write a response", "The agent
needs a yes or no".

**Let context carry context.** A subtitle names the next action or the target,
never a restatement of the title: the chat pane pairs "New session" with "Pick a
machine and a repository below". A confirmation whose message merely repeats its
title collapses to the title alone, and the empty message element is omitted
entirely rather than rendered blank. A dialog does not name the machine or
session it belongs to, because the workbench behind it already shows that. The
gallery's status band does not repeat the `group · name` that the stage header
directly above it already carries.

**Short states are labels; sentences are sentences.** "Output truncated", not
"…output truncated" and not "Output truncated." — a leading ellipsis made it
read as part of the output it describes, and a short state takes no final
period. "No output", not "(no output)": parenthesised asides are how a UI
mumbles, and a tool that genuinely printed nothing is a fact worth stating
plainly. The single ellipsis character marks a command that opens a dialog
("Add new…") or work still in progress ("Starting session…").

**A truncation budget counts its own ellipsis.** One helper cuts display text —
`ui::core::truncated_to_max_chars(text, max_chars)` — and its result is *at
most* `max_chars` characters with the ellipsis already inside that number, so a
budget means the same thing at every call site. It cuts on character
boundaries, not bytes, and trims a stranded space off the cut before appending
the mark, because "word …" reads as a missing word rather than a cut one. The
two callers used to disagree: the prompt strip's control labels landed on
exactly their budget while the transcript's tool-argument summaries landed one
character over, with a test pinning the discrepancy. Extractors that never
ellipsise — the sidebar's sid preview, the session name derived from a prompt,
client-core's byte-budget tail keeper — are a different operation and stay
separate.

**Words the user reads are named constants, not literals.** Every user-facing
string in `crates/harvester/src/` is a `const` beside the code that renders it,
so the words a surface owns can be read off the top of its file and a copy
change is a one-line diff. A string two surfaces share is declared **once**, in
whichever module decides it, and imported by the other: `CANCEL_LABEL` lives in
`ui/dialog.rs` because that is the shared modal chrome, `NEW_SESSION_LABEL` in
`chat.rs` because the sidebar's button opens exactly that pane, and
`MISSING_MACHINE_DETAIL` in `workbench/machine_presentation.rs` because that is
where the verdict is decided. Only two kinds of literal remain: a `format!`
template's interpolation (the fixed lead is still a const — see
`CONNECTING_PANE_MESSAGE_PREFIX`) and path syntax that is not prose (`"../"`,
the trailing `/` on a directory row).

**Copy that belongs to Pi is not ours.** Everything inside a pi dialog — titles,
messages, option strings — is shown verbatim. Only two transformations touch it:
whitespace trimming, and dropping a message that exactly duplicates its title.
If a pi extension writes "Are you sure you want to…", that phrasing reaches the
screen unchanged, and no amount of guide compliance on our side changes it. The
literals Harvester owns are the fallback condition titles and the button labels.

---

## 9. Empty states

An empty pane states what is missing and, where there is one, offers the way
out of it. `empty_pane_hint` is a centred column filling its parent: body-sized
text (not a tier smaller — it is the only thing on screen in that pane, so it
reads as content rather than as an annotation), centred (`text_center`, because
a wrapped hint ragged against an invisible left edge reads as a stray label),
tone-coloured, with `gap_4` and `p_8`. Both steps are relationships rather than
pixel counts: `gap_4` is the "separate groups inside one section" step, because
the sentence and the action are two things; `p_8` is the major-region step an
empty state wants, because crowding its one sentence against the edges is how an
empty state reads as broken rather than as waiting.

`empty_pane_action` is the trailing slot: one medium primary `Button`, at most
one per empty state. An empty pane that states a problem without offering the
one way out of it is a dead end — and the alternative, every empty pane
hand-rolling its own button, is how the tone rule got broken the last time.

A list with nothing in it is not the same thing. `sidebar_empty_note` ("No
sessions yet") is a plain note aligned to the sidebar's content inset, not a
row: an empty list has nothing to select, so it must not look clickable.

An empty state must never be a blank region. The transcript stories render a
fixture failure through `empty_pane_hint(Tone::Failed, …)` naming the offending
path, rather than showing an empty stage.

---

## 10. The component gallery

`cd crates/harvester-gallery && cargo run` renders Harvester's `ui` components
in isolation, Storybook-style (it is a crate of its own, outside the
workspace — see README §Build). It is the enforcement mechanism for everything
above, and it does three jobs:

1. **It makes every state renderable.** A component that can only be seen by
   driving a real session to the right state does not get reviewed. The prompt
   strip is a plain value struct precisely so the gallery can render it in every
   state — ready, loading models, models failed, no machines, starting, exited —
   which turns "what does this strip look like when models failed to load" into
   a question with one answer instead of three.
2. **It dogfoods the library it catalogs.** The gallery's own chrome is built
   from `ui::` components (`sidebar_row`, `collapsible_section`, `chat_header`,
   `statusbar_frame`, `inline_note`, `KeyboardFocusRing`), it reads
   `cx.theme().font_size` at its root rather than setting its own — a component
   catalogued at a scale no real pane gives it is being catalogued wrong — and
   every colour comes from a `Tone` or a theme slot. It has no bespoke widgets
   left.
3. **It is honest about what it shows.** A story renders the *real* component
   with the *real* constants: the dialog stories read `ui::dialog`'s own width
   constants and `options_height`, because the whole point of that story is the
   width ladder and a story that guessed the widths would be showing you the
   wrong thing. The tone story shows the one dot `Tone::is_hollow` derives rather than
   letting a knob pick a shape — a catalog whose controls can produce a
   combination the app cannot render is lying about the component. Where a story
   *cannot* be honest, it says so in a caption or in the recorded gaps.

Because the gallery's structure is projected into the accessibility tree, it is
also machine-drivable. `scripts/gallery_walk.py` is the regression check: it
walks every story over the gpui-mcp bridge, clicks each one and asserts the
selection moved, then toggles a knob and asserts the control's state flipped.
Its selectors track the accessibility tree — story rows are `ListItem`s
reporting `sel`, knob controls are `ButtonGroup` `Button`s reporting `toggled=`
— so **swapping a component for a different library widget breaks the walk.**
That is the intended behaviour: a component swap is exactly the kind of change
that should require a second look.

Adding a component to `ui/` means adding a story for it — or, when the
component is only ever drawn as part of a composite, making sure the
composite's story covers it. The prompt box's control, send button and
dropdown are catalogued through the prompt strip rather than through three
stories of their own, because no screen assembles them by hand and a catalog
of parts no screen uses is a second place for the truth to live. Anything a
story renders must be exported from `ui/mod.rs`, which makes that flat
re-export surface the catalog as well as the app's import path — and nothing
a story stops rendering should stay exported.

---

## 11. Known gaps

These are decisions to *not* fix something yet, each with the reason it was not
worth fixing blind. A contract that hides its own exceptions is worthless.

- **Status is still colour-only in two places.** The shape channel splits "not
  begun" from "something real", but it does not distinguish good from bad within
  the solid set. A tool-call block's three states (`Running`, `Ok`, `Error`) all
  render as solid dots told apart by hue, and a machine's `Live` and `Failed`
  are both solid. Both carry the state as a tooltip and an accessible name, so
  the a11y half of the rule holds; the visible non-colour channel does not.
  The workbench status band's link is likewise tinted by tone with no dot
  beside it.
- **The send button has no accessible name.** It is icon-only, and `Button`
  derives `aria_label` from its own label and from nothing else. Nothing can be
  done from this side: `aria_label` lives on `StatefulInteractiveElement`, which
  `Button` does not implement, and gpui's interactivity aria field is
  crate-private. Wrapping the button in a labelled `div` is theatre — a div with
  no role is filtered out of the tree, and a wrapper *with* a role is not the
  focusable node. The real fix is upstream.
- **`KeyboardFocusRing` has no inset variant.** A control inside a clipping
  container cannot use it, because an overflow-hidden parent masks a shadow
  drawn outside the child's box. The tool-block header therefore hand-rolls the
  equivalent: a permanently-reserved transparent 1px border recoloured on
  `focus_visible`, so gaining focus changes a colour and never a layout. One
  trait with an inset mode would let it share the app's one ring.
- **Light mode is blocked** by the memoized dark `TextViewStyle` in
  `ui/transcript.rs` (see §3). The fix and the theme toggle land together.
- **Two ellipses stand in for an indeterminate progress indicator**: the prompt
  strip's "Starting session…" and the streaming turn's bare "…". Decorative dots
  are the weaker answer, but this revision of gpui-component ships no
  indeterminate indicator and `inline_note` has no leading slot to put one in.
  Whoever adds the slot should retire both dots in the same change.
- **A long transcript is a long tab sequence.** Every tool header is a tab stop,
  which is what "every action is reachable by keyboard" demands and what chat
  UIs generally do, but the better end state is the transcript as a list with
  arrow-key traversal behind a single tab stop. That needs an owner for keyboard
  navigation across the whole shell.
- **The dialog stories are chrome only.** They render the confirmation body, the
  button row, the option list and both inputs inline on the canvas; they never
  call `window.open_dialog`. Modal behaviour — focus, escape, the Root dialog
  stack — belongs to `workbench/modal.rs` and is not what a component catalog
  is for.
- **The dialog geometry has never been seen driven by a real Pi.** The option
  row height, the three width rungs, and a virtual list inside an auto-height
  dialog body were all chosen by construction. They want one look at a genuine
  `select` from a real extension before they are treated as settled. The select
  dialog is also not searchable, which would help at a hundred options — but a
  search input takes focus and changes what Enter means inside a dialog whose
  whole contract is that Enter answers Pi.
- **`empty_pane_action` has no app caller yet.** It is catalogued in the gallery
  and exported, waiting for the first empty state that has a real next action to
  offer.
