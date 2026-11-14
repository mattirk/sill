#!/usr/bin/env python3
"""Driven by the daemon test's fake login/installer; schemas and public SFP only."""
import sys
from sfp_client import Client, RpcError

client = Client(sys.argv[1])
try:
    after = None
    ids = []
    while True:
        page = client.call("_sill/registry.list", {"after": after, "limit": 1})
        ids.extend(e["entry_id"] for e in page["entries"])
        after = page["next"]
        if after is None:
            break
    assert ids == sorted(set(ids)) and "fixture" in ids
    params = {"entry_id": "fixture", "version": "1.0.0", "source": "live"}
    try:
        client.call("_sill/registry.install", params)
        raise AssertionError("unreviewed install was allowed")
    except RpcError as error:
        assert error.code == "permission_denied"
    params["allow_unreviewed"] = True
    params["operation_id"] = "install-failed"
    opened = client.call("_sill/registry.install", params)
    progress = list(client.progress(opened["token"]))
    assert any(e == {"type": "output", "text": "fixture-installer-output"} for e in progress)
    assert progress[-1]["type"] == "failed" and progress[-1]["code"] == "operation_failed"
    receipt = client.call("_sill/operations.get", {"operation_id": "install-failed"})
    assert receipt["state"] == "failed"
    duplicate = client.call("_sill/registry.install", params)
    assert duplicate["token"] is None and duplicate["operation"] == receipt
    params["entry_id"] = "fixture-success"
    try:
        client.call("_sill/registry.install", params)
        raise AssertionError("conflicting operation identity was accepted")
    except RpcError as error:
        assert error.code == "id_conflict"
    params["operation_id"] = "install-success"
    opened = client.call("_sill/registry.install", params)
    progress = list(client.progress(opened["token"]))
    assert progress[-1] == {"type": "completed", "agent_id": "fixture-success"}
    assert client.call("_sill/operations.get", {"operation_id": "install-success"})["state"] == "completed"
    login = {"agent_id": "fixture", "method_id": "browser", "operation_id": "login-fixture"}
    opened = client.call("_sill/agents.login", login)
    try:
        client.call("_sill/agents.login", dict(login, operation_id="login-concurrent"))
        raise AssertionError("duplicate login ran concurrently")
    except RpcError as error:
        assert error.code == "busy"
    recovered = client.call("_sill/agents.login", login)
    assert recovered["token"] is None
    progress = list(client.progress(opened["token"]))
    assert progress[-1] == {"type": "completed", "agent_id": "fixture"}
    assert client.call("_sill/operations.get", {"operation_id": "login-fixture"})["state"] == "completed"
    try:
        list(client.progress(opened["token"]))
        raise AssertionError("stream token was reused")
    except EOFError:
        pass
    # Byte preservation and input EOF without losing the response direction.
    opened = client.call("_sill/streams.port", {"session_id": sys.argv[2], "port": 8123})
    with client.open_stream(opened["token"]) as port:
        import socket
        port.sendall(b"\0hello\xff")
        port.shutdown(socket.SHUT_WR)
        received = b""
        while True:
            chunk = port.recv(4096)
            if not chunk:
                break
            received += chunk
        assert received == b"\xffreply\0"
finally:
    client.close()
