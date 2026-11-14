#!/usr/bin/env python3
"""Phases driven by the Rust scripted-agent recovery fixture; public SFP only."""
import json
import sys
from pathlib import Path
from sfp_client import Client, View

socket, phase, session_id, state_path = sys.argv[1:]
client = Client(socket)
try:
    if phase == "create":
        request = {"create_id": "python-lifecycle", "agent": "claude", "policy": "default"}
        result = client.call("_sill/sessions.create", request)
        assert client.call("_sill/sessions.create", request) == result
        print(json.dumps(result))
    elif phase == "submit":
        assert client.call("_sill/sessions.get", {"session_id": session_id})["session_id"] == session_id
        client.call("_sill/sessions.update", {"session_id": session_id, "change": {"type": "title", "text": "Python lifecycle", "kind": "person"}})
        assert client.call("_sill/sessions.start", {"session_id": session_id})["fresh"] is False
        subscription, view = client.attach(session_id)
        result = client.call("_sill/sessions.submit", {"session_id": session_id, "submission_id": "python-turn", "text": "work"})
        assert result["submission_id"] == "python-turn"
        # Reattach captures the accepted submission and gives the restart a durable cursor.
        subscription, view = client.attach(session_id)
        view.save(Path(state_path))
    elif phase == "resume":
        saved = View(**json.loads(Path(state_path).read_text()))
        subscription, view = client.attach(session_id, saved)
        questions = [r["question"] for r in view.records.values() if r["type"] == "question" and r["question"]["state"] == "open"]
        assert len(questions) == 1
        request = {"session_id": session_id, "question_id": questions[0]["question_id"], "answer_id": "python-answer", "answer": {"type": "permission", "option_id": "allow"}}
        answer = client.call("_sill/questions.answer", request)
        assert client.call("_sill/questions.answer", request)["answer_id"] == answer["answer_id"]
    elif phase == "end":
        assert client.call("_sill/submissions.get", {"session_id": session_id, "submission_id": "python-turn"})["outcome"] == "completed"
        result = client.call("_sill/sessions.end", {"session_id": session_id, "reason": "close"})
        assert result["pending"]
    elif phase == "ended":
        session = client.call("_sill/sessions.get", {"session_id": session_id})
        assert session["ended_at"] is not None
        assert session["summary"]
        assert any(s["session_id"] == session_id for s in client.call("_sill/sessions.list", {"limit": 1})["sessions"])
    else:
        raise ValueError(phase)
finally:
    client.close()
