#!/usr/bin/env python3
"""Reject a release tag that differs from the workspace/package version."""
import os
from pathlib import Path
import re
import tomllib

root = Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
ref = os.environ.get("GITHUB_REF", "")
if not re.fullmatch(r"refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", ref):
    raise SystemExit("Select or push a version tag such as v0.1.0 to run a release.")
if ref != f"refs/tags/v{version}":
    raise SystemExit(f"Tag {ref} does not match workspace version {version}.")
lock = tomllib.loads((root / "Cargo.lock").read_text())
packages = {p["name"] for p in (tomllib.loads(f.read_text())["package"] for f in (root / "crates").glob("*/Cargo.toml"))}
for package in lock["package"]:
    if package["name"] in packages and "source" not in package and package["version"] != version:
        raise SystemExit(f"Cargo.lock has stale version for {package['name']}; update the lockfile.")
print(f"Release v{version}")
