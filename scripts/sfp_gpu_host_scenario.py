#!/usr/bin/env python3
"""Focused GPU host test. Run with --binary path/to/new/silli; uses a throwaway daemon."""
import argparse
import pathlib
import shlex
import shutil
import tempfile
import time
from sfp_permissions_host_scenario import Env, sh, states, code, REPO

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--binary", required=True, type=pathlib.Path)
args = p.parse_args()
root = pathlib.Path(tempfile.mkdtemp(prefix="gpu-host-", dir=REPO / "target"))
env = Env(args.binary.resolve(), root, "scope")
created = []
try:
    env.start_daemon()
    c = env.client()
    def create(name, permissions="", ghost=False):
        sid = c.call("_silli/sessions.create", {
            "create_id": name, "agent": "stub", "policy": str(env.policy(name, permissions=permissions)),
            "pins": [], "ghost": ghost})["session_id"]
        created.append(sid)
        return sid

    def stop(sid):
        c.call("_silli/sessions.stop", {"session_id": sid})
        deadline = time.monotonic() + 20
        while c.call("_silli/sessions.get", {"session_id": sid})["running"]:
            assert time.monotonic() < deadline
            time.sleep(.1)

    sid = create("gpu")
    c.call("_silli/sessions.start", {"session_id": sid})
    status, out = sh(c, sid, "test ! -d /dev/dri")
    assert status == 0, out
    print("PASS default has no render node", flush=True)
    c.call("_silli/permissions.set", {"session_id": sid, "capability": "gpu", "enabled": True})
    stop(sid)
    c.call("_silli/sessions.start", {"session_id": sid})
    status, out = sh(c, sid, 'ls -l /dev/dri; set -- /dev/dri/*; test "$#" = 1 && test -c "$1" && case "$1" in /dev/dri/renderD*) true;; *) false;; esac && test ! -e /dev/nvidia0')
    assert status == 0, out
    print("PASS GPU enabled, render-only devices:\n" + out, flush=True)
    probe = (REPO / "scripts/fixtures/gpu_render_probe.py").read_text()
    status, out = sh(c, sid, "python3 -c " + shlex.quote(probe))
    print(out, flush=True)
    assert status == 0, "hardware render failed"
    print("PASS hardware OpenGL pixel rendering", flush=True)
    stop(sid)
    env.stop_daemon()
    env.start_daemon()
    c = env.client()
    state = states(c.call("_silli/policy.preview", {"session_id": sid}))["gpu"]
    assert state["effective"], state
    print("PASS GPU choice survives daemon restart", flush=True)
    c.call("_silli/permissions.set", {"session_id": sid, "capability": "test_desktop", "enabled": True})
    c.call("_silli/sessions.start", {"session_id": sid})
    status, out = sh(c, sid, "timeout 4 weston-simple-egl")
    print("Desktop client: " + out, flush=True)
    assert status == 124, (status, out)
    print("PASS EGL client runs in private Weston desktop", flush=True)
    c.call("_silli/permissions.set", {"session_id": sid, "capability": "gpu", "enabled": False})
    stop(sid)
    c.call("_silli/sessions.start", {"session_id": sid})
    status, out = sh(c, sid, "test ! -d /dev/dri")
    assert status == 0, out
    print("PASS disabling GPU removes render node on rebuild", flush=True)
    for name, permissions, ghost in [("forbidden", 'gpu="forbidden"', False), ("ghost", "", True)]:
        restricted = create(name, permissions, ghost)
        assert code(c.call, "_silli/permissions.set", {"session_id": restricted, "capability": "gpu", "enabled": True}) is not None
        print("PASS " + name + " refuses GPU", flush=True)
finally:
    for sid in created:
        try:
            c.call("_silli/sessions.stop", {"session_id": sid})
        except Exception:
            pass
    env.stop_daemon()
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(env.run, ignore_errors=True)
