# Lint policy — implementation notes

This codebase carries no code comments (see AGENTS.md). The lint configuration
is spread across three files with no room for rationale in any of them;
everything a comment would have said lives here.

- `Cargo.toml` — `[workspace.lints.rust]` and `[workspace.lints.clippy]`. Every
  member crate opts in with `lints.workspace = true`.
- `clippy.toml` — thresholds and bans for the lints enabled above.
- `sgconfig.yml` + `.ast-grep/rules/` — the house rules clippy cannot express.

`crates/harvester-gallery` is its own workspace and does not inherit
`[workspace.lints]`; it is covered by the ast-grep rules, which walk the
filesystem rather than the member list, the same way `no_comments.rs` does.

## Why this exists

The one-way rules in AGENTS.md were written to be checkable with a grep, and
several of them turned out never to have been checked. `unreachable_pub` is
exactly the rule "`pub` means re-exported from `lib.rs`; everything else is
`pub(crate)`", and it found 163 violations. `use_self` is exactly "every struct
literal and every constructor return type is written `Self`", and it found 63.
A rule a reviewer *could* check is not a rule anyone *does* check.

## What the complexity lints found

Nothing. `cognitive_complexity`, `too_many_lines` and `excessive_nesting` are
all enabled with thresholds set in `clippy.toml` and all report zero across the
workspace. The "match arms delegate" and "factor out named handlers" rules had
already done that job; the thresholds stay configured so the property is held
rather than merely observed.

## Verdicts

Kept, and burned down to zero in the 2026-08-30 round:

- `unreachable_pub` (163) — the `pub`/`pub(crate)` rule, now enforced.
- `clippy::use_self` (63) — the `Self` rule, now enforced.
- `elided_lifetimes_in_paths` (141) — nearly all `Context<T>` → `Context<'_, T>`.
- `let_underscore_drop` (29) — all were `let _ = tx.send(..)`, rewritten to
  `drop(tx.send(..))`. `.ok()` is not an alternative: `Option` is `#[must_use]`,
  so it trips `unused_must_use` instead.
- `clippy::indexing_slicing` (79) — 76 were assertions in `#[cfg(test)]` blocks
  and are permitted by `allow-indexing-slicing-in-tests`; indexing a
  known-shape fixture is clearer there than `.get(..).unwrap()`. The 3 real
  ones were all in `read_frame`, and became `strip_suffix`.

Rejected, with the reason recorded next to the `allow` in `Cargo.toml`:

- `clippy::redundant_pub_crate` — directly contradicts `unreachable_pub`. No
  code satisfies both; `unreachable_pub` is the rule this codebase wants.
- `clippy::must_use_candidate` — written for library authors publishing an API.
  Harvester ships binaries.
- `clippy::map_unwrap_or` and `clippy::unnecessary_wraps` — the two pedantic
  opinions AGENTS.md already rejected, restated here so the config is
  self-explaining.

## Restriction lints are a menu

The `restriction` group is never enabled wholesale; it contains lints that
contradict each other by design. Enabled individually: `dbg_macro`, `todo`,
`unimplemented`, `indexing_slicing`, `mod_module_files`, `allow_attributes`,
`allow_attributes_without_reason`.

`allow_attributes_without_reason` forces `#[expect(lint, reason = "...")]` over
a bare `#[allow]`. `#[expect]` warns once the suppression is no longer needed,
so lint debt cleans itself up instead of accumulating. The `reason` string is an
attribute argument, not a comment, so this does not breach the zero-comment
rule.

## disallowed-methods

`clippy.toml` bans the primitives that a duplicate implementation would have to
start from, so the second implementation cannot be written by accident. This is
the enforcement arm of "one implementation per computation, in the lowest crate
both callers already depend on" — `unix_millis_now` had been written twice and
had silently diverged before it was converged.

The canonical helper trips its own ban and carries an `#[expect]` naming itself
as the exception, which is the intended shape: the exception is visible at the
one place it is legal.

## ast-grep rules

`.ast-grep/rules/theme-outside-ui-is-structural-only.yml` enforces "outside
`ui/`, `cx.theme()` is for structural slots only; every text colour comes from a
`Tone`". The allowlist is `sidebar`, `sidebar_border`, `background`,
`foreground`, `popover`, `border`, `radius`, `font_size`. `popover` is on the
list because its three uses are all `.bg(..)` — a surface colour, not a text
colour, which is what the rule is actually about.

ast-grep is preferred over dylint for anything purely syntactic: a rule is a
dozen lines of YAML rather than a lint crate. Rules needing type information
would still have to be dylint.

## Rules deliberately not automated

- **Blank-line padding around blocks.** rustfmt's `blank_lines_lower_bound` is
  nightly-only and operates between items, not inside function bodies. Still
  the recorded dylint candidate.
- **"No magic values."** No lint distinguishes a tunable from an ordinary
  integer literal. Review-only.
- **"Never a generic Unicode line reader."** The invariant is about protocol
  framing, but `str::lines()` has twelve legitimate uses in display and
  `ls`-output parsing. A blanket ban would be wrong and a scoped one cannot be
  expressed. `read_frame` being the only correct reader is documented in
  `harvester-proto.md` and left to review.

## Re-measuring

`--all-targets` compiles most code twice (lib and lib-test), so the raw warning
count roughly doubles. Deduplicate by `(file_name, line_start)` over the
`--message-format=json` stream before quoting a number.

More importantly: **a lint count taken from a partially-cached build is wrong.**
Cargo does not re-emit diagnostics for a target it did not rebuild, so a lint
can appear to have zero hits in a crate simply because that crate was fresh.
This produced a false "zero production `indexing_slicing` sites" reading during
the 2026-08-30 round when there were three. `touch` the crate roots, or
`cargo clean -p <crate>`, before trusting a tally.

## Open

The tail below is still unresolved and is why CI does not yet gate on a clean
clippy run:

- `missing_errors_doc` (15) and `missing_panics_doc` (2) demand `///` sections
  and are therefore incompatible with the zero-comment rule as written. Either
  both are allowed, or `no_comments.rs` grows a carve-out for `# Errors` and
  `# Panics` blocks specifically.
- `needless_pass_by_value` (8), `significant_drop_tightening` (4),
  `option_if_let_else` (4) and `future_not_send` (1) are nursery lints with
  known false-positive rates and no verdict yet.
- Roughly twenty single-site findings (`unreadable_literal`,
  `cast_possible_truncation`, `needless_continue`, `or_fun_call`,
  `allow_attributes_without_reason`) are small and unfixed.
