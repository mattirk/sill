#!/usr/bin/env python3
"""The version a release of this commit would have; rejects anything but main, a stale lockfile and
a version with no section in CHANGELOG.md. With --notes, prints that section: the release's notes."""
import os
import sys
from pathlib import Path
import tomllib

root = Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def notes(version):
    """The body of the `## VERSION …` section of CHANGELOG.md, or None."""
    changelog = root / "CHANGELOG.md"
    lines = changelog.read_text().splitlines() if changelog.exists() else []
    body = None
    for line in lines:
        if line.startswith("## "):
            if body is not None:
                break
            words = line[3:].split()
            if words and words[0] == version:
                body = []
        elif body is not None:
            body.append(line)
    if body is None:
        return None
    text = "\n".join(body).strip()
    return text or None


if sys.argv[1:] == ["--notes"]:
    found = notes(version)
    if found is None:
        raise SystemExit(f"CHANGELOG.md has no section for {version}.")
    print(found)
    raise SystemExit(0)
if os.environ.get("GITHUB_REF", "") != "refs/heads/main":
    raise SystemExit("Releases are built from main.")
lock = tomllib.loads((root / "Cargo.lock").read_text())
packages = {p["name"] for p in (tomllib.loads(f.read_text())["package"] for f in (root / "crates").glob("*/Cargo.toml"))}
for package in lock["package"]:
    if package["name"] in packages and "source" not in package and package["version"] != version:
        raise SystemExit(f"Cargo.lock has stale version for {package['name']}; update the lockfile.")
if notes(version) is None:
    raise SystemExit(f"CHANGELOG.md has no section for {version}; add one before releasing.")
if os.environ.get("GITHUB_OUTPUT"):
    with open(os.environ["GITHUB_OUTPUT"], "a") as out:
        out.write(f"version={version}\n")
print(f"Release v{version}")
