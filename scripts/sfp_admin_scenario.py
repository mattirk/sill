#!/usr/bin/env python3
"""Independent public administration/schema checks in an isolated daemon fixture."""
import base64
import sys
from sfp_client import Client, RpcError

with_client = Client(sys.argv[1])
try:
    c = with_client
    # Traverse every configured definition using public schemas.
    after, agents = None, []
    while True:
        page = c.call("_silli/agents.list", {"after": after, "limit": 1})
        agents.extend(page["agents"])
        after = page["next"]
        if after is None:
            break
    assert agents and [a["id"] for a in agents] == sorted(set(a["id"] for a in agents))
    agent = c.call("_silli/agents.get", {"agent_id": agents[0]["id"]})
    custom = dict(agent, id="sfp-conformance", name="Conformance fixture", command=["/bin/false"])
    c.call("_silli/agents.save", custom)
    assert c.call("_silli/agents.get", {"agent_id": custom["id"]}) == custom
    c.call("_silli/agents.defaults.set", {"agent_id": custom["id"], "settings": {"mode": None, "config": {}}})
    assert c.call("_silli/agents.remove", {"agent_id": custom["id"]})["removed"]
    assert not c.call("_silli/agents.remove", {"agent_id": custom["id"]})["removed"]
    try:
        c.call("_silli/agents.get", {"agent_id": "../escape"})
        raise AssertionError("unsafe agent identity accepted")
    except RpcError:
        pass
    # Error details: required shapes are checked by the client against the exported schema.
    try:
        c.call("_silli/sessions.list", {"limit": 129}, check=False)
        raise AssertionError("oversized page accepted")
    except RpcError as e:
        assert e.code == "limit_exceeded", e
        assert e.error["data"]["details"] == {"scope": "request", "maximum": 128}, e.error
    try:
        c.call("_silli/internal/sessions.list", {}, check=False)
        raise AssertionError("removed internal method still exists")
    except RpcError as e:
        assert e.error["code"] == -32601, e.error
    # the internal capability is gone; the public ones are named features (send_now, permission modes)
    offered = {cap["name"] for cap in c.schema["capabilities"]["offered"]}
    assert "silli.internal" not in offered and "silli.permission_modes" in offered, offered
    assert all(not m["name"].startswith("_silli/internal/") for m in c.schema["methods"])
    home = c.call("_silli/files.home", {})
    raw = base64.b64decode(home["bytes_base64"]) + b"/conformance-\xff"
    path = {"bytes_base64": base64.b64encode(raw).decode(), "display": "deliberately not the operational path"}
    c.call("_silli/files.write", {"path": path, "bytes_base64": base64.b64encode(b"\x00\xffhello").decode()})
    data = c.call("_silli/files.read", {"path": path, "offset": "1", "length": 3})
    assert base64.b64decode(data["bytes_base64"]) == b"\xffhe" and data["next_offset"] == "4"
    assert c.call("_silli/files.metadata", {"path": path})["length"] == "7"
    c.call("_silli/settings.instructions.set", {"text": "Conformance instructions"})
    assert c.call("_silli/settings.instructions.get", {})["text"] == "Conformance instructions"
    policies = c.call("_silli/policy.list", {"limit": 1})
    assert len(policies["names"]) <= 1
    session = c.call("_silli/sessions.create", {"create_id": "memory-spool", "agent": "claude", "policy": "default"})["session_id"]
    # Policy administration uses transferred text: nothing is read from a path the client names.
    assert c.call("_silli/policy.check", {"text": "name='shared'"})["valid"]
    assert not c.call("_silli/policy.check", {"text": "name='bad'\nunknown=true"})["valid"]
    installed = c.call("_silli/policy.import", {"name": "shared", "text": "name='shared'\n[network]\nmode='none'"})
    assert c.call("_silli/policy.get", {"name": "shared"}) == installed
    try:
        c.call("_silli/policy.import", {"name": "shared", "text": "name='shared'"})
        raise AssertionError("policy replaced without an explicit flag")
    except RpcError as error:
        assert error.code == "conflict", error
    replaced = c.call("_silli/policy.import", {"name": "shared", "text": "name='shared'", "replace": True})
    assert replaced != installed
    preview = c.call("_silli/policy.preview", {"session_id": session})
    assert preview["view"] == "preview" and not preview["diagnostics"]
    assert preview["text"] and preview["fingerprint"]
    assert c.call("_silli/policy.snapshot", {"session_id": session})["snapshot"] is None
    try:
        c.call("_silli/policy.preview", {"session_id": "missing"})
        raise AssertionError("unknown session previewed")
    except RpcError as error:
        assert error.code == "not_found", error
    c.call("_silli/memory.notes.add", {"session_id": session, "text": "immutable original note"})
    first = c.call("_silli/memory.text", {"session_id": session, "format": "show", "length": 8})
    c.call("_silli/memory.notes.add", {"session_id": session, "text": "later mutation"})
    request = {"session_id": session, "format": "show", "snapshot_id": first["snapshot_id"], "fingerprint": first["fingerprint"], "offset": first["next"]}
    foreign = Client(sys.argv[1])
    try:
        try:
            foreign.call("_silli/memory.text", request)
            raise AssertionError("foreign connection consumed memory snapshot")
        except RpcError as error:
            assert error.code == "snapshot_expired"
    finally:
        foreign.close()
    text = first["text"]
    while request["offset"] is not None:
        page = c.call("_silli/memory.text", request)
        text += page["text"]
        request["offset"] = page["next"]
    assert "immutable original note" in text and "later mutation" not in text
    c.call("_silli/memory.text.close", {"snapshot_id": first["snapshot_id"]})
    try:
        c.call("_silli/memory.text", dict(request, offset="0"))
        raise AssertionError("closed memory snapshot remained available")
    except RpcError as error:
        assert error.code == "snapshot_expired"
    c.call("_silli/policy.denied", {"session_id": session, "limit": 1})
finally:
    with_client.close()
