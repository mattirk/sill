#!/usr/bin/env python3
"""Independent SFP validation client. No sill-core imports or database access.

Install scripts/requirements-sfp.txt in a virtual environment. The daemon's exported
JSON Schema validates every public request, result and notification. --schema can
instead read the JSON printed by `sill daemon schema`. Cursor files include the
entire reconstructed view, because a cursor alone is insufficient for replay.
"""
import argparse
import base64
import copy
import json
import os
from pathlib import Path
import re
import socket
import sys
import uuid

PROTOCOL = "sfp/1-dev.1"
MAX_FRAME = 8 * 1024 * 1024
MAX_PENDING = 4096
IDENTITY = {"session": "session_id", "item": "item_id", "turn": "turn_id",
            "submission": "submission_id", "question": "question_id"}


class RpcError(Exception):
    def __init__(self, error):
        self.error = error
        self.code = error.get("data", {}).get("code")
        super().__init__(f"{self.code or error['code']}: {error['message']}")


def sequence(text):
    if not isinstance(text, str) or not re.fullmatch(r"0|[1-9][0-9]*", text):
        raise ValueError("noncanonical event sequence")
    return int(text)


class View:
    def __init__(self, session_id, seq="0", records=()):
        self.session_id = session_id
        self.seq = sequence(seq)
        self.records = {}
        for record in records:
            self.put(record, new=True)

    def put(self, record, new=False):
        kind = record["type"]
        if kind not in IDENTITY:
            raise ValueError(f"unknown state record {kind}")
        key = (kind, record[kind][IDENTITY[kind]])
        if new and key in self.records:
            raise ValueError(f"duplicate record {key}")
        self.records[key] = copy.deepcopy(record)

    def apply(self, notification):
        if notification["session_id"] != self.session_id:
            raise ValueError("event belongs to another session")
        seq = sequence(notification["seq"])
        if seq <= self.seq:
            return False
        if seq != self.seq + 1:
            raise ValueError("event gap; reattach before applying further events")
        event = notification["event"]
        kind = event["type"]
        if kind == "item.created":
            self.put({"type": "item", "item": event["item"]}, new=True)
        elif kind in ("session.updated", "turn.updated", "submission.updated", "question.updated"):
            record_type = kind.split(".")[0]
            self.put({"type": record_type, record_type: event[record_type]})
        elif kind in ("item.patched", "item.text_appended", "item.removed"):
            key = ("item", event["item_id"])
            if key not in self.records:
                raise ValueError("missing item target; obtain a replacement snapshot")
            item = copy.deepcopy(self.records[key]["item"])
            if kind == "item.removed":
                del self.records[key]
            elif kind == "item.patched":
                for name, value in event["set"].items():
                    if name not in ("content", "tool") or value is None:
                        raise ValueError(f"invalid patch field {name}")
                    item[name] = copy.deepcopy(value)
                self.records[key] = {"type": "item", "item": item}
            else:
                blocks = [b for b in item["content"] if b["block_id"] == event["block_id"]]
                if len(blocks) != 1 or blocks[0]["type"] not in ("text", "thought") or not isinstance(blocks[0]["text"], str):
                    raise ValueError("append target is not a known text block")
                blocks[0]["text"] += event["text"]
                self.records[key] = {"type": "item", "item": item}
        else:
            raise ValueError(f"unknown required state event {kind}; client upgrade needed")
        self.seq = seq
        return True

    def session(self):
        record = self.records.get(("session", self.session_id))
        return record["session"] if record else None

    def commands(self):
        """The slash commands the agent offers now (`/name`, its description and input hint)."""
        session = self.session()
        return [] if session is None else session.get("commands", [])

    def items(self, kind=None):
        found = [r["item"] for (k, _), r in self.records.items() if k == "item" and (kind is None or r["item"]["kind"] == kind)]
        return sorted(found, key=lambda item: sequence(item["created_seq"]))

    def plan(self):
        """The latest plan's steps as (status, priority, text); a plan revises itself in place."""
        plans = self.items("plan")
        if not plans:
            return []
        return [(b.get("status"), b.get("priority"), b["text"]) for b in plans[-1]["content"] if b["type"] == "plan_entry"]

    def attachments(self, item):
        """A user item's attachments, from its submission. Bytes are fetched with uploads.get;
        only records journaled before attachments became uploads still embed them."""
        record = self.records.get(("submission", item.get("submission_id")))
        return [] if record is None else record["submission"]["attachments"]

    def document(self):
        return {"session_id": self.session_id, "seq": str(self.seq), "records": list(self.records.values())}

    def save(self, path):
        if path:
            path = Path(path)
            temporary = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
            descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            try:
                with os.fdopen(descriptor, "w") as output:
                    json.dump(self.document(), output, ensure_ascii=False)
                    output.flush()
                    os.fsync(output.fileno())
                os.replace(temporary, path)
            finally:
                temporary.unlink(missing_ok=True)


class Client:
    def __init__(self, path, schema=None):
        try:
            from jsonschema import Draft202012Validator
        except ImportError as error:
            raise RuntimeError("install scripts/requirements-sfp.txt in your Python environment") from error
        self.validator = Draft202012Validator
        self.schema = schema
        self.pending = []
        self.next_id = 1
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(120)
        self.sock.connect(str(path))
        self.input = self.sock.makefile("rb")
        offer = {"protocols": [PROTOCOL], "client": {"name": "sfp-python", "version": "0.1"}, "capabilities": {}}
        response = self.call("_sill/initialize", offer)
        if response["protocol"] != PROTOCOL:
            raise ValueError("daemon selected an unoffered protocol")
        self.max_frame = min(MAX_FRAME, response["limits"]["max_message_bytes"])
        if self.schema is None:
            self.schema = self.call("_sill/daemon.schema", {})
        self.validate("_sill/initialize", "params", offer)
        self.validate("_sill/initialize", "result", response)

    def close(self):
        self.input.close()
        self.sock.close()

    def validate(self, method, member, value):
        if self.schema is None:
            return
        entries = [m for m in self.schema["methods"] if m["name"] == method]
        if len(entries) != 1 or entries[0].get("x-sill-internal"):
            raise ValueError(f"{method} is not a public method in the exported schema")
        shape = entries[0][member]
        if shape is None:
            shape = {"type": "object", "maxProperties": 0}
        schema = {"$schema": "https://json-schema.org/draft/2020-12/schema", "$defs": self.schema["$defs"], **shape}
        self.validator(schema).validate(value)

    def read(self):
        line = self.input.readline(getattr(self, "max_frame", MAX_FRAME) + 1)
        if not line:
            raise EOFError("daemon disconnected; mutation outcome may be unknown")
        if len(line) > getattr(self, "max_frame", MAX_FRAME) or not line.endswith(b"\n"):
            raise ValueError("oversized or incomplete SFP frame")
        message = json.loads(line)
        if message.get("jsonrpc") != "2.0":
            raise ValueError("invalid JSON-RPC version")
        return message

    def call(self, method, params):
        self.validate(method, "params", params)
        request_id = self.next_id
        self.next_id += 1
        frame = (json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n").encode()
        if len(frame) > getattr(self, "max_frame", MAX_FRAME):
            raise ValueError("request exceeds max_message_bytes")
        self.sock.sendall(frame)
        while True:
            message = self.read()
            if "method" in message and "id" not in message:
                if len(self.pending) >= MAX_PENDING:
                    raise ValueError("snapshot tail overflow; abandon sync and reattach")
                self.pending.append(message)
                continue
            if message.get("id") != request_id:
                raise ValueError("unexpected response ID")
            if "error" in message:
                raise RpcError(message["error"])
            self.validate(method, "result", message["result"])
            return message["result"]

    def event(self):
        message = self.pending.pop(0) if self.pending else self.read()
        if message.get("method") != "_sill/session.event":
            raise ValueError(f"unexpected notification {message.get('method')}")
        self.validate(message["method"], "params", message["params"])
        return message["params"]

    def attach(self, session_id, view=None):
        params = {"session_id": session_id}
        if view is not None:
            if view.session_id != session_id:
                raise ValueError("saved view belongs to another session")
            params["since"] = {"session_id": session_id, "seq": str(view.seq)}
        attached = self.call("_sill/sessions.attach", params)
        subscription = attached["subscription_id"]
        if attached["mode"] == "snapshot":
            lease = attached["snapshot"]
            records = []
            page = None
            while True:
                response = self.call("_sill/sessions.snapshot", {"token": lease["token"], "page": page})
                if response["at"] != lease["at"] or response["at"]["session_id"] != session_id:
                    raise ValueError("snapshot pages have different boundaries")
                records.extend(response["records"])
                page = response["next_page"]
                if page is None:
                    break
            view = View(session_id, lease["at"]["seq"], records)
        elif attached["mode"] == "replay":
            if view is None or attached["after"] != params["since"]:
                raise ValueError("invalid replay origin")
            through = attached["through"]
            if through["session_id"] != session_id or sequence(through["seq"]) < view.seq:
                raise ValueError("invalid replay boundary")
            while view.seq < sequence(through["seq"]):
                event = self.event()
                if event["subscription_id"] == subscription:
                    view.apply(event)
        else:
            raise ValueError("unknown attach mode")
        return subscription, view


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", default=str(Path(os.environ.get("XDG_RUNTIME_DIR", "/run/user/" + str(os.getuid()))) / "sill/daemon.sock"))
    parser.add_argument("--schema", type=Path, help="JSON exported by sill daemon schema (otherwise fetched)")
    commands = parser.add_subparsers(dest="command", required=True)
    watch = commands.add_parser("watch")
    watch.add_argument("session_id")
    watch.add_argument("--state", type=Path, help="persist the complete view and its cursor for reconnect")
    watch.add_argument("--once", action="store_true", help="synchronize and print the current view, then exit")
    submit = commands.add_parser("submit")
    submit.add_argument("session_id")
    submit.add_argument("text")
    submit.add_argument("--submission-id", required=True, help="reuse this ID and identical input after an uncertain reply")
    submit.add_argument("--retry", action="store_true", help="repeat the identical accepted submission to exercise deduplication")
    submit.add_argument("--image", type=Path, action="append", default=[], help="attach an image inline (at most 1 MiB in total)")
    upload = commands.add_parser("upload", help="write an upload's bytes (an attachment the journal references) to a file")
    upload.add_argument("session_id")
    upload.add_argument("upload_id")
    upload.add_argument("--out", type=Path, required=True)
    for name in ("status", "cancel", "resume"):
        action = commands.add_parser(name)
        action.add_argument("session_id")
        action.add_argument("submission_id")
    answer = commands.add_parser("answer")
    answer.add_argument("session_id")
    answer.add_argument("question_id")
    answer.add_argument("--answer-id", required=True)
    choice = answer.add_mutually_exclusive_group(required=True)
    choice.add_argument("--option")
    choice.add_argument("--form-json", type=json.loads)
    choice.add_argument("--cancel", action="store_true")
    args = parser.parse_args()
    schema = json.loads(args.schema.read_text()) if args.schema else None
    client = Client(args.socket, schema)
    try:
        if args.command == "watch":
            view = View(**json.loads(args.state.read_text())) if args.state and args.state.exists() else None
            subscription, view = client.attach(args.session_id, view)
            view.save(args.state)
            print(json.dumps(view.document(), ensure_ascii=False), flush=True)
            while not args.once:
                event = client.event()
                if event["subscription_id"] == subscription and view.apply(event):
                    view.save(args.state)
                    print(json.dumps(event, ensure_ascii=False), flush=True)
            client.call("_sill/sessions.detach", {"session_id": args.session_id, "subscription_id": subscription})
        elif args.command == "submit":
            # Attach starts the agent if necessary; the mutation itself never creates a session.
            client.attach(args.session_id)
            images = [{"type": "inline", "name": path.name, "mime": "image/" + (path.suffix.lstrip(".").lower().replace("jpg", "jpeg") or "png"),
                       "data_base64": base64.b64encode(path.read_bytes()).decode()} for path in args.image]
            params = {"session_id": args.session_id, "submission_id": args.submission_id, "text": args.text, "attachments": images}
            result = client.call("_sill/sessions.submit", params)
            if args.retry:
                retry = client.call("_sill/sessions.submit", params)
                if (retry["submission_id"], retry["accepted_seq"]) != (result["submission_id"], result["accepted_seq"]):
                    raise ValueError("retry created another submission")
                result = retry
            if any(a["type"] == "inline" for a in result["attachments"]):
                raise ValueError("the accepted submission embeds attachment bytes instead of referencing an upload")
            print(json.dumps(result, ensure_ascii=False))
        elif args.command == "upload":
            result = client.call("_sill/uploads.get", {"session_id": args.session_id, "upload_id": args.upload_id})
            args.out.write_bytes(base64.b64decode(result["data_base64"], validate=True))
            print(json.dumps({k: result[k] for k in ("upload_id", "name", "mime")}, ensure_ascii=False))
        elif args.command == "answer":
            answer = {"type": "cancel"} if args.cancel else ({"type": "permission", "option_id": args.option} if args.option else {"type": "form", "values": args.form_json})
            result = client.call("_sill/questions.answer", {"session_id": args.session_id, "question_id": args.question_id, "answer_id": args.answer_id, "answer": answer})
            print(json.dumps(result, ensure_ascii=False))
        else:
            method = "get" if args.command == "status" else args.command
            print(json.dumps(client.call("_sill/submissions." + method, {"session_id": args.session_id, "submission_id": args.submission_id}), ensure_ascii=False))
    finally:
        client.close()


if __name__ == "__main__":
    try:
        main()
    except (RpcError, ValueError, RuntimeError, OSError, EOFError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
