# Harvester

A Rust + [gpui](https://gpui.rs) desktop orchestrator for [Pi](https://pi.dev)
coding-agent sessions running on remote machines.

Sessions run **detached** on your VMs — the client is a window onto them, not
their owner. Close the laptop; the agents keep working. Reopen it; everything
is still there.

```
┌──────────────┐   SSH (socket forward)   ┌────────────────┐   stdin/stdout    ┌───────────────┐
│ gpui client  │ ───────────────────────► │  harvesterd    │ ────────────────► │ pi --mode rpc │ × N
└──────────────┘                          │  (per machine) │                   └───────────────┘
                                          └────────────────┘                     sessions live in
                                            stateless relay                      pi's own session
                                                                                 files on the VM
```

- **Machines** come from user-pluggable providers — any executable speaking a
  one-shot JSON-stdio contract (`docs/PROVIDERS.md`). [exe.dev](https://exe.dev)
  ships as the built-in one.
- **harvesterd** is a tiny static musl daemon bootstrapped over SSH. It exists
  because pi's RPC mode lives and dies with its stdin; it owns that stdin and
  relays pi's JSONL protocol verbatim to any number of clients
  (`docs/DAEMON.md`).
- **No database, no journal, no sync layer.** Pi's documented session files are
  the only state; reconnecting clients replay from pi's durable entry cursor.

## Status

Early development. Contracts are designed and hands-on verified against pi
0.83.0 and real exe.dev VMs; `harvesterd`, the wire protocol crate, the
`exe-dev` provider, and the SSH bootstrap/connect library are implemented and
live-tested.

The gpui client has grown past its first vertical slice. Built: a fleet of
machines behind one state per machine (probes, connect policy, status dots), a
sessions sidebar, chat from before a session exists until after its pi exits,
prompt-strip machine/repository/model pickers, tool-call blocks, a repairable
transcript replica, workspaces in `~/.harvester/workspaces.json`, pi's blocking
dialogs as modals, and the whole widget layer on `gpui-component` under
Harvester's own dark theme (`DESIGN.md`). Several of those are built but
unexercised — the dialogs have never been seen on screen, the `local`
machine has never been click-tested, and the prompt strip's Stop button — the
client half of stopping a running session, shown while a turn is in flight —
has never been clicked against a live pi. Session rename, the cost/token
statusbar and machine create/delete are deferred.

`docs/SPEC.md`'s build-state table is the authoritative, slice-by-slice version
of this paragraph.

## Layout

```
docs/            specs & contracts (start at docs/SPEC.md)
providers/
  exe-dev            the built-in machine provider (sh + jq, the contract's reference)
scripts/
  gallery_walk.py    drives the component gallery over MCP as its regression check
crates/
  harvester-proto/       typed envelope for the client ↔ daemon wire (JSONL)
  harvesterd/            the per-machine daemon
  harvester-client-core/ bootstrap-over-SSH + socket forward + envelope client
  harvester-cli/         dev driver for the client library (ping / list-sessions / raw)
  harvester/             the gpui desktop client
  harvester-gallery/     dev-only component gallery — its own workspace, not a member
```

## Build

```sh
cargo build --workspace
cargo build --profile daemon-release --target x86_64-unknown-linux-musl -p harvesterd   # deployable daemon
```

### The component gallery

`crates/harvester-gallery` renders every `harvester::ui` component in
isolation, Storybook-style. It is **excluded from the workspace on purpose**:
it path-depends on a sibling `gpui-mcp` checkout that a clean clone does not
have, and cargo resolves a workspace member's path dependencies whether or not
anything enables them — so as a member it broke `cargo build --workspace` for
everyone else. Excluded, it
keeps its own `Cargo.lock` and `target/`, and the repo's own gate passes
without gpui-mcp present.

```sh
cd crates/harvester-gallery && cargo run    # the catalog
python3 scripts/gallery_walk.py             # the walk (needs gpui-mcp built)
```

The walk is the gallery's regression check: it attaches over MCP, clicks every
story and toggles a knob, asserting from the accessibility tree that the UI
moved. It needs a display and a built `gpui-mcp` server; `GPUI_MCP_ROOT`
overrides where that checkout is looked for. `DESIGN.md` §10 is the gallery's
design rationale, `docs/internals/harvester-gallery.md` its implementation
notes.
