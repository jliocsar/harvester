# harvester

Harvester is a Rust + gpui agents orchestrator tool.

## Citizens

**Machine**: Where the agent runs. The user can configure N Machines with a label to them, so the user can have a "vps-work", "vps-personal" and so on. The default machine (read-only, can never be deleted / edited) is "local" -- i.e. the machine in which the user opened Harvester.

**Workspace**: A workspace (git repo) from where a session will start from.

**Sessions**: Session = a running agent in a machine. Agents must run in a "headless" mode (i.e. without launching a TUI / interactive flow) so that we can integrate them with our UI (so we'll most likely either use their SDK or use something like `claude -p --output-format json`).

## UI Sketch

**App layout**:

```
{sidebar} {sessions_tabs}
{sidebar} {chat}
{sidebar} {statusbar}
```

**Sidebar layout**:

```
{machines_header}
{machines_list}
{workspaces_header}
{workspaces_list}
{sessions_header}
{sessions_list}
```

## Flows

### Adding a new machine

1. User clicks on a "+" button in the `machines_header`.
2. We open a modal where the user picks:
   - Machine name / label.
3. We assert the machine has conectivity before storing it.

From there one, we always healthcheck the machine to show the accurate status in its sidebar `machines_list` item (green pulsing dot vs. red dot).

### Adding a new workspace

1. User clicks on a "+" button in the `workspaces_header`.
2. We open a modal with a dropdown of the Machines for the user to select from.
3. Once a machine is selected, we open a folder picker (from that machine) so that the user can configure their workspace path.
4. We validate that the directory is a git repo, if not -> clear error message, let them pick again.
5. If it is a git repo, we store that as a `machine->folder_name`, meaning the Workspace is now `folder_name` in `machine_label` (this is how we'll present in the `workspaces_list`.

### Starting a new session

1. User clicks on a "+" button in the `workspaces_list` item.
2. We open a "ghost" chat session: we display the empty chat, with the "harness" + "model" picker and so on.
3. Once the user sends the first prompt, we add that session to the `sessions_list` (with a "temporary" / randomized session title).

### Renaming a session

1. User right clicks the `sessions_list` item.
2. They select the "Rename" option in the dropdown menu.
3. (the session title in the list item must be an input for this): We "enable" the session title ghost input so that the user can type on / edit it.
4. The user then presses Enter to save the new session title.

## References

- https://gpui.rs/
- https://code.claude.com/llms.txt
- https://pi.dev/docs/latest
- https://learn.chatgpt.com/docs/developer-commands?surface=cli
- https://learn.chatgpt.com/docs/codex-sdk
