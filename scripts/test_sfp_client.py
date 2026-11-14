"""Run with python3 -m unittest discover -s scripts -p 'test_sfp_client.py'."""
import unittest
from sfp_client import Client, View, sequence


def item():
    return {"item_id": "i", "content": [{"block_id": "b", "type": "text", "text": "hello"}]}


def event(seq, payload):
    return {"session_id": "s", "subscription_id": "sub", "seq": str(seq), "event": payload}


class ViewTests(unittest.TestCase):
    def test_duplicate_delivery_does_not_append_twice_and_gap_leaves_state_unchanged(self):
        view = View("s")
        view.apply(event(1, {"type": "item.created", "item": item()}))
        append = event(2, {"type": "item.text_appended", "item_id": "i", "block_id": "b", "text": " world"})
        self.assertTrue(view.apply(append))
        self.assertFalse(view.apply(append))
        before = view.document()
        with self.assertRaises(ValueError):
            view.apply(event(4, {"type": "item.removed", "item_id": "i"}))
        self.assertEqual(view.document(), before)
        self.assertEqual(view.records[("item", "i")]["item"]["content"][0]["text"], "hello world")

    def test_unknown_state_event_and_missing_target_require_resynchronization(self):
        view = View("s")
        for payload in ({"type": "future.state"}, {"type": "item.patched", "item_id": "missing", "set": {}}):
            with self.assertRaises(ValueError):
                view.apply(event(1, payload))
            self.assertEqual(view.seq, 0)

    def test_cursor_keeps_the_complete_view_and_large_sequences_are_exact(self):
        seq = str(2**64 - 2)
        view = View("s", seq, [{"type": "item", "item": item()}])
        loaded = View(**view.document())
        loaded.apply(event(2**64 - 1, {"type": "item.text_appended", "item_id": "i", "block_id": "b", "text": "!"}))
        self.assertEqual(loaded.seq, 2**64 - 1)
        for invalid in ("01", "-1", "1.0", 1):
            with self.assertRaises(ValueError):
                sequence(invalid)

    def test_snapshot_pages_replace_old_view_before_buffered_events(self):
        class SnapshotClient(Client):
            def __init__(self):
                self.page = 0

            def call(self, method, params):
                if method == "_sill/sessions.attach":
                    return {"mode": "snapshot", "subscription_id": "sub", "snapshot": {"token": "t", "at": {"session_id": "s", "seq": "10"}}}
                self.page += 1
                return {"at": {"session_id": "s", "seq": "10"}, "records": [{"type": "item", "item": item()}] if self.page == 1 else [], "next_page": "next" if self.page == 1 else None}
        old = View("s", "4", [{"type": "question", "question": {"question_id": "obsolete"}}])
        sub, replacement = SnapshotClient().attach("s", old)
        self.assertEqual(sub, "sub")
        self.assertNotIn(("question", "obsolete"), replacement.records)
        replacement.apply(event(11, {"type": "item.text_appended", "item_id": "i", "block_id": "b", "text": "!"}))
        self.assertEqual(replacement.records[("item", "i")]["item"]["content"][0]["text"], "hello!")


class PresentationTests(unittest.TestCase):
    def test_plan_revises_in_place_and_commands_come_from_the_session(self):
        step = lambda text, status: {"block_id": "entry-" + text, "type": "plan_entry", "text": text, "status": status, "priority": "high"}
        plan = {"item_id": "p", "created_seq": "1", "kind": "plan", "content": [step("read", "in_progress"), step("fix", "pending")]}
        view = View("s")
        view.apply(event(1, {"type": "item.created", "item": plan}))
        view.apply(event(2, {"type": "item.patched", "item_id": "p", "set": {"content": [step("read", "completed"), step("fix", "in_progress")]}}))
        self.assertEqual(view.plan(), [("completed", "high", "read"), ("in_progress", "high", "fix")])
        view.apply(event(3, {"type": "session.updated", "session": {"session_id": "s", "commands": [{"name": "review", "description": "Review", "hint": None}]}}))
        self.assertEqual(view.commands()[0]["name"], "review")

    def test_user_attachments_reference_uploads(self):
        submission = {"submission_id": "x", "attachments": [{"type": "uploaded", "upload_id": "inline-0-x", "name": "shot.png", "mime": "image/png"}]}
        view = View("s", "2", [{"type": "submission", "submission": submission},
                               {"type": "item", "item": {"item_id": "u", "created_seq": "2", "kind": "user", "submission_id": "x", "content": []}}])
        self.assertEqual(view.attachments(view.items("user")[0])[0]["upload_id"], "inline-0-x")


if __name__ == "__main__":
    unittest.main()
