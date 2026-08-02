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
live-tested. The gpui client exists as a first vertical slice: connect to one
machine, list sessions, attach (or resume), send a prompt, watch it stream.

## Layout

```
docs/            specs & contracts (start at docs/SPEC.md)
providers/
  exe-dev            the built-in machine provider (sh + jq, the contract's reference)
crates/
  harvester-proto/       typed envelope for the client ↔ daemon wire (JSONL)
  harvesterd/            the per-machine daemon
  harvester-client-core/ bootstrap-over-SSH + socket forward + envelope client
  harvester-cli/         dev driver for the client library (ping / list-sessions / raw)
  harvester/             the gpui desktop client
```

## Build

```sh
cargo build --workspace
cargo build --release --target x86_64-unknown-linux-musl -p harvesterd   # deployable daemon
```
