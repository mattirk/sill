#!/usr/bin/env python3
"""Runs site/install.sh for SILL_VERSION on this machine, answering its questions on a pseudo-terminal
(the script asks on /dev/tty), then checks that what it installed is there. For CI runners."""
import os
from pathlib import Path
import platform
import pty
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
mac = platform.system() == "Darwin"
# Mac: install the app. Linux: the Debian package, without starting the window with a desktop session.
# Each answer is typed when its question is on the screen: typed ahead, the second one is swallowed
# by sudo, which relays the terminal's input to apt-get.
answers = [(b"Choose [", b"1\n")] if mac else [(b"Choose [", b"1\n"), (b"[Y/n] ", b"n\n")]
pid, fd = pty.fork()
if pid == 0:
    os.execvp("sh", ["sh", str(root / "site/install.sh")])
seen = b""
while True:
    try:
        chunk = os.read(fd, 4096)
    except OSError:
        break
    if not chunk:
        break
    sys.stdout.buffer.write(chunk)
    sys.stdout.buffer.flush()
    seen += chunk
    if answers and answers[0][0] in seen:
        os.write(fd, answers.pop(0)[1])
        seen = b""
status = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])
if status != 0:
    raise SystemExit(f"install.sh exited with {status}")
if mac:
    subprocess.run(["codesign", "--verify", "--deep", "/Applications/Sill.app"], check=True)
else:
    subprocess.run(["sill", "--version"], check=True)
