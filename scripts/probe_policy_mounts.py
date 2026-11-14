#!/usr/bin/env python3
"""Check the travelling-policy fixture from inside a freshly built Sill jail.

Pin tests/fixtures/travelling-policy itself before building that jail. Probe writes are
unique, synthetic files removed immediately; this never writes an existing fixture file.
"""
import errno
import os
from pathlib import Path
import stat
import tempfile


def file_is_masked(path):
    """A /dev/null bind may return empty bytes or deny reads on a nodev mount."""
    info = path.lstat()
    if not (stat.S_ISCHR(info.st_mode)
            and info.st_rdev == Path("/dev/null").stat().st_rdev
            and path.is_mount()):
        return False
    try:
        return path.read_bytes() == b""
    except OSError as error:
        if error.errno != errno.EACCES:
            raise
        return True


def main():
    if not os.environ.get("SILL_SESSION"):
        raise SystemExit("Run inside a Sill jail with the fixture folder pinned.")
    root = Path(__file__).resolve().parent.parent / "tests/fixtures/travelling-policy"
    failures = []

    def check(condition, message):
        print(f"{'PASS' if condition else 'FAIL'}: {message}")
        if not condition:
            failures.append(message)

    check("must stay readable" in (root / "readonly/visible.txt").read_text(),
          "read-only file stays readable")
    try:
        with tempfile.NamedTemporaryFile(prefix=".sill-policy-probe-", dir=root / "readonly"):
            pass
    except OSError as error:
        check(error.errno == errno.EROFS, "write to read-only directory fails with EROFS")
    else:
        check(False, "write to read-only directory must be refused")

    with tempfile.NamedTemporaryFile(prefix=".sill-policy-probe-", dir=root / "writable") as file:
        file.write(b"synthetic probe\n")
        file.flush()
        check(Path(file.name).read_bytes() == b"synthetic probe\n",
              "unrestricted directory remains writable")

    for view, path in [("jail", root), ("host", Path("/.sill/host") / root.relative_to("/"))]:
        try:
            fd = os.open(path / "readonly/visible.txt", os.O_WRONLY)
        except OSError as error:
            check(error.errno == errno.EROFS,
                  f"existing read-only file refuses writable open in {view} view with EROFS")
        else:
            os.close(fd)
            check(False, f"existing read-only file must refuse writable open in {view} view")

        try:
            check(file_is_masked(path / "hidden.txt"), f"synthetic file masked in {view} view")
        except OSError as error:
            check(False, f"synthetic file masked in {view} view ({error})")
    if failures:
        raise SystemExit(f"{len(failures)} check(s) failed. Confirm the fixture root was pinned before this jail was built.")
    print("All mount probes passed; no probe files left behind.")


if __name__ == "__main__":
    main()
