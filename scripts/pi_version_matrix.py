#!/usr/bin/env python3
"""Verify a pi version against everything Harvester reads from the wire.

Run on a throwaway VM whenever pi ships a new version:
    ssh exe.dev new --json --name pi-matrix
    scp scripts/pi_version_matrix.py <vm>.exe.xyz:/tmp/
    ssh <vm>.exe.xyz 'pi --version && timeout 400 python3 /tmp/pi_version_matrix.py'
    ssh exe.dev rm pi-matrix

Checks, in order: the extension_ui_request dialog shape (before seeding the
LLM-integration preference), get_available_models immediately after spawn
(five times -- catches snapshot-warmup regressions), --session-id
create/resume plus stdin-close exit codes (undocumented API, cannot be
tolerated client-side), tool_execution_* event shapes, get_state/set_model,
and get_entries replay with leafId. Results land in /tmp/vcheck_out.json,
the raw stream in /tmp/vcheck_raw.jsonl. Verified matrix lives in
docs/HARNESS_RESEARCH.md.
"""

import json
import subprocess
import time

OUT = {}
RAW = open("/tmp/vcheck_raw.jsonl", "w")


def spawn(extra):
    return subprocess.Popen(
        ["pi", "--mode", "rpc"] + extra,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def send(child, obj):
    child.stdin.write(json.dumps(obj) + "\n")
    child.stdin.flush()


def read_until(child, pred, collect):
    for line in child.stdout:
        RAW.write(line)
        try:
            ev = json.loads(line)
        except Exception:
            continue
        collect.append(ev)
        if pred(ev):
            return True
    return False


def close_and_exit_code(child):
    child.stdin.close()
    try:
        return child.wait(timeout=20)
    except subprocess.TimeoutExpired:
        child.kill()
        return "TIMEOUT-KILLED"


def is_response(rid):
    return lambda ev: ev.get("type") == "response" and ev.get("id") == rid


print("== A: extension_ui_request without pref file")
child = spawn(["--no-session"])
send(child, {"id": "m0", "type": "get_available_models"})
seen = []
read_until(child, is_response("m0"), seen)
uis = [ev for ev in seen if ev.get("type") == "extension_ui_request"]
OUT["A_extension_ui"] = uis[0] if uis else "NONE-SEEN"
OUT["A_models_without_integration"] = len((seen[-1].get("data") or {}).get("models", []))
OUT["A_exit"] = close_and_exit_code(child)

print("== B: seed pref file")
import os
os.makedirs(os.path.expanduser("~/.pi/agent"), exist_ok=True)
open(os.path.expanduser("~/.pi/agent/exe-dev-llm-integration.json"), "w").write(
    '{"version":1,"useExeIntegration":true}')

print("== C: models immediately after spawn x5")
counts = []
for attempt in range(5):
    child = spawn(["--no-session"])
    send(child, {"id": "m1", "type": "get_available_models"})
    seen = []
    start = time.time()
    read_until(child, is_response("m1"), seen)
    data = seen[-1].get("data") or {}
    counts.append({"models": len(data.get("models", [])), "secs": round(time.time() - start, 2)})
    close_and_exit_code(child)
OUT["C_model_counts"] = counts

print("== D: session-id create, stdin-close, resume, tools, set_model")
SID = "thermo-vcheck-0843"
child = spawn(["--session-id", SID])
send(child, {"id": "s1", "type": "get_state"})
seen = []
read_until(child, is_response("s1"), seen)
OUT["D_create_sessionId"] = (seen[-1].get("data") or {}).get("sessionId")
OUT["D_create_exit"] = close_and_exit_code(child)
OUT["D_create_stderr"] = child.stderr.read()[:300]

child = spawn(["--session-id", SID])
send(child, {"id": "s2", "type": "get_state"})
seen = []
read_until(child, is_response("s2"), seen)
OUT["D_resume_sessionId"] = (seen[-1].get("data") or {}).get("sessionId")

send(child, {"id": "p1", "type": "prompt",
             "message": "Use the bash tool to run: echo vcheck-ok. Then reply DONE."})
seen = []
read_until(child, lambda ev: ev.get("type") == "agent_settled", seen)
tools = [ev for ev in seen if str(ev.get("type", "")).startswith("tool_execution")]
OUT["D_tool_event_types"] = [ev.get("type") for ev in tools]
OUT["D_tool_start_keys"] = sorted(tools[0].keys()) if tools else "NONE"
OUT["D_tool_end"] = next((ev for ev in tools if ev.get("type") == "tool_execution_end"), "NONE")

send(child, {"id": "m2", "type": "get_available_models"})
seen = []
read_until(child, is_response("m2"), seen)
models = (seen[-1].get("data") or {}).get("models", [])
current = None
send(child, {"id": "s3", "type": "get_state"})
seen = []
read_until(child, is_response("s3"), seen)
state = seen[-1].get("data") or {}
current = (state.get("model") or {}).get("id")
OUT["D_state_keys"] = sorted(state.keys())
other = next((m for m in models if m.get("id") != current), None)
if other:
    send(child, {"id": "sm1", "type": "set_model",
                 "provider": other.get("provider"), "modelId": other.get("id")})
    seen = []
    read_until(child, is_response("sm1"), seen)
    OUT["D_set_model_success"] = seen[-1].get("success")
    send(child, {"id": "s4", "type": "get_state"})
    seen = []
    read_until(child, is_response("s4"), seen)
    OUT["D_model_after_set"] = ((seen[-1].get("data") or {}).get("model") or {}).get("id")
    OUT["D_model_target"] = other.get("id")
OUT["D_session_exit"] = close_and_exit_code(child)

print("== E: replay entries on a third open")
child = spawn(["--session-id", SID])
send(child, {"id": "e1", "type": "get_entries"})
seen = []
read_until(child, is_response("e1"), seen)
data = seen[-1].get("data") or {}
OUT["E_entry_count"] = len(data.get("entries", []))
OUT["E_has_leafId"] = "leafId" in data
OUT["E_exit"] = close_and_exit_code(child)

RAW.close()
print(json.dumps(OUT, indent=1)[:4000])
open("/tmp/vcheck_out.json", "w").write(json.dumps(OUT, indent=1))
