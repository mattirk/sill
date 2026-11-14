#!/usr/bin/env python3
# A minimal ACP agent over stdio: answers initialize / session/new / prompt, and during the prompt
# sends an agent_message_chunk, a usage_update and a made-up update kind, then stops.
import sys, json
def send(obj): sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
def notify(sid, update): send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"update":update}})
for line in sys.stdin:
    m = json.loads(line); mid = m.get("id"); meth = m.get("method"); p = m.get("params", {})
    if meth == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":False,"promptCapabilities":{"image":False}},"authMethods":[]}})
    elif meth == "session/new":
        send({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"fake-1","modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"}]},"configOptions":[]}})
    elif meth == "session/prompt":
        sid = p["sessionId"]
        notify(sid, {"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello "}})
        notify(sid, {"sessionUpdate":"usage_update","used":12345,"size":200000})
        # as claude-agent-acp sends it: a cost, and a cost missing its amount (total_cost_usd unset)
        notify(sid, {"sessionUpdate":"usage_update","used":23456,"size":200000,"cost":{"amount":0.0123,"currency":"USD"},"_meta":{"_claude/origin":"x"}})
        notify(sid, {"sessionUpdate":"usage_update","used":34567,"size":200000,"cost":{"currency":"USD"}})
        notify(sid, {"sessionUpdate":"something_new","foo":1})
        notify(sid, {"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}})
        send({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}})
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
