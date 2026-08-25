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

GALLERY_BINARY = os.path.join(REPO_ROOT, "target/debug/gallery")
SERVER_BINARY = os.path.join(GPUI_MCP_ROOT, "target/debug/gpui-mcp")
ATTACH_ATTEMPTS = 60
ATTACH_DELAY_SECONDS = 1.0
DOCK_TITLE = "Controls"


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
                if "scrolled out of view" in clicked:
                    collapse_first_expanded_group(fresh)
                    continue
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

        def collapse_first_expanded_group(snapshot_text):
            """What an agent does after the out-of-view refusal: make room by
            collapsing an expanded story group above the target."""
            for key, role, label, states in parse_lines(snapshot_text):
                if role == "Button" and "exp" in states.split() and label.casefold() != DOCK_TITLE.casefold():
                    client.call_tool(
                        "click",
                        {"key": key, "frame": header_frame(snapshot_text)},
                    )
                    return

        story_names = [label for _, label, _ in find_story_rows(snapshot)]
        check("sidebar lists stories", len(story_names) >= 10, f"{len(story_names)} stories")

        knob_story = None
        for name in story_names:
            clicked, failure = click_by_label("ListItem", name)
            if clicked is None:
                check(f"select {name!r}", False, failure[:80])
                continue
            now_selected = [label for _, label, sel in find_story_rows(clicked) if sel]
            check(f"select {name!r}", now_selected == [name], f"selected={now_selected}")
            has_chips = any(
                role == "Button" and toggle_state(states) is not None
                for _, role, _, states in parse_lines(clicked)
            )
            if knob_story is None and has_chips:
                knob_story = name

        def expand_collapsed_groups():
            """Undo any collapses the out-of-view recovery made, so every
            story row is findable again. One group per pass because every
            click reassigns keys."""
            for _ in range(10):
                fresh, is_error = client.call_tool("snapshot")
                if is_error:
                    return
                collapsed = next(
                    (key for key, role, label, states in parse_lines(fresh)
                     if role == "Button" and "col" in states.split()
                     and label.casefold() != DOCK_TITLE.casefold()),
                    None,
                )
                if collapsed is None:
                    return
                client.call_tool(
                    "click", {"key": collapsed, "frame": header_frame(fresh)}
                )

        check(
            "a story exposes a toggleable knob",
            knob_story is not None,
            "no story reported a control with a toggled= state",
        )

        if knob_story is not None:
            expand_collapsed_groups()
            reselected, failure = click_by_label("ListItem", knob_story)
            chips = [] if reselected is None else [
                (label, toggle_state(states))
                for _, role, label, states in parse_lines(reselected)
                if role == "Button" and toggle_state(states) is not None
            ]
            target_label = next(
                (label for label, on in chips if not on),
                chips[0][0] if chips else None,
            )

            if target_label is None:
                check(
                    f"toggle a knob on story {knob_story!r}",
                    False,
                    "the story exposed no untoggled chip to click",
                )
            else:
                clicked, failure = click_by_label("Button", target_label)
                flipped = [] if clicked is None else [
                    toggle_state(states)
                    for _, role, label, states in parse_lines(clicked)
                    if role == "Button" and label == target_label
                ]
                check(
                    f"toggle knob {target_label!r} on story {knob_story!r}",
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
