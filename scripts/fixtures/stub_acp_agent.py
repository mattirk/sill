#!/usr/bin/env python3
"""A minimal ACP agent for host-side jail tests: answers the handshake and ends every turn at
once. It makes no network requests and needs no credentials; tests drive the jail through
session terminals instead of the agent."""
import json
import sys


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    try:
        request = json.loads(line)
    except ValueError:
        continue
    if "id" not in request or "method" not in request:
        continue
    method, ident = request["method"], request["id"]
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}, "authMethods": []}
    elif method == "session/new":
        result = {"sessionId": "stub-session"}
    elif method == "session/load":
        result = {}
    elif method == "session/prompt":
        session = request.get("params", {}).get("sessionId", "stub-session")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {
            "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "stub"}}}})
        result = {"stopReason": "end_turn"}
    elif method == "session/cancel":
        continue
    else:
        send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32601, "message": "not supported"}})
        continue
    send({"jsonrpc": "2.0", "id": ident, "result": result})
