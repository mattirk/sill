#!/usr/bin/env python3
"""Session network approval checks against an isolated daemon; no agent is launched."""
import pathlib
import sys
import tempfile
import json
import socket

class RpcError(Exception):
    pass

class Client:
    """Small independent wire client; no optional Python dependencies required."""
    def __init__(self, path, capabilities=None):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(10)
        self.socket.connect(path)
        self.input = self.socket.makefile("rb")
        self.sequence = 0
        self.capabilities = self.call("_silli/initialize", {"protocols": ["sfp/1-dev.3"], "client": {"name": "permission-test", "version": "1"}, "capabilities": capabilities or {}})["capabilities"]

    def call(self, method, params):
        self.sequence += 1
        request = {"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params}
        self.socket.sendall(json.dumps(request).encode() + b"\n")
        while True:
            line = self.input.readline()
            if not line:
                raise RuntimeError("daemon disconnected")
            response = json.loads(line)
            if response.get("id") != self.sequence:
                continue
            if "error" in response:
                raise RpcError(response["error"])
            return response["result"]

    def close(self):
        self.input.close()
        self.socket.close()


if sys.argv[1] == "--binary":
    import os
    import subprocess
    import time
    with tempfile.TemporaryDirectory(prefix="silli-permissions-daemon-") as temporary:
        root = pathlib.Path(temporary)
        environment = os.environ.copy()
        for name in ("SILLI_SESSION", "SILLI_BOX", "INVOCATION_ID", "NOTIFY_SOCKET", "LISTEN_PID", "LISTEN_FDS"):
            environment.pop(name, None)
        # HOME too: agent definitions and Save for future sessions use $HOME/.config/silli/agents,
        # which would otherwise be the person's own (this scenario saves a host for `claude`)
        for name, child in (("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"), ("HOME", "home")):
            (root / child).mkdir(mode=0o700)
            environment[name] = str(root / child)
        environment["SILLI_LAUNCHER"] = "direct"
        with (root / "daemon.log").open("w+") as log:
            daemon = subprocess.Popen([str(pathlib.Path(sys.argv[2]).resolve()), "daemon"], env=environment,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)
            try:
                path = root / "run/silli/daemon.sock"
                deadline = time.monotonic() + 10
                while not path.exists():
                    if daemon.poll() is not None or time.monotonic() > deadline:
                        log.seek(0)
                        raise RuntimeError(log.read() or "daemon startup timed out")
                    time.sleep(.025)
                subprocess.run([sys.executable, __file__, str(path)], env=environment, check=True)
            finally:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()
    sys.exit(0)

client = Client(sys.argv[1])
modes = Client(sys.argv[1], {"silli.permission_modes": 1})
try:
    with tempfile.TemporaryDirectory(prefix="silli-network-policies-") as temporary:
        root = pathlib.Path(temporary)
        def session(name, network):
            path = root / (name + ".toml")
            path.write_text("name='" + name + "'\n[network]\n" + network)
            return client.call("_silli/sessions.create", {
                "create_id": name, "agent": "claude", "policy": str(path)
            })["session_id"]
        def denied(session_id, host):
            try:
                client.call("_silli/policy.allow_host", {"session_id": session_id, "host": host})
            except RpcError:
                return
            raise AssertionError("forbidden approval accepted")
        first = session("first", "mode='allow'")
        second = session("second", "mode='allow'")
        agent = client.call("_silli/agents.get", {"agent_id": "claude"})
        result = client.call("_silli/policy.allow_host", {"session_id": first, "host": "permission-fixture.example.com"})
        assert result == {"agent_name": "this session", "live": False}, result
        assert client.call("_silli/agents.get", {"agent_id": "claude"}) == agent
        assert "permission-fixture.example.com" in client.call("_silli/policy.preview", {"session_id": first})["text"]
        assert "permission-fixture.example.com" not in client.call("_silli/policy.preview", {"session_id": second})["text"]
        denied(session("offline", "mode='none'"), "example.com")
        denied(session("ceiling", "allow_additional_hosts=false"), "example.com")
        denied(first, "*.example.com")
        # Permission inspection must distinguish an eligible capability from an implemented backend.
        desktop = session("desktop", "mode='allow'\n[permissions]\ngpu='on'\nhost_control='forbidden'")
        view = client.call("_silli/policy.preview", {"session_id": desktop})
        states = {item["capability"]: item for item in view["permissions"]}
        # GPU availability depends on accessible supported host devices.
        assert states["gpu"]["allowed"] and states["gpu"]["default_enabled"]
        assert states["gpu"]["effective"] == states["gpu"]["available"]
        assert not states["host_control"]["allowed"] and not states["host_control"]["effective"]
        if not states["gpu"]["available"]:
            try:
                modes.call("_silli/sessions.start", {"session_id": desktop})
            except RpcError as error:
                assert "render node" in str(error), error
            else:
                raise AssertionError("unavailable GPU request started a jail")
        assert not client.call("_silli/policy.check", {"text": "name='bad'\n[permissions]\nhost_control='on'"})["valid"]
        web = session("web", "mode='allow'\n[permissions]\npublic_web='on'")
        states = {item["capability"]: item for item in client.call("_silli/policy.preview", {"session_id": web})["permissions"]}
        assert states["public_web"]["available"] and states["public_web"]["effective"]
        # Previously remembered grants do not survive a newly imposed hard ceiling in previews.
        (root / "first.toml").write_text("name='first'\n[network]\nallow_additional_hosts=false")
        assert "permission-fixture.example.com" not in client.call("_silli/policy.preview", {"session_id": first})["text"]
        denied(first, "another.example.com")
        # Session capability choices need the frontend-declared capability; clients without it (line mode, scripts) are refused.
        assert "silli.permission_modes" not in client.capabilities and modes.capabilities["silli.permission_modes"] == 1
        def code(call, *args):
            try:
                call(*args)
            except RpcError as error:
                return error.args[0].get("data", {}).get("code") or error.args[0]["code"]
            raise AssertionError("refusal expected")
        choice = lambda session_id, capability, enabled: {"session_id": session_id, "capability": capability, "enabled": enabled}
        assert code(client.call, "_silli/permissions.set", choice(second, "public_web", True)) == "unsupported"
        changed = modes.call("_silli/permissions.set", choice(second, "public_web", True))
        states = {item["capability"]: item for item in changed["preview"]["permissions"]}
        assert states["public_web"]["effective"] and states["public_web"]["source"] == "session", states
        assert not changed["restart_required"]
        assert code(modes.call, "_silli/permissions.set", choice(second, "gpu", True)) == "permission_denied"
        assert code(modes.call, "_silli/permissions.set", choice(second, "host_control", True)) == "permission_denied"
        assert code(modes.call, "_silli/permissions.set", choice(session("restricted", "mode='allow'\nallow_additional_hosts=false"), "public_web", True)) == "permission_denied"
        restored = modes.call("_silli/permissions.restore_defaults", {"session_id": second})
        states = {item["capability"]: item for item in restored["preview"]["permissions"]}
        assert not states["public_web"]["effective"] and states["public_web"]["requested"] is None, states
        # A session requesting Desktop development capabilities refuses operation without them.
        assert code(client.call, "_silli/policy.allow_host", {"session_id": desktop, "host": "example.com"}) == "unsupported"
        assert code(client.call, "_silli/sessions.start", {"session_id": desktop}) == "unsupported"
        modes.call("_silli/policy.allow_host", {"session_id": desktop, "host": "example.com"})
        saved = modes.call("_silli/policy.save_host", {"session_id": second, "host": "*.saved-fixture.example"})
        assert saved["agent_id"] == "claude"
        assert "*.saved-fixture.example" in client.call("_silli/agents.get", {"agent_id": "claude"})["domains"]
        print("Session network approvals, capability inspection, limits, unsupported-backend refusal, session choices, Restore defaults and the frontend gate passed")
finally:
    client.close()
    modes.close()
