# Harvester Provider Contract (protocol v1)

> Status: agreed 2026-08-01. Open decisions listed at the bottom; everything else is normative.
>
> **Implementation status (2026-08-04).** Normative ≠ built. Harvester implements the mandatory core — discovery, `info` (with the protocol gate), `list`, the 30s timeout — and nothing else. Sections marked **[NOT IMPLEMENTED]** are contract a provider may satisfy today but Harvester will not exercise: `create`, `delete`, and the `{argv}` form of `ssh`. A provider that only implements `info` + `list` returning `{host, user, port}` is fully usable right now; one that relies on a deferred section is not, and the marker is there so that is knowable without reading the client.

A **provider** is a user-supplied executable that answers one question for Harvester: *what machines exist and how do I SSH into them?* — plus machine CRUD.

Providers are **control plane only**. Session traffic (harvesterd, pi, file pickers, git checks) always flows over plain SSH using the connection info the provider returns. A provider never proxies session data.

A provider also *makes its own connections*: it is a separate program, so Harvester's client-side SSH multiplexing and keepalive policy do not extend to it, and none of it is contract. A provider that talks to a control plane on every `list` pays its own handshake each time (the bundled `exe-dev` does) — acceptable at `list` cadence, and the provider's own call if it wants otherwise.

## Discovery

```
~/.harvester/providers/<name>     # any executable, any language
```

- Executable bit set → registered, `<name>` is the provider id shown in the UI.
- Non-executable files and dotfiles are ignored.
- Harvester ships one default provider, `exe-dev`, written into this directory whenever it is **absent** — on first run, and on any later run where it is missing. Implemented in the same format as user providers; there are no privileged code paths.
  - The consequence, since absence is the only trigger: **deleting the bundled provider does not disable it** — the next launch writes it back. **Clear the executable bit to disable any provider** (non-executable files are ignored by discovery). This applies to the bundled one only; a user provider is never written or modified by Harvester.

## Transport: one-shot invocations

No resident process, no framing, no handshake. Each operation is one exec:

```
<provider> <operation>          # args (if any) as a single JSON object on stdin
                                # result as a single JSON object on stdout
                                # diagnostics free-form on stderr (logged, never parsed)
```

- **Exit 0** = success, stdout must parse as the operation's result object.
- **Exit non-zero** = failure; stdout *may* contain `{"error": "human-readable message"}` which is surfaced verbatim in the UI. If stdout is unparseable, Harvester shows a generic failure + captured stderr.
- **Timeout**: Harvester kills any invocation after **30s** (`create`: **120s**, contract only — see `create`). A timeout is a failure. Implementers must not depend on longer runtimes.
- stdin is closed after the args object is written. Providers must not prompt or wait for further input.
- Unknown operations must exit non-zero (Harvester uses this for forward compat).

Debugging a provider is just running it in a terminal:

```sh
echo '{"name":"vps-work"}' | ~/.harvester/providers/exe-dev create
```

## Operations

### `info` — identity & capabilities

No stdin args.

```json
{
  "protocol": 1,
  "name": "exe.dev",
  "capabilities": ["create", "delete"],
  "create_fields": [
    {"key": "name",   "label": "Machine name", "type": "string", "required": true},
    {"key": "cpu",    "label": "CPUs",         "type": "number", "required": false},
    {"key": "memory", "label": "Memory",       "type": "string", "required": false}
  ]
}
```

- `protocol` (required): contract version this binary speaks. **Harvester requires exact equality** with the one version it speaks (`1` today) and refuses everything else, older included — there is no compatibility window, and none is implied by the number being an integer. Forward compatibility is the job of `capabilities`, which is why additive commands never move this field.
- `capabilities` (required): **the list of optional operations this executable implements, by command name.** This is the composition mechanism of the protocol: every part of the API beyond the mandatory core (`info`, `list`) is an independent capability that a provider opts into, and future protocol additions (e.g. a hypothetical `resize`, `logs`, `stop`) arrive as new capability strings — no version bump needed for additive commands. v1 defines `create` and `delete`. Rules:
  - Mandatory operations (`info`, `list`) are never declared.
  - Declaring a capability promises the command exists and speaks its documented schema.
  - A capability absent → the corresponding UI affordance (e.g. "+" create button) is hidden for this provider's machines; Harvester never invokes an undeclared command.
  - Harvester ignores capability strings it doesn't recognize (older Harvester + newer provider composes safely).
- `create_fields` (optional, only meaningful with `create`): drives the generic "add machine" modal. `type` ∈ `string | number | boolean | secret`. Harvester renders these blindly and passes the values back to `create` untouched.

### `list` — enumerate machines (mandatory)

No stdin args.

```json
{
  "machines": [
    {
      "id": "bloggy",
      "label": "bloggy",
      "status": "running",
      "ssh": {"host": "bloggy.exe.xyz", "user": "exedev", "port": 22}
    }
  ]
}
```

- `id` (required): stable identifier, unique within the provider. Harvester keys machines as `<provider>/<id>`.
- `label` (required): display name for the sidebar.
- `status` (required): `running | stopped | unknown`. Advisory only — the green/red dot in the UI is driven by Harvester's own direct probe (`ssh <machine> true`), not by this field. Providers that can't tell should say `unknown`.
- `ssh` (required): **one of two forms** —

  ```json
  {"host": "bloggy.exe.xyz", "user": "exedev", "port": 22}
  ```

  `user`/`port` optional (defaults: current user, 22). Or the escape hatch:

  ```json
  {"argv": ["fly", "ssh", "console", "-a", "myapp", "-C"]}
  ```

  `argv` is a command producing a bidirectional byte stream to the machine's shell (ProxyCommand-style). This is what makes arbitrary platforms wireable without Harvester knowing about them.

  **[NOT IMPLEMENTED]** — the client refuses to derive a target from `{argv}` today. Such machines still *list* (they appear in the sidebar, sourced and labelled) but carry no connection target: they never probe, never bootstrap a daemon, and their dot stays gray. Nothing warns the machine's author, so **a provider returning only `{argv}` machines ships a fleet that cannot connect**; return `{host, user, port}` for anything that must work now. Tracked as deferred in `SPEC.md` §Deferred.

### `create` — provision a machine

**[NOT IMPLEMENTED]** — deferred with the machine-CRUD slice (the `create_fields` modal and the "+" affordance are the same slice). Harvester never invokes it today, and the 120s timeout below is contract, not code: the only timeout that exists is the 30s general one. The bundled `exe-dev` implements `create` regardless, so the contract below is live-tested by running it in a terminal — it just has no caller inside Harvester yet.

stdin: the values collected from `create_fields`, e.g. `{"name": "vps-work", "cpu": 4}`.

```json
{"machine": {"id": "vps-work", "label": "vps-work", "status": "running", "ssh": {"host": "vps-work.exe.xyz"}}}
```

Same machine object shape as `list`. Timeout 120s.

### `delete` — destroy a machine

**[NOT IMPLEMENTED]** — same slice, same status as `create`.

stdin: `{"id": "vps-work"}` → `{}` on success.

Harvester always confirms with the user before invoking; providers should not add their own interactive confirmation (they can't — stdin is closed).

## What is deliberately NOT in the contract

- **Health**: no `health` op. `list.status` is advisory; liveness truth is Harvester's own SSH probe.
- **Events/push**: none. Harvester polls `list` (low frequency) and probes SSH (dot frequency). If a resident-process protocol is ever needed, that's protocol v2.
- **Session anything**: workspaces, sessions, harvesterd bootstrap — all plain SSH, provider not involved.
- **Machine mutation** (resize/rename/tags): platform CLIs do this fine; not Harvester's job in v1.

## Reference implementation: `exe-dev`

The default provider is approximately a field-renaming wrapper over exe.dev's SSH control plane:

- `list` → `ssh exe.dev ls --json` → map `{vm_name → id/label, status, ssh_dest → ssh.host}`
- `create` → `ssh exe.dev new --json --name <name> [--cpu … --memory …]`
- `delete` → `ssh exe.dev rm <id>`
- Auth rides the user's existing exe.dev SSH key; the provider stores nothing.

It doubles as the contract's living documentation: small enough to read in one sitting, and proof that a shell script is a valid provider. Implemented at `providers/exe-dev` (POSIX sh + `jq`); verified live against exe.dev — full create/list/delete cycle, error paths, and unsafe-value rejection.

That last one is the rule worth copying into any provider that shells out: **every value that reaches the remote command line is gated on `[A-Za-z0-9-]` and refused rather than quoted** — machine ids, but equally `--cpu` and `--memory`, which travel exactly as far. Refusing is the point: a quoting scheme is a thing to get subtly wrong once, whereas a rejected value is a visible error with an exit code. The gate is `require_safe_value` in the script.

## Local machine

The built-in read-only "local" machine (per INITIAL_SPEC) is **not** a provider — it's hardcoded in Harvester (spawn harvesterd as a child over a Unix socket). Providers only model remote machines.

## Future: TypeScript SDK (post-v1)

Ship `harvester` on npm so users can build a spec-compliant provider without hand-rolling the protocol:

```ts
import { provider } from "harvester";

provider({
  name: "my-cloud",
  create_fields: [{ key: "name", label: "Name", type: "string", required: true }],
  list:   async () => [...],
  create: async (args) => ({...}),
  delete: async ({ id }) => {},
});
```

The SDK owns arg parsing, stdin/stdout JSON, exit codes, and protocol versioning; the user writes three async functions and compiles to a single binary (`bun build --compile` / `deno compile`). The SDK is a *convenience*, not a requirement — the stdio contract remains the spec.

## Open decisions

1. Windows story (no executable bit; `.exe`/`.cmd` suffix resolution) — deferred, target is Linux/macOS first.
2. ~~Should `list` results be cached between polls, and for how long?~~ — resolved in the client: `list` polls every 300s, the SSH probe every 30s, and the last listing is displayed between polls. Still not contract.
3. `secret` field type: where do secret values live between modal and `create` call (memory only, never persisted by Harvester)? Leaning: pass-through only, providers own their own credential storage.
