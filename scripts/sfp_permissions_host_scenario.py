#!/usr/bin/env python3
"""Permission modes on a real host: jails, proxy enforcement, session grants and hostile probes.

Runs a throwaway daemon with its own HOME, runtime and state directories, a stub ACP agent and
real bubblewrap jails (systemd scopes when the user manager answers). Every check that needs a
jail runs a shell inside it through a session terminal. Needs internet access for the public-web
checks (example.com/.org/.net, www.iana.org), Python jsonschema (scripts/requirements-sfp.txt),
and for the test desktop Weston, Xwayland, xdotool and ImageMagick (`magick`), for the private bus
dbus-daemon and dbus-send, and for Host control's display operations the system Python's
python-xlib (the witness window); it runs headless, since the throwaway daemon gets no display
until section 19 gives it a headless one of its own:

    python scripts/sfp_permissions_host_scenario.py --binary target/local/sill [--keep]

It never connects to, restarts or changes the person's own daemon, configuration or sessions;
sessions it creates are stopped and deleted at the end. Not runnable inside a Sill jail (nested
namespaces are refused there). Prints one line per check and exits nonzero when any fails.
"""
import argparse
import base64
import http.server
import json
import os
import pathlib
import re
import shutil
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from sfp_client import Client, RpcError  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
STUB = REPO / "scripts/fixtures/stub_acp_agent.py"
WITNESS = REPO / "scripts/fixtures/x11_input_witness.py"
ALLOWED, OTHER, THIRD, FOURTH = "example.com", "example.org", "example.net", "www.iana.org"
MODES = {"sill.permission_modes": 1}

results = []


def check(name, condition, detail=""):
    results.append((name, bool(condition), detail))
    print(("PASS " if condition else "FAIL ") + name + (f" — {detail}" if detail and not condition else ""), flush=True)
    return condition


def info(text):
    print("     " + text, flush=True)


# --- environment -----------------------------------------------------------------------------

def policy_text(name, mode="allow", allow=(ALLOWED,), extra_network="", permissions=""):
    """A complete policy from the shipped default: its masks, hidden mounts and limits are kept
    (a policy without `hidden_mounts` cannot overlay a vfat /boot), the network replaced."""
    text = (REPO / "policies/default.toml").read_text()
    text = re.sub(r'^name = "default"', f'name = "{name}"', text, count=1, flags=re.M)
    text = re.sub(r'^mode = "allow".*$', f'mode = "{mode}"', text, count=1, flags=re.M)
    text = re.sub(r'^allow = \[.*?^\]', "allow = " + json.dumps(list(allow)) + "\n" + extra_network,
                  text, count=1, flags=re.M | re.S)
    if permissions:
        text += "\n[permissions]\n" + permissions + "\n"
    return text


class Env:
    def __init__(self, binary, root, launcher):
        self.binary = binary
        self.root = root
        self.home = root / "home"
        # the runtime directory under /tmp, as production's /run/user/<uid>: both are fresh in a
        # jail, so the daemon's socket is out of reach there (a runtime dir under /home would not be)
        self.run = pathlib.Path(tempfile.mkdtemp(prefix="sill-hp-", dir="/tmp"))
        self.policies = root / "policies"
        if root.exists():
            shutil.rmtree(root)
        for path in (self.home / ".config/sill/agents", self.home / ".local/state/sill/tools/stub",
                     self.home / ".ssh", self.policies):
            path.mkdir(parents=True)
        agent = self.home / ".local/state/sill/tools/stub/agent"
        shutil.copy(STUB, agent)
        agent.chmod(0o755)
        (self.home / ".config/sill/agents/stub.toml").write_text(
            f'id = "stub"\nname = "Stub agent"\ncommand = ["{agent}"]\nsupports_load = true\n'
            'domains = ["stub-agent.invalid"]\n')
        (self.home / ".ssh/id_planted").write_text("planted secret\n")
        (self.home / ".config/sill/sill.toml").write_text("theme = \"iceberg-dark\"\n")
        self.environment = {k: v for k, v in os.environ.items() if k not in (
            "SILL_SESSION", "SILL_BOX", "INVOCATION_ID", "NOTIFY_SOCKET", "LISTEN_PID", "LISTEN_FDS",
            "XDG_CONFIG_HOME", "XDG_STATE_HOME", "DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS")}
        self.environment.update(HOME=str(self.home), XDG_RUNTIME_DIR=str(self.run))
        if launcher == "direct":
            self.environment["SILL_LAUNCHER"] = "direct"
        else:
            # scopes in the person's user manager, as production sessions get them
            uid = os.getuid()
            for name in ("systemd", "bus"):
                os.symlink(f"/run/user/{uid}/{name}", self.run / name)
        self.socket = self.run / "sill/daemon.sock"
        self.daemon = None

    def start_daemon(self):
        log = (self.root / "daemon.log").open("a")
        self.daemon = subprocess.Popen([str(self.binary), "daemon"], env=self.environment, cwd=self.root,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        deadline = time.monotonic() + 15
        while True:
            try:
                probe = socket.socket(socket.AF_UNIX)
                probe.connect(str(self.socket))
                probe.close()
                return
            except OSError:
                if self.daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError((self.root / "daemon.log").read_text()[-2000:] or "daemon startup timed out")
                time.sleep(0.05)

    def stop_daemon(self):
        """The daemon leaves; running sessions keep going (as `sill daemon stop`)."""
        if self.daemon is None:
            return
        self.cli("daemon", "stop", check=False)
        try:
            self.daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait()
        self.daemon = None

    def cli(self, *args, check=True):
        done = subprocess.run([str(self.binary), *args], env=self.environment, capture_output=True, text=True, timeout=60)
        if check and done.returncode != 0:
            raise RuntimeError(f"sill {' '.join(args)}: {done.stderr.strip()}")
        return done

    def policy(self, name, **kwargs):
        path = self.policies / f"{name}.toml"
        path.write_text(policy_text(name, **kwargs))
        return path

    def client(self, modes=True, name="host-permissions"):
        return Client(self.socket, capabilities=MODES if modes else {}, name=name)


def host_path(path):
    raw = os.fsencode(str(path))
    return {"bytes_base64": base64.b64encode(raw).decode(), "display": str(path)}


class Fixture(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"host-loopback-fixture\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


# --- jail access ------------------------------------------------------------------------------

def sh(client, session_id, script, timeout=60):
    """Run `script` with /bin/sh in the session's jail; (exit status, output)."""
    marker = "__SILL_EXIT"
    term = client.call("_sill/terminals.open", {
        "session_id": session_id, "label": "host-scenario",
        "argv": ["/bin/sh", "-c", "(\n" + script + f'\n)\necho "{marker}:$?"'], "cols": 240, "rows": 60})
    token = client.call("_sill/terminals.attach", {"terminal_id": term["terminal_id"]})["token"]
    stream = client.open_stream(token)
    stream.settimeout(timeout)
    output = b""
    try:
        while not re.search(rb"__SILL_EXIT:(\d+)", output):
            chunk = stream.recv(65536)
            if not chunk:
                break
            output += chunk
    finally:
        stream.close()
        try:
            client.call("_sill/terminals.close", {"terminal_id": term["terminal_id"]})
        except RpcError:
            pass
    text = output.decode(errors="replace").replace("\r\n", "\n")
    found = re.search(r"__SILL_EXIT:(\d+)", text)
    if not found:
        raise RuntimeError(f"no exit marker from the jail: {text[-500:]!r}")
    return int(found.group(1)), text[:found.start()].strip()


def fetch(client, session_id, url, proxy=True):
    """`curl` from the jail: (HTTP status, CONNECT status) as strings; 000 when none."""
    flags = "" if proxy else "--noproxy '*' "
    code, out = sh(client, session_id,
                   f"curl -s -o /dev/null --max-time 15 {flags}-w '%{{http_code}} %{{http_connect}}' '{url}'")
    parts = out.split()
    return (parts[0], parts[1]) if len(parts) == 2 else ("ERR", out)


def via_proxy(client, session_id, url):
    """Plain HTTP through the session proxy even for loopback names curl would not proxy."""
    code, out = sh(client, session_id,
                   f"curl -s -o /dev/null --max-time 15 --noproxy '' -x \"$HTTP_PROXY\" -w '%{{http_code}}' '{url}'")
    return out.strip() or "ERR"


def reaches(client, session_id, host):
    return fetch(client, session_id, f"https://{host}/")[0] == "200"


def refused(client, session_id, host):
    """Refused by the session proxy (CONNECT 403), not merely failing."""
    status, connect = fetch(client, session_id, f"https://{host}/")
    return status == "000" and connect == "403"


def refused_or_offline(client, session_id, host):
    """No connection to `host`: refused by the proxy, or no proxy answer at all (offline)."""
    return fetch(client, session_id, f"https://{host}/")[0] != "200"


def code(call, method, params):
    try:
        call(method, params)
    except RpcError as error:
        return error.code or error.error.get("code")
    return None


def states(view):
    return {item["capability"]: item for item in view["permissions"]}


def read(path):
    try:
        return pathlib.Path(path).read_bytes().decode(errors="replace")
    except OSError:
        return ""


def comm(pid):
    return read(f"/proc/{pid}/comm").strip()


def scope_active(session_id):
    done = subprocess.run(["systemctl", "--user", "is-active", f"sill-{session_id}.scope"],
                          capture_output=True, text=True)
    return done.stdout.strip() == "active"


# --- the scenario -----------------------------------------------------------------------------

def scenario(env, port, lan):
    c = env.client()
    gtk = env.client(modes=False, name="gtk-like")
    created = []
    background = []

    def create(name, policy, pins=(), ghost=False):
        sid = c.call("_sill/sessions.create", {"create_id": f"host-{name}", "agent": "stub", "policy": str(policy),
                                               "pins": [host_path(p) for p in pins], "ghost": ghost})["session_id"]
        created.append(sid)
        return sid

    def start(sid, client=None):
        (client or c).call("_sill/sessions.start", {"session_id": sid})
        # the launcher retries a jail that ends at once; give it time to settle, then prove it runs
        code_, out = sh(c, sid, "echo up")
        return out == "up"

    def restart(sid):
        c.call("_sill/sessions.stop", {"session_id": sid})
        deadline = time.monotonic() + 20
        while c.call("_sill/sessions.get", {"session_id": sid})["running"] and time.monotonic() < deadline:
            time.sleep(0.2)
        return start(sid)

    def get(sid):
        return c.call("_sill/sessions.get", {"session_id": sid})

    try:
        # -- 1. two ordinary sessions, same agent and policy text, separate files ---------------
        a = create("a", env.policy("a"))
        b = create("b", env.policy("b"))
        check("jail A starts with a complete custom policy", start(a))
        check("jail B starts concurrently", start(b))
        check("A reaches the allow-listed host", reaches(c, a, ALLOWED))
        check("A: other public host refused by its proxy (CONNECT 403)", refused(c, a, OTHER))
        check("B: other public host refused", refused(c, b, OTHER))
        denied = c.call("_sill/policy.denied", {"session_id": a})
        check("A's refusal is listed for approval", OTHER in denied["hosts"], json.dumps(denied))

        # -- 2. hostile probes in Default -------------------------------------------------------
        status, out = sh(c, a, f"""
            fail=0
            [ -z "$DISPLAY$WAYLAND_DISPLAY" ] || {{ echo "display env: $DISPLAY$WAYLAND_DISPLAY"; fail=1; }}
            case "$DBUS_SESSION_BUS_ADDRESS" in disabled:*) ;; *) echo "bus: $DBUS_SESSION_BUS_ADDRESS"; fail=1;; esac
            for p in /tmp/.X11-unix/X0 /run/user/{os.getuid()}/bus /run/user/{os.getuid()}/sill/daemon.sock \
                     /run/user/{os.getuid()}/wayland-0 /var/lib/sill "$HOME/.ssh/id_planted" \
                     "$HOME/.config/sill/sill.toml" "$HOME/.local/state/sill/sill.db" \
                     /.sill/host/var/lib/sill/{os.getuid()} "/.sill/host$HOME/.ssh/id_planted" \
                     {env.socket} {env.home}/.local/state/sill/sill.db; do
                [ -e "$p" ] && {{ echo "visible: $p"; fail=1; }}
            done
            python3 - <<'PY' || fail=1
import socket, sys
bad = 0
for name in ("\\0/tmp/.X11-unix/X0", "\\0/tmp/.ICE-unix/0"):
    s = socket.socket(socket.AF_UNIX)
    try:
        s.connect(name)
        print("abstract socket reachable:", repr(name)); bad = 1
    except OSError:
        pass
for host, port in (("127.0.0.1", {port}), ("{lan}", {port}), ("1.1.1.1", 443)):
    s = socket.socket(); s.settimeout(3)
    try:
        s.connect((host, port)); print("direct TCP reachable:", host, port); bad = 1
    except OSError:
        pass
sys.exit(bad)
PY
            n=$(ls /proc | grep -c '^[0-9]')
            [ "$n" -lt 40 ] || {{ echo "pid namespace shows $n processes"; fail=1; }}
            exit $fail""")
        check("Default jail: no display/bus env, host sockets, sill state, planted secret, abstract X11, "
              "host loopback/LAN/direct internet or host processes", status == 0, out)
        # an agent cannot grant itself anything: no daemon socket in the jail, and the CLI refuses
        status, out = sh(c, a, f"timeout 20 sill permissions set {a} public_web on 2>&1; "
                               f"env -u SILL_SESSION timeout 20 sill permissions set {a} public_web on 2>&1; exit 0")
        info("in-jail `sill permissions set`: " + " | ".join(out.splitlines()[-2:]))
        check("a permission change from inside the jail does not reach the daemon",
              states(c.call("_sill/policy.preview", {"session_id": a}))["public_web"]["requested"] is None)
        check("proxy refuses host loopback", via_proxy(c, a, f"http://127.0.0.1:{port}/") == "403")
        check("proxy refuses the host's LAN address", via_proxy(c, a, f"http://{lan}:{port}/") == "403")
        check("proxy refuses localhost by name", via_proxy(c, a, f"http://localhost:{port}/") == "403")

        # -- 3. session-only Allow, live on a running jail --------------------------------------
        agent_before = c.call("_sill/agents.get", {"agent_id": "stub"})
        allowed = c.call("_sill/policy.allow_host", {"session_id": a, "host": OTHER})
        check("Allow on a running session is live", allowed.get("live") is True, json.dumps(allowed))
        check("A reaches the approved host without a restart", reaches(c, a, OTHER))
        check("B (same agent) still refused the host A approved", refused(c, b, OTHER))
        check("Allow leaves the agent's defaults unchanged", c.call("_sill/agents.get", {"agent_id": "stub"}) == agent_before)
        check("a live Allow does not mark the jail outdated", not get(a)["policy_changed"])
        check("Allow refuses wildcards", code(c.call, "_sill/policy.allow_host", {"session_id": a, "host": "*.example.org"}) is not None)
        # a name resolving to loopback stays refused even when approved: local rules are separate
        c.call("_sill/policy.allow_host", {"session_id": a, "host": "127.0.0.1.nip.io"})
        check("an approved public name resolving to loopback is still refused",
              via_proxy(c, a, f"http://127.0.0.1.nip.io:{port}/") == "403")

        # -- 4. Restore defaults on the running jail --------------------------------------------
        restored = c.call("_sill/permissions.restore_defaults", {"session_id": a})
        info(f"restore_defaults on running A: restart_required={restored['restart_required']}")
        check("Restore defaults stops the run-approved host matching new connections", refused(c, a, OTHER))
        check("Restore defaults keeps the allow-list", reaches(c, a, ALLOWED))
        check("Restore defaults removes the grant from the next build",
              OTHER not in c.call("_sill/policy.preview", {"session_id": a})["text"])

        # -- 5. grants survive stop/reopen ------------------------------------------------------
        c.call("_sill/policy.allow_host", {"session_id": a, "host": OTHER})
        check("reopened A has its approved host again", restart(a) and reaches(c, a, OTHER))
        snapshot = c.call("_sill/policy.snapshot", {"session_id": a})["snapshot"]
        check("A's launch snapshot records the session host", snapshot is not None and OTHER in snapshot["text"])
        check("…and lists it as an applied session grant",
              {"host": OTHER, "applied": True, "reason": ""} in snapshot["session_hosts"], json.dumps(snapshot["session_hosts"]))
        check("B still refused after A's reopen", refused(c, b, OTHER))

        # -- 6. public web as a session choice --------------------------------------------------
        changed = c.call("_sill/permissions.set", {"session_id": b, "capability": "public_web", "enabled": True})
        check("public_web on a running jail needs a restart", changed["restart_required"], json.dumps(changed)[:300])
        check("the running jail is marked outdated", get(b)["policy_changed"])
        check("not effective before the restart", refused(c, b, THIRD))
        check("after the restart B reaches public hosts", restart(b) and reaches(c, b, THIRD) and reaches(c, b, FOURTH))
        snap = states(c.call("_sill/policy.snapshot", {"session_id": b})["snapshot"])["public_web"]
        check("B's snapshot: public_web effective from the session choice",
              snap["effective"] and snap["source"] == "session", json.dumps(snap))
        check("A is unaffected by B's public web", refused(c, a, THIRD))
        check("public web still refuses host loopback", via_proxy(c, b, f"http://127.0.0.1:{port}/") == "403")
        check("public web still refuses the LAN address", via_proxy(c, b, f"http://{lan}:{port}/") == "403")
        check("public web still refuses a public name resolving to loopback",
              via_proxy(c, b, f"http://127.0.0.1.nip.io:{port}/") == "403")
        status, out = sh(c, b, "curl -s -o /dev/null --max-time 8 --noproxy '*' -w '%{http_code}' https://example.org/; echo \" $?\"")
        check("public web is not direct networking (no route without the proxy)", not out.startswith("200"), out)
        c.call("_sill/permissions.set", {"session_id": b, "capability": "public_web", "enabled": False})
        check("turning public web off applies at the next build", restart(b) and refused(c, b, THIRD))
        c.call("_sill/permissions.set", {"session_id": b, "capability": "public_web", "enabled": None})
        view = states(c.call("_sill/policy.preview", {"session_id": b}))["public_web"]
        check("null returns to the policy default", view["requested"] is None and not view["effective"], json.dumps(view))

        # -- 7. a later hard limit blocks remembered grants -------------------------------------
        env.policy("a", extra_network="allow_additional_hosts = false\n")
        check("editing the policy marks running A outdated", get(a)["policy_changed"])
        check("A keeps its launch state until rebuilt", reaches(c, a, OTHER))
        check("after the rebuild the remembered host is blocked by the new ceiling",
              restart(a) and refused(c, a, OTHER))
        hosts = c.call("_sill/policy.preview", {"session_id": a})["session_hosts"]
        check("the preview lists the remembered host as blocked, with the reason",
              len(hosts) == 1 and hosts[0]["host"] == OTHER and not hosts[0]["applied"]
              and "allow_additional_hosts" in hosts[0]["reason"], json.dumps(hosts))
        check("Allow is refused under the ceiling",
              code(c.call, "_sill/policy.allow_host", {"session_id": a, "host": THIRD}) is not None)
        check("public web is refused under the ceiling",
              code(c.call, "_sill/permissions.set", {"session_id": a, "capability": "public_web", "enabled": True}) == "permission_denied")

        # -- 8. repository restrictions ---------------------------------------------------------
        offline_repo = env.root / "repo-offline"
        (offline_repo / ".sill").mkdir(parents=True)
        (offline_repo / ".sill/policy.toml").write_text("[network]\nmode = 'none'\n")
        r = create("offline", env.policy("offline"), pins=[offline_repo])
        check("a session pinned to an offline repository starts", start(r))
        check("offline fragment: the allow-list is refused too", not reaches(c, r, ALLOWED))
        check("offline fragment: Allow is refused",
              code(c.call, "_sill/policy.allow_host", {"session_id": r, "host": OTHER}) is not None)
        check("offline fragment: public web is refused",
              code(c.call, "_sill/permissions.set", {"session_id": r, "capability": "public_web", "enabled": True}) == "permission_denied")
        c.call("_sill/sessions.stop", {"session_id": r})
        time.sleep(1)
        c.call("_sill/policy.start", {"session_id": r, "ignore_roots": [host_path(offline_repo)]})
        sh(c, r, "true")
        check("an explicit one-build exception lifts the fragment", reaches(c, r, ALLOWED))
        check("the exception expires at the next ordinary build", restart(r) and not reaches(c, r, ALLOWED))

        narrow_repo = env.root / "repo-narrow"
        (narrow_repo / ".sill").mkdir(parents=True)
        (narrow_repo / ".sill/policy.toml").write_text("[permissions]\npublic_web = 'forbidden'\n")
        n = create("narrow", env.policy("narrow", mode="all"), pins=[narrow_repo])
        check("network.mode=all with a public-web prohibition starts", start(n))
        check("the prohibition narrows `all` to the allow-list", reaches(c, n, ALLOWED) and refused(c, n, OTHER))
        wide = create("wide", env.policy("wide", mode="all"))
        check("the same policy without the fragment is public", start(wide) and reaches(c, wide, OTHER))

        # a remembered grant meets a fragment added later: blocked while it applies, never dropped
        k = create("kept", env.policy("kept"))
        start(k)
        c.call("_sill/policy.allow_host", {"session_id": k, "host": OTHER})
        c.call("_sill/sessions.pin", {"session_id": k, "path": host_path(offline_repo)})
        check("a later offline fragment blocks the remembered host at the next build",
              restart(k) and refused_or_offline(c, k, OTHER))
        restored = c.call("_sill/permissions.restore_defaults", {"session_id": r})
        check("Restore defaults keeps the session's pins",
              [p["display"] for p in get(r)["pins"]] == [str(offline_repo)], json.dumps(get(r)["pins"]))
        c.call("_sill/sessions.unpin", {"session_id": k, "path": host_path(offline_repo)})
        check("without the fragment the remembered host is back", restart(k) and reaches(c, k, OTHER))

        # -- 9. ghosts ---------------------------------------------------------------------------
        g = create("ghost", env.policy("ghost", mode="all"), ghost=True)
        check("a ghost starts", start(g))
        # a ghost's `all` becomes the shipped default policy's allow-list, not its own policy's list
        check("a ghost's `all` network is narrowed to the default allow-list",
              reaches(c, g, "github.com") and refused(c, g, OTHER) and refused(c, g, ALLOWED))
        check("a ghost refuses capability choices",
              code(c.call, "_sill/permissions.set", {"session_id": g, "capability": "public_web", "enabled": True}) == "permission_denied")
        check("a ghost refuses Save for future sessions",
              code(c.call, "_sill/policy.save_host", {"session_id": g, "host": THIRD}) == "permission_denied")
        ghost_view = states(c.call("_sill/policy.preview", {"session_id": g}))
        check("a ghost forbids every additional capability",
              all(not s["allowed"] for k, s in ghost_view.items() if k != "public_web") and not ghost_view["public_web"]["effective"],
              json.dumps(ghost_view)[:400])

        # -- 10. Desktop development requests and the frontend gate ------------------------------
        # (the GPU has no backend yet: the request without one; the test desktop is 14, the bus 15.)
        d = create("desktop", env.policy("desktop", permissions='gpu = "on"\nprivate_bus = "forbidden"'))
        shown = env.cli("permissions", "show", d).stdout
        info("sill permissions show (GPU request):\n       " + "\n       ".join(shown.splitlines()))
        line = next((l for l in shown.splitlines() if l.startswith("gpu")), "")
        check("the CLI shows a requested GPU as unavailable, policy default on",
              "unavailable" in line and "policy default: on" in line, shown)
        check("the CLI shows a forbidden capability",
              any(l.startswith("private_bus") and "policy default: forbidden" in l for l in shown.splitlines()), shown)
        check("a GPU request refuses launch (no backend)", code(c.call, "_sill/sessions.start", {"session_id": d}) is not None)
        time.sleep(0.5)
        check("…and no jail or scope was created (no fallback)", not scope_active(d) and not get(d)["running"])
        check("a GTK-like client cannot start it", code(gtk.call, "_sill/sessions.start", {"session_id": d}) == "unsupported")
        check("a GTK-like client cannot approve hosts for it",
              code(gtk.call, "_sill/policy.allow_host", {"session_id": d, "host": OTHER}) == "unsupported")
        check("a GTK-like client cannot change choices",
              code(gtk.call, "_sill/permissions.set", {"session_id": a, "capability": "public_web", "enabled": False}) == "unsupported")
        # a running Default jail whose next build would request a Desktop development capability
        nxt = create("next-desktop", env.policy("next-desktop"))
        start(nxt)
        env.policy("next-desktop", permissions='gpu = "on"')
        submit = {"session_id": nxt, "submission_id": "gate-1", "text": "hello"}
        # while running, the launch decides: the jail is still a Default one until it is rebuilt
        check("GTK-like submit reaches the running Default jail despite an edited policy",
              code(gtk.call, "_sill/sessions.submit", submit) is None)
        check("GTK-like clients can stop it", code(gtk.call, "_sill/sessions.stop", {"session_id": nxt}) is None)
        deadline = time.monotonic() + 20
        while get(nxt)["running"] and time.monotonic() < deadline:
            time.sleep(0.2)
        check("stopped, its next build decides: GTK-like submit is refused",
              code(gtk.call, "_sill/sessions.submit", dict(submit, submission_id="gate-2")) == "unsupported")
        check("…and GTK-like start is refused",
              code(gtk.call, "_sill/sessions.start", {"session_id": nxt}) == "unsupported")
        check("a permission-modes client is refused by the missing backend instead",
              code(c.call, "_sill/sessions.start", {"session_id": nxt}) not in (None, "unsupported"))
        check("an ordinary Default session stays operable from a GTK-like client",
              code(gtk.call, "_sill/sessions.submit", {"session_id": b, "submission_id": "gate-3", "text": "hi"}) is None)
        check("host_control can never be on in a policy",
              not c.call("_sill/policy.check", {"text": policy_text("hc", permissions='host_control = "on"')})["valid"])
        check("host_control is never a session choice",
              code(c.call, "_sill/permissions.set", {"session_id": b, "capability": "host_control", "enabled": True}) == "permission_denied")

        # -- 11. daemon restart: running jails keep their actual state ---------------------------
        c.call("_sill/policy.allow_host", {"session_id": b, "host": OTHER})
        c.close()
        gtk.close()
        env.stop_daemon()
        check("B's jail outlives the daemon", scope_active(b) or os.environ.get("SILL_LAUNCHER") == "direct")
        env.start_daemon()
        c = env.client()
        gtk = env.client(modes=False, name="gtk-like")
        check("after a daemon restart B still reaches its live-approved host", reaches(c, b, OTHER))
        check("…and A still refuses it (ceiling)", refused(c, a, OTHER))
        check("A is still reported as running and current", get(a)["running"] and not get(a)["policy_changed"])

        # -- 12. CLI and Save for future sessions ----------------------------------------------
        shown = env.cli("permissions", "show", b).stdout
        check("`sill permissions show` lists the capabilities", "public_web" in shown and "test_desktop" in shown, shown[:300])
        env.cli("permissions", "set", b, "public_web", "on")
        check("`sill permissions set` remembers a choice",
              states(c.call("_sill/policy.preview", {"session_id": b}))["public_web"]["requested"] is True)
        env.cli("permissions", "restore", b)
        check("`sill permissions restore` clears it",
              states(c.call("_sill/policy.preview", {"session_id": b}))["public_web"]["requested"] is None)
        # `wide` runs with nothing pending: an added agent default must not change that
        current_before = get(wide)["running"] and not get(wide)["policy_changed"]
        env.cli("permissions", "save-host", b, THIRD)
        stub = c.call("_sill/agents.get", {"agent_id": "stub"})
        check("Save for future sessions adds the host to the agent's defaults", THIRD in stub["domains"], json.dumps(stub)[:300])
        check("…written to the throwaway HOME only", THIRD in (env.home / ".config/sill/agents/stub.toml").read_text())
        check("a running session does not gain the saved host", refused(c, b, THIRD))
        check("a current running session is not asked to restart for an added agent default",
              current_before and not get(wide)["policy_changed"], json.dumps(get(wide))[:300])
        fresh = create("after-save", env.policy("after-save"))
        check("a new session reaches the saved host", start(fresh) and reaches(c, fresh, THIRD))

        # -- 13. a jail that cannot start is a failed start, not a running session ---------------
        c.close()
        gtk.close()
        env.stop_daemon()
        fake = env.root / "fake-bin"
        fake.mkdir()
        (fake / "bwrap").write_text("#!/bin/sh\necho 'bwrap: fixture refuses' >&2\nexit 1\n")
        (fake / "bwrap").chmod(0o755)
        path = env.environment["PATH"]
        env.environment["PATH"] = f"{fake}:{path}"
        env.start_daemon()
        c = env.client()
        gtk = env.client(modes=False, name="gtk-like")
        broken = create("broken", env.policy("broken"))
        started_at = time.monotonic()
        failure = None
        try:
            c.call("_sill/sessions.start", {"session_id": broken})
        except RpcError as error:
            failure = str(error)
        check("starting a jail that cannot start reports the failure", failure is not None and "could not start" in failure,
              failure or "start succeeded")
        info(f"failed start after {time.monotonic() - started_at:.1f} s: {failure}")
        check("…and the session is not running, with no scope left", not get(broken)["running"] and not scope_active(broken))
        c.close()
        gtk.close()
        env.stop_daemon()
        env.environment["PATH"] = path
        env.start_daemon()
        c = env.client()
        gtk = env.client(modes=False, name="gtk-like")

        # -- 14. the test desktop (Weston + Xwayland, headless: this daemon has no display) ------
        if not shutil.which("weston"):
            check("Weston is installed for the test desktop checks", False, "install weston")
            return created
        desk = create("test-desktop", env.policy("test-desktop", permissions='test_desktop = "on"'))
        view = states(c.call("_sill/policy.preview", {"session_id": desk}))["test_desktop"]
        check("the test desktop is available and on for a requesting policy",
              view["available"] and view["effective"] and "Weston" in view["reason"], json.dumps(view))
        check("a GTK-like client cannot start a test-desktop session",
              code(gtk.call, "_sill/sessions.start", {"session_id": desk}) == "unsupported")
        check("a permission-modes client starts it", start(desk))
        status, out = sh(c, desk, """
            echo "env $WAYLAND_DISPLAY $DISPLAY $DBUS_SESSION_BUS_ADDRESS"
            ls /tmp/.X11-unix
            [ -S "/run/user/$(id -u)/$WAYLAND_DISPLAY" ] && echo wayland-socket
            xdotool getdisplaygeometry
            DISPLAY=:0 xdotool getdisplaygeometry >/dev/null 2>&1 && echo REAL-DISPLAY-REACHED
            python3 - <<'PY'
import socket
s = socket.socket(socket.AF_UNIX)
try:
    s.connect("\\0/tmp/.X11-unix/X0"); print("ABSTRACT-X0-REACHED")
except OSError:
    pass
PY
            [ -e /tmp/.X11-unix/X0 ] && echo HOST-X0-VISIBLE
            timeout 5 weston-debug -l 2>&1 | grep -v "^Available debug streams" | grep . && echo DEBUG-SCOPES-OFFERED
            exit 0""")
        lines = out.splitlines()
        env_line = next((l for l in lines if l.startswith("env ")), "")
        display = env_line.split()[2] if len(env_line.split()) > 2 else ""
        check("the jail gets the desktop's Wayland socket and X11 display, no bus",
              env_line.startswith("env wayland-0 :") and env_line.endswith("disabled:") and "wayland-socket" in lines, out)
        check("only the desktop's own X11 socket is in the jail", f"X{display[1:]}" in lines and "X0" not in lines, out)
        check("X11 programs see the 1280×800 test display", "1280 800" in lines, out)
        check("the real display stays out of reach (path, abstract socket, DISPLAY=:0)",
              not any(m in out for m in ("REAL-DISPLAY-REACHED", "ABSTRACT-X0-REACHED", "HOST-X0-VISIBLE")), out)
        check("the desktop offers no debug scope (no protocol dump of other clients)", "DEBUG-SCOPES-OFFERED" not in out, out)
        # a Wayland client on the desktop, then a screenshot from inside, copied out to look at
        status, out = sh(c, desk, """
            weston-flower >/dev/null 2>&1 &
            weston-simple-shm >/dev/null 2>&1 &
            sleep 1.5
            cd "$(mktemp -d)" && weston-screenshooter >/dev/null 2>&1 && base64 -w0 wayland-screenshot-*.png""", timeout=90)
        shot = env.root / "test-desktop.png"
        try:
            shot.write_bytes(base64.b64decode(out.strip()))
            info_line = subprocess.run(["magick", "identify", "-format", "%w %h %k", str(shot)],
                                       capture_output=True, text=True).stdout.split()
        except Exception as error:  # noqa: BLE001 - reported as the check's detail
            info_line = [str(error)]
        check("a screenshot taken inside shows the 1280×800 desktop with its clients",
              info_line[:2] == ["1280", "800"] and len(info_line) == 3 and int(info_line[2]) > 200,
              " ".join(info_line))
        # the daemon's own pictures, for the live view: taken outside the jail, read in chunks
        frame = c.call("_sill/desktop.capture", {"session_id": desk})
        png = b""
        while True:
            chunk = c.call("_sill/desktop.read", {"session_id": desk, "revision": frame["revision"],
                                                   "offset": str(len(png)), "length": 65536})
            png += base64.b64decode(chunk["bytes_base64"])
            if chunk["eof"]:
                break
        check("the daemon takes pictures of the test desktop for the live view",
              frame["width"] == 1280 and frame["height"] == 800 and frame["scale"] == 1
              and len(png) == int(frame["size"]) and png[:8] == b"\x89PNG\r\n\x1a\n", json.dumps(frame))
        check("a replaced picture is refused rather than mixed",
              code(c.call, "_sill/desktop.read", {"session_id": desk, "revision": "stale", "offset": "0"}) == "conflict")
        # Weston itself: in this session's scope, headless (always), none of the launcher's display
        weston = [pid for pid in os.listdir("/proc") if pid.isdigit() and comm(pid) == "weston"
                  and f"sill-{desk}.scope" in read(f"/proc/{pid}/cgroup")]
        check("Weston runs in the session's scope", len(weston) == 1, str(weston))
        if weston:
            argv = read(f"/proc/{weston[0]}/cmdline").split("\0")
            environ = dict(kv.split("=", 1) for kv in read(f"/proc/{weston[0]}/environ").split("\0") if "=" in kv)
            config = next((a.split("=", 1)[1] for a in argv if a.startswith("--config=")), "")
            check("…headless, its own weston.ini only, software rendering, its own runtime directory",
                  "--backend=headless" in argv and config.endswith("/desktop/weston.ini") and "--renderer=pixman" in argv
                  and "--debug-scopes=none" in argv
                  and environ.get("XDG_RUNTIME_DIR", "").endswith("/desktop") and environ.get("HOME") == environ.get("XDG_RUNTIME_DIR"),
                  " ".join(argv))
            check("…and none of the person's display or bus variables",
                  not any(k in environ for k in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS")),
                  " ".join(sorted(environ)))
        x11 = pathlib.Path(f"/tmp/.X11-unix/X{display[1:]}") if display else None
        c.call("_sill/sessions.stop", {"session_id": desk})
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and (any(pathlib.Path(f"/proc/{p}").exists() for p in weston)
                                               or (x11 is not None and x11.exists())):
            time.sleep(0.2)
        check("stopping the session stops Weston and removes its X11 socket",
              not any(pathlib.Path(f"/proc/{p}").exists() for p in weston) and x11 is not None and not x11.exists())
        # as a session choice: a restart builds it in, and out again
        choice = create("desktop-choice", env.policy("desktop-choice"))
        start(choice)
        changed = c.call("_sill/permissions.set", {"session_id": choice, "capability": "test_desktop", "enabled": True})
        check("turning the test desktop on for a running session needs a restart", changed["restart_required"])
        has_desktop = lambda sid: sh(c, sid, 'echo "[$WAYLAND_DISPLAY]"')[1] == "[wayland-0]"
        check("after the restart the session has its desktop", restart(choice) and has_desktop(choice))
        c.call("_sill/permissions.set", {"session_id": choice, "capability": "test_desktop", "enabled": False})
        check("turned off, the next build has none", restart(choice) and not has_desktop(choice))
        ghost_desktop = create("ghost-desktop", env.policy("ghost-desktop", permissions='test_desktop = "on"'), ghost=True)
        check("a ghost refuses a test desktop",
              not states(c.call("_sill/policy.preview", {"session_id": ghost_desktop}))["test_desktop"]["allowed"]
              and code(c.call, "_sill/permissions.set", {"session_id": ghost_desktop, "capability": "test_desktop", "enabled": True}) == "permission_denied")
        # a Weston that cannot start fails the launch: no jail, never the real display instead
        c.close()
        gtk.close()
        env.stop_daemon()
        fake = env.root / "fake-weston"
        fake.mkdir()
        (fake / "weston").write_text('#!/bin/sh\n[ "$1" = --version ] && { echo "weston 15.0.1"; exit 0; }\necho "weston: fixture refuses" >&2\nexit 1\n')
        (fake / "weston").chmod(0o755)
        env.environment["PATH"] = f"{fake}:{path}"
        env.start_daemon()
        c = env.client()
        gtk = env.client(modes=False, name="gtk-like")
        broken = create("broken-desktop", env.policy("broken-desktop", permissions='test_desktop = "on"'))
        failure = None
        try:
            c.call("_sill/sessions.start", {"session_id": broken})
        except RpcError as error:
            failure = str(error)
        check("a test desktop that cannot start fails the launch, saying why",
              failure is not None and "test desktop" in failure and "fixture refuses" in failure, failure or "started")
        info(f"broken Weston: {failure}")
        check("…with no jail or scope left", not get(broken)["running"] and not scope_active(broken))
        c.close()
        gtk.close()
        env.stop_daemon()
        env.environment["PATH"] = path
        # the person's screen scale: a HiDPI picture of a desktop of the same size
        env.environment["SILL_TEST_DESKTOP_SCALE"] = "2"
        env.start_daemon()
        c = env.client()
        gtk = env.client(modes=False, name="gtk-like")
        hidpi = create("hidpi-desktop", env.policy("hidpi-desktop", permissions='test_desktop = "on"'))
        start(hidpi)
        frame = c.call("_sill/desktop.capture", {"session_id": hidpi})
        status, out = sh(c, hidpi, "xdotool getdisplaygeometry")
        check("at scale 2 the desktop renders 2560×1600 for a 1280×800 desktop",
              (frame["width"], frame["height"], frame["scale"]) == (2560, 1600, 2) and out.strip() == "1280 800",
              f"{json.dumps(frame)} {out}")
        check("a session without a test desktop has no pictures",
              code(c.call, "_sill/desktop.capture", {"session_id": a}) == "not_found")
        c.close()
        gtk.close()
        env.stop_daemon()
        del env.environment["SILL_TEST_DESKTOP_SCALE"]
        env.start_daemon()
        c = env.client()
        gtk = env.client(modes=False, name="gtk-like")

        # -- 15. the private D-Bus session bus (a dbus-daemon of the jail's own, inside it) --------
        if not shutil.which("dbus-daemon"):
            check("dbus-daemon is installed for the private bus checks", False, "install dbus")
            return created
        bus = create("private-bus", env.policy("private-bus", permissions='private_bus = "on"'))
        view = states(c.call("_sill/policy.preview", {"session_id": bus}))["private_bus"]
        check("the private bus is available and on for a requesting policy",
              view["available"] and view["effective"] and "jail's own" in view["reason"], json.dumps(view))
        check("a GTK-like client cannot start a private-bus session",
              code(gtk.call, "_sill/sessions.start", {"session_id": bus}) == "unsupported")
        check("a permission-modes client starts it", start(bus))
        names = "dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.ListNames"
        status, out = sh(c, bus, f"""
            echo "env [$DBUS_SESSION_BUS_ADDRESS] [$DBUS_SYSTEM_BUS_ADDRESS] [$WAYLAND_DISPLAY]"
            [ -S "/run/user/$(id -u)/bus" ] && echo bus-socket
            {names} 2>&1 | grep -c 'string "' | sed 's/^/names /'
            {names} 2>&1 | grep -q systemd1 && echo HOST-NAMES-VISIBLE
            dbus-send --system --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.ListNames >/dev/null 2>&1 && echo SYSTEM-BUS-REACHED
            [ -e /run/dbus/system_bus_socket ] && echo SYSTEM-SOCKET-VISIBLE
            mkdir -p ~/.local/share/dbus-1/services
            printf '[D-BUS Service]\\nName=org.sill.Probe\\nExec=/bin/sh -c "echo activated > /tmp/sill-activated; exec sleep 20"\\n' > ~/.local/share/dbus-1/services/org.sill.Probe.service
            dbus-send --session --print-reply --reply-timeout=4000 --dest=org.sill.Probe / org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1
            sleep 1
            [ -f /tmp/sill-activated ] && echo activated-in-jail
            exit 0""", timeout=90)
        lines = out.splitlines()
        env_line = next((l for l in lines if l.startswith("env ")), "")
        check("the jail's bus address is its own socket; the system bus stays dead; no desktop",
              env_line == f"env [unix:path=/run/user/{os.getuid()}/bus] [disabled:] []" and "bus-socket" in lines, out)
        names_line = next((l for l in lines if l.startswith("names ")), "names 0")
        check("dbus-send talks to the private bus", int(names_line.split()[1]) >= 2, out)
        check("none of the person's bus names, no system bus, no system socket",
              not any(m in out for m in ("HOST-NAMES-VISIBLE", "SYSTEM-BUS-REACHED", "SYSTEM-SOCKET-VISIBLE")), out)
        check("a service file of the agent's own activates inside the jail, not on the host",
              "activated-in-jail" in lines and not pathlib.Path("/tmp/sill-activated").exists(), out)
        # (an activation leaves a forked babysitter with the same comm until the service settles)
        daemons = [pid for pid in os.listdir("/proc") if pid.isdigit() and comm(pid) == "dbus-daemon"
                   and f"sill-{bus}.scope" in read(f"/proc/{pid}/cgroup")]
        parent = lambda pid: next((l.split()[1] for l in read(f"/proc/{pid}/status").splitlines() if l.startswith("PPid:")), "")
        daemons = [pid for pid in daemons if parent(pid) not in daemons]
        check("one dbus-daemon runs in the session's scope", len(daemons) == 1, str(daemons))
        if daemons:
            argv = read(f"/proc/{daemons[0]}/cmdline").split("\0")
            environ = dict(kv.split("=", 1) for kv in read(f"/proc/{daemons[0]}/environ").split("\0") if "=" in kv)
            try:
                jailed = os.stat(f"/proc/{daemons[0]}/ns/mnt").st_ino != os.stat("/proc/self/ns/mnt").st_ino
            except OSError as error:
                jailed = f"ns: {error}"
            check("…inside the jail (its mount namespace, the jail's environment), on sill's configuration only",
                  jailed is True and environ.get("SILL_SESSION") == "1" and "DISPLAY" not in environ
                  and any(a.endswith("/sill-bus.conf") for a in argv) and "--nofork" in argv,
                  f"{jailed} {' '.join(argv)} {' '.join(sorted(environ))}")
        c.call("_sill/sessions.stop", {"session_id": bus})
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and any(pathlib.Path(f"/proc/{p}").exists() for p in daemons):
            time.sleep(0.2)
        check("stopping the session ends its dbus-daemon", not any(pathlib.Path(f"/proc/{p}").exists() for p in daemons))
        # a Default session has none; as a session choice it comes with the next build
        plain = create("no-bus", env.policy("no-bus"))
        start(plain)
        has_bus = lambda sid: sh(c, sid, f'echo "[$DBUS_SESSION_BUS_ADDRESS]"; {names} >/dev/null 2>&1 && echo BUS-REACHED')[1]
        before = has_bus(plain)
        check("a Default session has a dead bus address and no bus", before == "[disabled:]", before)
        changed = c.call("_sill/permissions.set", {"session_id": plain, "capability": "private_bus", "enabled": True})
        check("turning the private bus on for a running session needs a restart", changed["restart_required"])
        after = has_bus(plain) if restart(plain) else "no restart"
        check("after the restart the session has its bus", after.endswith("BUS-REACHED") and "unix:path=" in after, after)
        ghost_bus = create("ghost-bus", env.policy("ghost-bus", permissions='private_bus = "on"'), ghost=True)
        check("a ghost refuses a private bus",
              not states(c.call("_sill/policy.preview", {"session_id": ghost_bus}))["private_bus"]["allowed"])

        # -- 16. Apply development permissions: the test desktop and the private bus together -----
        dev = create("development", env.policy("development"))
        start(dev)
        check("a GTK-like client cannot apply development permissions",
              code(gtk.call, "_sill/permissions.apply_development", {"session_id": dev}) == "unsupported")
        changed = c.call("_sill/permissions.apply_development", {"session_id": dev})
        st = states(changed["preview"])
        check("applied together: both are session choices for the next build, restart required",
              all(st[k]["requested"] and st[k]["source"] == "session" and st[k]["effective"] for k in ("test_desktop", "private_bus"))
              and changed["restart_required"], json.dumps({k: st[k] for k in ("test_desktop", "private_bus")})[:400])
        both = sh(c, dev, 'echo "[$WAYLAND_DISPLAY] [$DBUS_SESSION_BUS_ADDRESS]"')[1] if restart(dev) else "no restart"
        check("after the restart the session has its desktop and its bus",
              both == f"[wayland-0] [unix:path=/run/user/{os.getuid()}/bus]", both)
        st = states(c.call("_sill/permissions.restore_defaults", {"session_id": dev})["preview"])
        check("Restore defaults takes both away again for the next build",
              all(st[k]["requested"] is None and not st[k]["effective"] for k in ("test_desktop", "private_bus")))
        half = create("half-forbidden", env.policy("half-forbidden", permissions='private_bus = "forbidden"'))
        try:
            c.call("_sill/permissions.apply_development", {"session_id": half})
            refusal = None
        except RpcError as error:
            refusal = str(error)
        st = states(c.call("_sill/policy.preview", {"session_id": half}))
        check("a forbidden half refuses the group as a whole, naming it, and records nothing",
              refusal is not None and "private_bus" in refusal and st["test_desktop"]["requested"] is None,
              f"{refusal} {json.dumps(st['test_desktop'])}")
        check("a ghost refuses development permissions",
              code(c.call, "_sill/permissions.apply_development", {"session_id": ghost_bus}) == "permission_denied")

        # -- 17. Host control: an activation a displaying frontend holds --------------------------
        HOST = {"sill.permission_modes": 1, "sill.host_control": 1}
        hc = Client(env.socket, capabilities=HOST, name="host-control")
        ctl = create("host-control", env.policy("host-control"))
        activate = lambda client, sid, ops: client.call("_sill/host_control.activate", {"session_id": sid, "operations": ops})
        hc_state = lambda sid: c.call("_sill/host_control.state", {"session_id": sid})
        check("a GTK-like client cannot activate Host control",
              code(gtk.call, "_sill/host_control.activate", {"session_id": ctl, "operations": ["launch"]}) == "unsupported")
        check("a permission-modes client without the host-control capability cannot either",
              code(c.call, "_sill/host_control.activate", {"session_id": ctl, "operations": ["launch"]}) == "unsupported")
        check("a stopped session cannot be activated",
              code(hc.call, "_sill/host_control.activate", {"session_id": ctl, "operations": ["launch"]}) == "invalid_state")
        start(ctl)
        st = activate(hc, ctl, ["launch", "stop"])
        check("activation: generation 1 with the named operations, one displaying frontend",
              st["active"] and st["generation"] == 1 and st["operations"] == ["launch", "stop"] and st["displaying"] == 1, json.dumps(st))
        rec = c.call("_sill/sessions.get", {"session_id": ctl})
        check("the session record carries the active brief for every client",
              rec.get("host_control") == {"generation": 1, "operations": ["launch", "stop"]}, json.dumps(rec.get("host_control")))
        st = activate(hc, ctl, ["launch", "stop", "commands"])
        check("a scope change is a new generation", st["active"] and st["generation"] == 2 and "commands" in st["operations"], json.dumps(st))
        sub_id, view = c.attach(ctl)
        texts = " | ".join((item["content"][0].get("text") or "") for item in view.items("notice"))
        check("activation and scope change are notices in the conversation",
              "Host control activated (generation 1)" in texts and "scope changed (generation 2)" in texts, texts[:300])
        c.call("_sill/sessions.detach", {"session_id": ctl, "subscription_id": sub_id})
        st = hc.call("_sill/host_control.revoke", {"session_id": ctl})
        check("revoked by the person: inactive, with the reason", not st["active"] and st["ended"]["reason"].startswith("revoked"), json.dumps(st))
        check("…and the record no longer carries it", c.call("_sill/sessions.get", {"session_id": ctl}).get("host_control") is None)
        activate(hc, ctl, ["inspect"])
        hc.close()
        time.sleep(1)
        st = hc_state(ctl)
        check("the activating frontend leaving ends the activation", not st["active"] and "displaying" in st["ended"]["reason"], json.dumps(st))
        hc = Client(env.socket, capabilities=HOST, name="host-control")
        hc2 = Client(env.socket, capabilities=HOST, name="host-control-2")
        activate(hc, ctl, ["inspect"])
        hc2.attach(ctl)
        st = hc_state(ctl)
        check("an attached host-control frontend displays it too", st["active"] and st["displaying"] == 2, json.dumps(st))
        hc.close()
        time.sleep(1)
        st = hc_state(ctl)
        check("the activation survives while another displaying frontend stays attached", st["active"] and st["displaying"] == 1, json.dumps(st))
        hc2.close()
        time.sleep(1)
        st = hc_state(ctl)
        check("…and ends when that one goes too", not st["active"], json.dumps(st))
        hc = Client(env.socket, capabilities=HOST, name="host-control")
        activate(hc, ctl, ["launch"])
        c.call("_sill/sessions.stop", {"session_id": ctl})
        st = hc_state(ctl)
        check("stopping the session ends its Host control", not st["active"] and "stopped" in st["ended"]["reason"], json.dumps(st))
        no_host = create("no-host-control", env.policy("no-host-control", permissions='host_control = "forbidden"'))
        start(no_host)
        check("a forbidding policy refuses activation",
              code(hc.call, "_sill/host_control.activate", {"session_id": no_host, "operations": ["launch"]}) == "permission_denied")
        check("a ghost refuses activation",
              code(hc.call, "_sill/host_control.activate", {"session_id": ghost_bus, "operations": ["launch"]}) == "permission_denied")
        # -- 18. The jailed host bridge, generations, scope and process disposition ---------------
        start(ctl)
        def bridge(sid, op, generation=None, args=None):
            argv = ["/.sill/bin/sill-host", op]
            if op != "state":
                argv += [str(generation), json.dumps(args or {})]
            rc, text = sh(c, sid, shlex.join(argv))
            return rc, text

        check("inactive bridge refuses the agent", bridge(ctl, "state")[0] != 0)
        generation = activate(hc, ctl, ["launch", "stop", "inspect"])["generation"]
        rc, text = bridge(ctl, "state")
        check("agent can read the current generation", rc == 0 and json.loads(text)["generation"] == generation, text)
        check("commands needs its separate grant", bridge(ctl, "commands", generation,
              {"argv": ["/bin/true"], "cwd": "/tmp"})[0] != 0)
        check("a stale generation is refused", bridge(ctl, "inspect", generation - 1)[0] != 0)
        check("another session cannot use this bridge", bridge(no_host, "state")[0] != 0)
        rc, text = bridge(ctl, "launch", generation, {"argv": ["/bin/sh", "-c", "echo bridge-output; exec sleep 120"], "cwd": "/tmp"})
        check("agent launches a host process", rc == 0, text)
        launched = json.loads(text)
        name = launched["name"]
        time.sleep(.3)
        rc, output = bridge(ctl, "inspect", generation, {"name": name})
        check("agent reads captured host output", rc == 0 and "bridge-output" in output, output)
        rows = c.call("_sill/procs.list", {"session_id": ctl})["processes"]
        check("process list identifies the host owner and location", any(p["name"] == name and p["location"] == "host" for p in rows))
        rc, listed = sh(c, ctl, "/.sill/bin/sill-proc list")
        check("the jail's sill-proc lists the host process, marked as on the host",
              rc == 0 and name in listed and "on the host" in listed and "running" in listed, listed)
        rc, tailed = sh(c, ctl, f"/.sill/bin/sill-proc tail {name}")
        check("…and tails its output", rc == 0 and "bridge-output" in tailed, tailed)
        rc, refused_stop = sh(c, ctl, f"/.sill/bin/sill-proc stop {name}")
        check("…but cannot stop it: it points at sill-host", rc != 0 and "sill-host stop" in refused_stop
              and pathlib.Path(f"/proc/{launched['pid']}").exists(), refused_stop)
        check("sill-proc tail of a host name of another session finds nothing",
              sh(c, no_host, f"/.sill/bin/sill-proc tail {name}")[0] != 0)
        hc.call("_sill/host_control.keep", {"session_id": ctl, "name": name, "keep": True})
        hc.call("_sill/host_control.revoke", {"session_id": ctl})
        check("explicitly kept process survives revocation", pathlib.Path(f"/proc/{launched['pid']}").exists())
        rc, listed = sh(c, ctl, "/.sill/bin/sill-proc list")
        check("…and stays listed in the jail as kept after revocation", rc == 0 and name in listed and "kept" in listed, listed)
        check("revocation removes agent access", bridge(ctl, "state")[0] != 0)
        generation = activate(hc, ctl, ["stop", "launch"])["generation"]
        check("new activation can deliberately stop a kept owned process", bridge(ctl, "stop", generation, {"name": name})[0] == 0)
        rc, text = bridge(ctl, "launch", generation, {"argv": ["/bin/sleep", "120"], "cwd": "/tmp"})
        check("second host launch succeeds", rc == 0, text)
        second = json.loads(text)
        hc.call("_sill/host_control.revoke", {"session_id": ctl})
        time.sleep(.3)
        check("unkept host process is stopped on revocation", not pathlib.Path(f"/proc/{second['pid']}").exists())
        rc, listed = sh(c, ctl, "/.sill/bin/sill-proc list")
        check("…and the jail's list shows it stopped", rc == 0 and second["name"] in listed and "stopped" in listed, listed)

        # -- 19. display operations: no display, then a headless Weston/Xwayland as the host display
        generation = activate(hc, ctl, ["view", "input"])["generation"]
        rc, text = bridge(ctl, "view", generation, {})
        check("view without a host display is refused, saying why", rc != 0 and "DISPLAY" in text, text)
        rc, text = bridge(ctl, "input", generation, {"actions": [{"key": "a"}]})
        check("input likewise, and nothing is typed", rc != 0 and "DISPLAY" in text, text)
        hc.close()
        c.close()
        env.stop_daemon()
        hd = env.run / "hostdisplay"
        hd.mkdir(mode=0o700)
        # a headless Weston, and on it a full-screen (not rootless) Xwayland: an X server with a
        # readable root window and XTEST input, like the person's Xorg, and no window manager
        weston = subprocess.Popen(
            [shutil.which("weston"), "--backend=headless", "--renderer=pixman", "--width=1280", "--height=800",
             "--socket=wayland-hd", "--idle-time=0", "--no-config", f"--log={hd}/weston.log"],
            env={"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": str(hd), "XDG_RUNTIME_DIR": str(hd)},
            cwd=hd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True)
        background.append(weston)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not (hd / "wayland-hd").exists():
            time.sleep(0.1)
        display_number = next(n for n in range(50, 100) if not pathlib.Path(f"/tmp/.X11-unix/X{n}").exists())
        xwayland = subprocess.Popen(
            [shutil.which("Xwayland"), f":{display_number}", "-noreset", "-fullscreen", "-geometry", "1280x800"],
            env={"PATH": "/usr/bin:/bin", "HOME": str(hd), "XDG_RUNTIME_DIR": str(hd), "WAYLAND_DISPLAY": "wayland-hd"},
            cwd=hd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=(hd / "xwayland.log").open("w"),
            start_new_session=True)
        background.append(xwayland)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not pathlib.Path(f"/tmp/.X11-unix/X{display_number}").exists():
            time.sleep(0.1)
        time.sleep(0.5)
        check("a headless host display for the throwaway daemon (Weston + a full Xwayland)",
              xwayland.poll() is None and pathlib.Path(f"/tmp/.X11-unix/X{display_number}").exists(),
              read(hd / "xwayland.log")[-500:])
        env.environment["DISPLAY"] = f":{display_number}"
        env.start_daemon()
        c = env.client()
        hc = Client(env.socket, capabilities=HOST, name="host-control")
        witness_log = env.root / "witness.log"
        witness = subprocess.Popen(["/usr/bin/python3", "-I", str(WITNESS), str(witness_log)],
                                   env={"DISPLAY": env.environment["DISPLAY"], "PATH": "/usr/bin:/bin"},
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        background.append(witness)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not (witness_log.exists() and "mapped" in read(witness_log)):
            time.sleep(0.1)
        time.sleep(0.5)
        check("the witness window is up on it", "mapped" in read(witness_log), read(witness_log))
        check("the jail outlived the daemon restart", sh(c, ctl, "echo up")[1] == "up")
        generation = activate(hc, ctl, ["view", "input"])["generation"]
        rc, text = bridge(ctl, "view", generation, {})
        check("agent takes a picture of the host desktop", rc == 0, text)
        shot = json.loads(text) if rc == 0 else {}
        check("…1280×800, a PNG under the bridge directory",
              shot.get("width") == 1280 and shot.get("height") == 800 and shot.get("path", "").startswith("/.sill/bridge/view/")
              and shot.get("location") == "host", json.dumps(shot))
        rc, sig = sh(c, ctl, f"head -c 8 {shot.get('path', '/nonexistent')} | od -An -tx1 | tr -d ' \\n'")
        check("…readable inside the jail", sig == "89504e470d0a1a0a", sig)
        host_png = env.run / "sill" / ctl / "bridge/view" / pathlib.PurePath(shot.get("path", "x")).name
        ihdr = host_png.read_bytes()[16:24] if host_png.exists() else b""
        check("…the host file carries the desktop's size", ihdr == (1280).to_bytes(4, "big") + (800).to_bytes(4, "big"))
        rc, text = bridge(ctl, "view", generation, {"list": True})
        rows = json.loads(text).get("windows", []) if rc == 0 else []
        witness_window = next((w for w in rows if w["name"] == "sill-witness"), None)
        check("view lists the desktop's windows with the witness", witness_window is not None, text[:300])
        witness_window = witness_window or {"window": 0, "x": 0, "y": 0, "width": 0, "height": 0}
        rc, text = bridge(ctl, "view", generation, {"window": witness_window["window"]})
        part = json.loads(text) if rc == 0 else {}
        check("a selected window's area of the desktop", rc == 0 and (part.get("width"), part.get("height")) == (300, 200)
              and part.get("window") == witness_window["window"], text)
        target = (witness_window["x"] + 50, witness_window["y"] + 50)
        rc, text = bridge(ctl, "input", generation, {"actions": [
            {"move": list(target)}, {"click": 1}, {"wait": 100}, {"key": "a"}, {"type": "hi"}]})
        done = json.loads(text) if rc == 0 else {}
        check("agent gives the host desktop input", rc == 0 and done.get("performed") == 5, text)
        check("…the pointer went where it was sent", (done.get("pointer") or {}).get("x") == target[0]
              and (done.get("pointer") or {}).get("y") == target[1], json.dumps(done))
        time.sleep(0.5)
        seen = read(witness_log)
        check("…the window received the click and the keys", "button 1" in seen and "key a" in seen
              and "key h" in seen and "key i" in seen, seen)
        check("an action the daemon does not know is refused whole",
              bridge(ctl, "input", generation, {"actions": [{"key": "a"}, {"run": "xterm"}]})[0] != 0)
        check("a key with shell characters is refused",
              bridge(ctl, "input", generation, {"actions": [{"key": "a; rm -rf /"}]})[0] != 0)
        check("too many actions are refused",
              bridge(ctl, "input", generation, {"actions": [{"wait": 1}] * 33})[0] != 0)
        before = len(read(witness_log))
        generation = activate(hc, ctl, ["input"])["generation"]
        check("viewing needs its own grant", bridge(ctl, "view", generation, {})[0] != 0)
        generation = activate(hc, ctl, ["view"])["generation"]
        rc, text = bridge(ctl, "input", generation, {"actions": [{"key": "z"}]})
        check("input needs its own grant", rc != 0 and len(read(witness_log)) == before, text)
        check("pictures stay while control is active", host_png.exists())
        hc.call("_sill/host_control.revoke", {"session_id": ctl})
        check("revocation removes the pictures", not host_png.parent.exists())
        hc.close()
    finally:
        for sid in created:
            for method in ("_sill/sessions.stop", "_sill/sessions.delete"):
                try:
                    c.call(method, {"session_id": sid})
                except Exception:
                    pass
            if method == "_sill/sessions.delete":
                time.sleep(0.1)
        try:
            c.close()
            gtk.close()
        except Exception:
            pass
        for process in background:
            process.kill()
            process.wait()
    return created


def lan_address():
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect(("192.0.2.1", 9))  # no packet is sent; picks the outgoing interface
        return probe.getsockname()[0]
    finally:
        probe.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--root", type=pathlib.Path, default=REPO / "target/tmp/host-permissions",
                        help="throwaway directory on disk (not /tmp: the daemon stages its binary there)")
    parser.add_argument("--launcher", choices=("scope", "direct"), default="scope")
    parser.add_argument("--keep", action="store_true", help="leave the throwaway directory for inspection")
    options = parser.parse_args()
    if os.environ.get("SILL_SESSION"):
        sys.exit("run this on the host, not inside a Sill jail")
    env = Env(options.binary.resolve(), options.root.resolve(), options.launcher)
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 0), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port, lan = server.server_address[1], lan_address()
    created = []
    try:
        env.start_daemon()
        created = scenario(env, port, lan)
    finally:
        server.shutdown()
        env.stop_daemon()
        leftovers = [sid for sid in created if scope_active(sid)]
        for sid in leftovers:
            subprocess.run(["systemctl", "--user", "stop", f"sill-{sid}.scope"], check=False)
        check("cleanup: no session scope left running", not leftovers, " ".join(leftovers))
        if not options.keep:
            shutil.rmtree(env.root, ignore_errors=True)
            shutil.rmtree(env.run, ignore_errors=True)
    failed = [name for name, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
