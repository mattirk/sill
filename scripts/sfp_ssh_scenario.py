#!/usr/bin/env python3
"""Real loopback OpenSSH conformance, using only disposable keys and daemon state."""
import json
import os
from pathlib import Path
import pwd
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from sfp_client import Client

binary = str(Path(sys.argv[1]).resolve())
processes = []
with tempfile.TemporaryDirectory(prefix="sill-sfp-ssh-") as directory:
    root = Path(directory)
    env = os.environ.copy()
    for name in ("SILL_SESSION", "SILL_BOX", "INVOCATION_ID", "NOTIFY_SOCKET", "LISTEN_PID", "LISTEN_FDS"):
        env.pop(name, None)
    for name, child in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run")):
        (root / child).mkdir()
        env[name] = str(root / child)
    env["SILL_LAUNCHER"] = "direct"
    def start(argv, **kwargs):
        process = subprocess.Popen(argv, **kwargs)
        processes.append(process)
        return process
    def wait_path(path):
        deadline = time.monotonic() + 10
        while not path.exists():
            if time.monotonic() > deadline:
                raise RuntimeError(f"timeout waiting for {path}")
            for process in processes:
                if process.poll() is not None:
                    raise RuntimeError(f"process exited: {process.args}")
            time.sleep(.03)
    try:
        with (root / "daemon.log").open("w") as log:
            daemon = start([binary, "daemon"], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)
        remote_socket = root / "run/sill/daemon.sock"
        wait_path(remote_socket)
        for name in ("host", "client"):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / name)], check=True)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        config = root / "sshd.conf"
        config.write_text(f"Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/host\nPidFile {root}/sshd.pid\nAuthorizedKeysFile {root}/client.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowStreamLocalForwarding yes\n")
        start([shutil.which("sshd"), "-D", "-f", str(config), "-E", str(root / "sshd.log")])
        deadline = time.monotonic() + 5
        while True:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(.03)
        ssh_config = root / "ssh.conf"
        ssh_config.write_text(f"Host sfp-fixture\n HostName 127.0.0.1\n Port {port}\n User {pwd.getpwuid(os.getuid()).pw_name}\n IdentityFile {root}/client\n IdentitiesOnly yes\n StrictHostKeyChecking no\n UserKnownHostsFile {root}/known_hosts\n BatchMode yes\n")
        forwarded = root / "forward.sock"
        with (root / "ssh.log").open("w") as log:
            start(["ssh", "-F", str(ssh_config), "-o", "ExitOnForwardFailure=yes", "-N", "-L", f"{forwarded}:{remote_socket}", "sfp-fixture"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)
        wait_path(forwarded)
        client = Client(forwarded)
        status = client.call("_sill/daemon.status", {})
        assert status["pid"] == daemon.pid
        opened = client.call("_sill/terminals.open", {"label": "ssh-fixture", "argv": ["/bin/sh", "-c", "printf READY; read reply; printf 'DONE:%s\\n' \"$reply\""], "cols": 80, "rows": 24})
        key = {"terminal_id": opened["terminal_id"]}
        token = client.call("_sill/terminals.attach", key)["token"]
        with client.open_stream(token) as stream:
            data = b""
            while b"READY" not in data:
                data += stream.recv(4096)
        client.close()
        # A new SSH channel/control connection adopts the same shell and replays its bytes.
        client = Client(forwarded)
        assert client.call("_sill/terminals.get", key)["pid"] == opened["pid"]
        client.call("_sill/terminals.resize", dict(key, cols=100, rows=30))
        token = client.call("_sill/terminals.attach", key)["token"]
        with client.open_stream(token) as stream:
            stream.sendall(b"through-ssh\n")
            data = b""
            while b"DONE:through-ssh" not in data:
                chunk = stream.recv(4096)
                assert chunk, data
                data += chunk
            assert b"READY" in data
        client.close()
        assert daemon.poll() is None
        # Also exercise Sill's actual selected-box SSH command, not just port forwarding.
        wrapper = root / "remote-sill"
        exports = " ".join(f"{name}={shlex.quote(env[name])}" for name in ("HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "SILL_LAUNCHER"))
        wrapper.write_text(f"#!/bin/sh\nunset SILL_SESSION SILL_BOX\nexport {exports}\nexec {shlex.quote(binary)} \"$@\"\n")
        wrapper.chmod(0o700)
        tools = root / "bin"
        tools.mkdir()
        ssh = tools / "ssh"
        ssh.write_text(f"#!/bin/sh\nexec {shlex.quote(shutil.which('ssh'))} -F {shlex.quote(str(ssh_config))} \"$@\"\n")
        ssh.chmod(0o700)
        (root / "config/sill").mkdir(exist_ok=True)
        (root / "config/sill/boxes.json").write_text(json.dumps({"test": {"host": "sfp-fixture", "binary": str(wrapper)}}))
        frontend_env = dict(env, PATH=str(tools) + os.pathsep + env.get("PATH", ""))
        result = subprocess.run([binary, "--box", "test", "daemon", "status"], env=frontend_env, capture_output=True, text=True, timeout=15)
        assert result.returncode == 0, result.stderr
        assert f"pid {daemon.pid}" in result.stdout
        # the command left the box's shared connection behind; a second one rides it
        sockets = [p for p in (Path(env["XDG_RUNTIME_DIR"]) / "sill/ssh").iterdir() if p.is_socket()]
        assert len(sockets) == 1, sockets
        result = subprocess.run([binary, "--box", "test", "daemon", "status"], env=frontend_env, capture_output=True, text=True, timeout=15)
        assert result.returncode == 0, result.stderr
        result = subprocess.run([binary, "box", "disconnect", "test"], env=frontend_env, capture_output=True, text=True, timeout=15)
        assert result.returncode == 0, result.stderr
        assert not sockets[0].exists(), "disconnect closes the shared connection"
        print("real SSH: public schema/control, terminal replay/reattach/resize, and selected-box transport passed")
    except Exception:
        for log in root.glob("*.log"):
            print(log.name, log.read_text(), file=sys.stderr)
        raise
    finally:
        # a shared connection left behind would outlive this script by its ControlPersist time
        for master in (root / "run/sill/ssh").glob("*"):
            if master.is_socket():
                subprocess.run(["ssh", "-o", f"ControlPath={master}", "-O", "exit", "sfp-fixture"], capture_output=True, timeout=5)
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
        for process in reversed(processes):
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
