# harvester-cli — implementation notes

This codebase carries no code comments (see AGENTS.md). Everything a comment
would have said about `harvester-cli` lives here instead, one section per module.
Update the relevant section in the same change that touches the code.

## crates/harvester-cli/src/main.rs

`harvester-cli` is a dev driver for `harvester-client-core`: it bootstraps
harvesterd onto a machine, connects, and runs one command. It also doubles as
the protocol smoke-test tool — the `raw` subcommand replaces the throwaway
python drivers that were used during the e2e rounds referenced in
CLAUDE.md's Build & test section.

### `raw` subcommand: protocol REPL/pipe semantics

`raw` is a byte pump in both directions: stdin lines go to the daemon as-is,
daemon lines come back to stdout as-is. This is deliberate — decoding either
side would hide exactly what the tool exists to inspect. A malformed frame
must be allowed to reach the daemon (its parse-error response is a documented
path), and a line this tool cannot parse must still be printed rather than
silently ending the stream.

Shutdown sequencing: on stdin EOF, the write half is dropped, which
half-closes the socket; the daemon then finishes any queued responses and
closes its side, and the printer task stops on that EOF. This means scripted
use (`echo ... | harvester-cli <dest> raw`) reliably sees every answer instead
of racing the round trip.
