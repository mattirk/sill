#!/usr/bin/env python3
"""Permission modes on a real host: jails, proxy enforcement, session grants and hostile probes.

Runs a throwaway daemon with its own HOME, runtime and state directories, a stub ACP agent and
real bubblewrap jails (systemd scopes when the user manager answers). Every check that needs a
jail runs a shell inside it through a session terminal. Needs internet access for the public-web
checks (example.com/.org/.net, www.iana.org) and Python jsonschema (scripts/requirements-sfp.txt):

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


def scope_active(session_id):
    done = subprocess.run(["systemctl", "--user", "is-active", f"sill-{session_id}.scope"],
                          capture_output=True, text=True)
    return done.stdout.strip() == "active"


# --- the scenario -----------------------------------------------------------------------------

def scenario(env, port, lan):
    c = env.client()
    gtk = env.client(modes=False, name="gtk-like")
    created = []

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
        d = create("desktop", env.policy("desktop", permissions='test_desktop = "on"\ngpu = "forbidden"'))
        shown = env.cli("permissions", "show", d).stdout
        info("sill permissions show (desktop request):\n       " + "\n       ".join(shown.splitlines()))
        line = next((l for l in shown.splitlines() if l.startswith("test_desktop")), "")
        check("the CLI shows a requested desktop as unavailable, policy default on",
              "unavailable" in line and "policy default: on" in line, shown)
        check("the CLI shows a forbidden capability",
              any(l.startswith("gpu") and "policy default: forbidden" in l for l in shown.splitlines()), shown)
        check("a test-desktop request refuses launch (no backend)", code(c.call, "_sill/sessions.start", {"session_id": d}) is not None)
        time.sleep(0.5)
        check("…and no jail or scope was created (no fallback)", not scope_active(d) and not get(d)["running"])
        check("a GTK-like client cannot start it", code(gtk.call, "_sill/sessions.start", {"session_id": d}) == "unsupported")
        check("a GTK-like client cannot approve hosts for it",
              code(gtk.call, "_sill/policy.allow_host", {"session_id": d, "host": OTHER}) == "unsupported")
        check("a GTK-like client cannot change choices",
              code(gtk.call, "_sill/permissions.set", {"session_id": a, "capability": "public_web", "enabled": False}) == "unsupported")
        # a running Default jail whose next build would request a test desktop
        nxt = create("next-desktop", env.policy("next-desktop"))
        start(nxt)
        env.policy("next-desktop", permissions='test_desktop = "on"')
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
        env.cli("permissions", "save-host", b, THIRD)
        stub = c.call("_sill/agents.get", {"agent_id": "stub"})
        check("Save for future sessions adds the host to the agent's defaults", THIRD in stub["domains"], json.dumps(stub)[:300])
        check("…written to the throwaway HOME only", THIRD in (env.home / ".config/sill/agents/stub.toml").read_text())
        check("a running session does not gain the saved host", refused(c, b, THIRD))
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
