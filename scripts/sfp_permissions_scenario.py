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
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(10)
        self.socket.connect(path)
        self.input = self.socket.makefile("rb")
        self.sequence = 0
        self.call("_sill/initialize", {"protocols": ["sfp/1-dev.3"], "client": {"name": "permission-test", "version": "1"}, "capabilities": {}})

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
    with tempfile.TemporaryDirectory(prefix="sill-permissions-daemon-") as temporary:
        root = pathlib.Path(temporary)
        environment = os.environ.copy()
        for name in ("SILL_SESSION", "SILL_BOX", "INVOCATION_ID", "NOTIFY_SOCKET", "LISTEN_PID", "LISTEN_FDS"):
            environment.pop(name, None)
        for name, child in (("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run")):
            (root / child).mkdir(mode=0o700)
            environment[name] = str(root / child)
        environment["SILL_LAUNCHER"] = "direct"
        with (root / "daemon.log").open("w+") as log:
            daemon = subprocess.Popen([str(pathlib.Path(sys.argv[2]).resolve()), "daemon"], env=environment,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)
            try:
                path = root / "run/sill/daemon.sock"
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
try:
    with tempfile.TemporaryDirectory(prefix="sill-network-policies-") as temporary:
        root = pathlib.Path(temporary)
        def session(name, network):
            path = root / (name + ".toml")
            path.write_text("name='" + name + "'\n[network]\n" + network)
            return client.call("_sill/sessions.create", {
                "create_id": name, "agent": "claude", "policy": str(path)
            })["session_id"]
        def denied(session_id, host):
            try:
                client.call("_sill/policy.allow_host", {"session_id": session_id, "host": host})
            except RpcError:
                return
            raise AssertionError("forbidden approval accepted")
        first = session("first", "mode='allow'")
        second = session("second", "mode='allow'")
        agent = client.call("_sill/agents.get", {"agent_id": "claude"})
        result = client.call("_sill/policy.allow_host", {"session_id": first, "host": "permission-fixture.example.com"})
        assert result == {"agent_name": "this session", "live": False}, result
        assert client.call("_sill/agents.get", {"agent_id": "claude"}) == agent
        assert "permission-fixture.example.com" in client.call("_sill/policy.preview", {"session_id": first})["text"]
        assert "permission-fixture.example.com" not in client.call("_sill/policy.preview", {"session_id": second})["text"]
        denied(session("offline", "mode='none'"), "example.com")
        denied(session("ceiling", "allow_additional_hosts=false"), "example.com")
        denied(first, "*.example.com")
        # Previously remembered grants do not survive a newly imposed hard ceiling in previews.
        (root / "first.toml").write_text("name='first'\n[network]\nallow_additional_hosts=false")
        assert "permission-fixture.example.com" not in client.call("_sill/policy.preview", {"session_id": first})["text"]
        denied(first, "another.example.com")
        print("Session network approval isolation, persistence preview, limits and default preservation passed")
finally:
    client.close()
