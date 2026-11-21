#!/usr/bin/env python3
"""Independent Changes client: exact daemon paths, bounded pages, checked diffs and mutations."""
import base64
import os
import sys
from sfp_client import Client, RpcError


def path(raw):
    return {"bytes_base64": base64.b64encode(raw).decode(), "display": "presentation-only"}


with_client = Client(sys.argv[1])
try:
    client = with_client
    repo = os.fsencode(sys.argv[2])
    target = repo + b"/file-\xff\nname"
    client.call("_silli/pins.add", {"path": path(repo)})
    after = None
    found = []
    while True:
        page = client.call("_silli/changes.list", {"after": after, "limit": 1})
        found.extend(page["entries"])
        after = page["next"]
        if after is None:
            break
    assert len(found) == 1 and found[0]["type"] == "live"
    assert base64.b64decode(found[0]["path"]["bytes_base64"]) == target
    params = {"target": {"type": "live", "repo": path(repo), "path": path(target)}}
    diff = client.call("_silli/changes.diff", params)
    assert "fixture contents" in diff["text"] and diff["next_offset"] is None
    try:
        client.call("_silli/changes.diff", dict(params, offset="1", fingerprint="stale"))
        raise AssertionError("stale diff accepted")
    except RpcError as error:
        assert error.code == "invalid_cursor"
    history = client.call("_silli/changes.history", {"limit": 1})
    assert history["events"] == []
    client.call("_silli/changes.git-revert", {"repo": path(repo), "path": path(target)})
    assert client.call("_silli/changes.list", {})["entries"] == []
    assert client.changes_dirty
finally:
    with_client.close()
