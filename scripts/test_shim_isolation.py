#!/usr/bin/env python3
"""Linux regression: python3 scripts/test_shim_isolation.py target/debug/silli-shim.

Uses disposable processes and sockets only. The positive control must succeed so an
ambient seccomp/Yama restriction cannot masquerade as a successful protection test.
Run inside the same jail configuration used by the agent.
"""
import ctypes
import errno
import os
import socket
import subprocess
import sys
import tempfile

libc = ctypes.CDLL(None, use_errno=True)


def duplicate(pid, fd):
    handle = os.pidfd_open(pid)
    try:
        result = libc.syscall(438, handle, fd, 0)  # pidfd_getfd on Linux arm64/x86-64
        error = ctypes.get_errno()
        if result >= 0:
            os.close(result)
        return result, error
    finally:
        os.close(handle)


with tempfile.TemporaryDirectory(prefix="silli-shim-isolation-") as directory:
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(directory + "/control")
        listener.listen()
        fd = listener.fileno()
        control = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], pass_fds=(fd,))
        try:
            result, error = duplicate(control.pid, fd)
            assert result >= 0, f"positive control blocked (errno {error}); cannot validate isolation here"
        finally:
            control.kill()
            control.wait()
        shim = subprocess.Popen(
            [sys.argv[1], "--control-fd", str(fd)], pass_fds=(fd,),
            env={**os.environ, "SILLI_PROC_DIR": directory}, stderr=subprocess.PIPE,
        )
        try:
            ready = shim.stderr.readline()
            assert b"ready" in ready, ready
            result, error = duplicate(shim.pid, fd)
            assert result == -1 and error == errno.EPERM, (result, error)
            with socket.socket(socket.AF_UNIX) as client:
                client.connect(directory + "/control")
                client.sendall(b'{"role":"control"}\n{"op":"status"}\n')
                assert b'"ok":true' in client.recv(4096), "normal daemon control must still work"
            print("PASS: positive control duplicated; protected shim denied; control protocol works")
        finally:
            shim.kill()
            shim.wait()
