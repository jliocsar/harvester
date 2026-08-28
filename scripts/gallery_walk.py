#!/usr/bin/env python3
"""Drive the gallery like an agent would, as the gallery's regression check.

Walks every story in the sidebar: click it, and assert from the snapshot the
click itself returned that selection moved. Then finds a story with a
toggleable knob and toggles it, asserting the control's state flipped.

Selectors track the accessibility tree, so they break when a component is
swapped for a different library widget. Story rows are ListItems reporting
`sel`; knob controls are ButtonGroup buttons reporting `toggled=`.
"""

import os
import subprocess
import sys
import time

GPUI_MCP_ROOT = os.environ.get(
    "GPUI_MCP_ROOT", os.path.expanduser("~/Projects/gpui-mcp")
)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

sys.path.insert(0, os.path.join(GPUI_MCP_ROOT, "scripts"))
from e2e_mcp import McpClient, PROTOCOL_VERSION, header_frame

# The gallery is a crate of its own, with its own workspace and target dir, so
# its binary does not land in the repo-root target dir.
GALLERY_CRATE = os.path.join(REPO_ROOT, "crates/harvester-gallery")
GALLERY_BINARY = os.path.join(GALLERY_CRATE, "target/debug/gallery")
SERVER_BINARY = os.path.join(GPUI_MCP_ROOT, "target/debug/gpui-mcp")
ATTACH_ATTEMPTS = 60
ATTACH_DELAY_SECONDS = 1.0
DOCK_TITLE = "Controls"
CLICK_SETTLE_SECONDS = 0.6
# The catalog as registered in crates/harvester-gallery/src/registry.rs. Floors
# rather than equalities, so adding a story does not fail the walk while a
# story (or a whole group) silently disappearing does.
EXPECTED_GROUP_COUNT = 6
EXPECTED_STORY_COUNT = 14


def parse_lines(snapshot_text):
    """(key, role, label, states) per node line."""
    entries = []
    for line in snapshot_text.splitlines()[1:]:
        parts = line.split()
        if len(parts) < 2:
            continue
        key, role = parts[0], parts[1]
        label = ""
        if '"' in line:
            label = line.split('"')[1]
        states = line.rsplit('"', 1)[-1] if '"' in line else line
        entries.append((key, role, label, states))
    return entries


def find_story_rows(snapshot_text):
    return [
        (key, label, "sel" in states.split())
        for key, role, label, states in parse_lines(snapshot_text)
        if role == "ListItem"
    ]


def main():
    gallery = subprocess.Popen(
        [GALLERY_BINARY],
        env={**subprocess.os.environ, "GPUI_MCP": "1"},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    client = McpClient(SERVER_BINARY)
    failures = []

    def check(label, condition, detail=""):
        status = "PASS" if condition else "FAIL"
        if not condition:
            failures.append(label)
        print(f"[{status}] {label}{(' — ' + detail) if detail else ''}")

    try:
        client.request(
            "initialize",
            {"protocolVersion": PROTOCOL_VERSION, "capabilities": {},
             "clientInfo": {"name": "gallery-walk", "version": "0"}},
        )
        client.notify("notifications/initialized")

        attached = False
        for _ in range(ATTACH_ATTEMPTS):
            time.sleep(ATTACH_DELAY_SECONDS)
            text, is_error = client.call_tool("attach", {"pid": gallery.pid})
            if not is_error:
                attached = True
                break
        if not attached:
            print("could not attach:", text)
            return 1

        snapshot, is_error = client.call_tool("snapshot")
        for _ in range(ATTACH_ATTEMPTS):
            if not is_error and "ListItem" in snapshot:
                break
            time.sleep(ATTACH_DELAY_SECONDS)
            snapshot, is_error = client.call_tool("snapshot")

        def click_by_label(role_wanted, label_wanted, attempts=4):
            """Re-snapshot, find the node by role+label, click. Keys and frame
            stamps are per-dump, so on an animating story a stamp can go stale
            between snapshot and click; each retry re-resolves both."""
            last = ""
            for _ in range(attempts):
                time.sleep(CLICK_SETTLE_SECONDS)
                fresh, is_error = client.call_tool("snapshot")
                if is_error:
                    last = fresh
                    continue
                target = next(
                    (key for key, role, label, _ in parse_lines(fresh)
                     if role == role_wanted and label == label_wanted),
                    None,
                )
                if target is None:
                    last = f"{role_wanted} {label_wanted!r} not in snapshot"
                    time.sleep(0.3)
                    continue
                clicked, is_error = client.call_tool(
                    "click",
                    {"key": target, "frame": header_frame(fresh), "snapshot": True},
                )
                if not is_error:
                    return clicked, ""
                last = clicked
                if "stale_handle" not in clicked:
                    break
            return None, last

        def toggle_state(states):
            """ButtonGroup segments and bool knobs report aria_toggled, which
            the snapshot renders as `toggled=true` / `toggled=false`. They
            never report aria_selected, which is what story rows use."""
            for token in states.split():
                if token.startswith("toggled="):
                    return token.split("=", 1)[1] == "true"
            return None

        def group_headers(snapshot_text):
            """(key, label, expanded) per story group header, never the dock."""
            headers = []
            for key, role, label, states in parse_lines(snapshot_text):
                if role != "Button" or label.casefold() == DOCK_TITLE.casefold():
                    continue
                tokens = states.split()
                if "exp" in tokens:
                    headers.append((key, label, True))
                elif "col" in tokens:
                    headers.append((key, label, False))
            return headers

        def set_group(label, expanded):
            """Drive one group to a known state. Keys are per-dump, so this
            re-resolves on every attempt rather than caching one."""
            for _ in range(4):
                time.sleep(CLICK_SETTLE_SECONDS)
                fresh, is_error = client.call_tool("snapshot")
                if is_error:
                    return False
                match = next(
                    ((key, is_open) for key, name, is_open in group_headers(fresh)
                     if name == label),
                    None,
                )
                if match is None:
                    return False
                key, is_open = match
                if is_open == expanded:
                    return True
                _, is_error = client.call_tool(
                    "click", {"key": key, "frame": header_frame(fresh)}
                )
                if is_error:
                    return False
            return False

        def knob_chips(snapshot_text):
            """Toggleable controls in the control dock only. The stage renders
            buttons of its own, and a story that happens to draw a toggled
            control is not a knob."""
            lines = parse_lines(snapshot_text)
            dock_at = next(
                (index for index, (_, role, label, _) in enumerate(lines)
                 if role == "Button" and label.casefold() == DOCK_TITLE.casefold()),
                None,
            )
            if dock_at is None:
                return []
            return [
                (label, toggle_state(states))
                for _, role, label, states in lines[dock_at + 1:]
                if role == "Button" and toggle_state(states) is not None
            ]

        groups = [label for _, label, _ in group_headers(snapshot)]
        check(
            "sidebar lists story groups",
            len(groups) >= EXPECTED_GROUP_COUNT,
            f"{len(groups)} groups",
        )

        for label in groups:
            set_group(label, False)

        story_count = 0
        knob_story = None

        for group in groups:
            if not set_group(group, True):
                check(f"expand group {group!r}", False, "the header did not respond")
                continue

            fresh, is_error = client.call_tool("snapshot")
            if is_error:
                check(f"list stories in {group!r}", False, fresh[:80])
                continue

            for name in [label for _, label, _ in find_story_rows(fresh)]:
                story_count += 1
                clicked, failure = click_by_label("ListItem", name)
                if clicked is None:
                    check(f"select {name!r}", False, failure[:80])
                    continue
                now_selected = [
                    label for _, label, sel in find_story_rows(clicked) if sel
                ]
                check(
                    f"select {name!r}",
                    now_selected == [name],
                    f"selected={now_selected}",
                )
                if knob_story is None and knob_chips(clicked):
                    knob_story = (group, name)

            set_group(group, False)

        check(
            "walk reached every story",
            story_count >= EXPECTED_STORY_COUNT,
            f"{story_count} stories",
        )

        if knob_story is None:
            check(
                "a story exposes a toggleable knob",
                False,
                "no dock control reported a toggled= state",
            )
        else:
            group, name = knob_story
            set_group(group, True)
            reselected, failure = click_by_label("ListItem", name)
            chips = [] if reselected is None else knob_chips(reselected)
            target_label = next(
                (label for label, on in chips if not on),
                chips[0][0] if chips else None,
            )

            if target_label is None:
                check(
                    f"toggle a knob on story {name!r}",
                    False,
                    "the dock exposed no untoggled chip to click",
                )
            else:
                clicked, failure = click_by_label("Button", target_label)
                flipped = [] if clicked is None else [
                    on for label, on in knob_chips(clicked) if label == target_label
                ]
                check(
                    f"toggle knob {target_label!r} on story {name!r}",
                    True in flipped,
                    failure[:80] if clicked is None else f"chip states now {flipped}",
                )

        print()
    finally:
        client.close()
        gallery.terminate()
        gallery.wait(timeout=10)

    if failures:
        print(f"FAILURES ({len(failures)}): " + ", ".join(failures))
        return 1
    print("all gallery walk checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
