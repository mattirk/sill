#!/usr/bin/env python3
"""The version a release of this commit would have; rejects anything but main and a stale lockfile."""
import os
from pathlib import Path
import tomllib

root = Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
if os.environ.get("GITHUB_REF", "") != "refs/heads/main":
    raise SystemExit("Releases are built from main.")
lock = tomllib.loads((root / "Cargo.lock").read_text())
packages = {p["name"] for p in (tomllib.loads(f.read_text())["package"] for f in (root / "crates").glob("*/Cargo.toml"))}
for package in lock["package"]:
    if package["name"] in packages and "source" not in package and package["version"] != version:
        raise SystemExit(f"Cargo.lock has stale version for {package['name']}; update the lockfile.")
if os.environ.get("GITHUB_OUTPUT"):
    with open(os.environ["GITHUB_OUTPUT"], "a") as out:
        out.write(f"version={version}\n")
print(f"Release v{version}")
