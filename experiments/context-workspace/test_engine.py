"""Contract tests, not task-success or model-capability evaluations."""
import json
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from engine import Budget, CapacityError, InvalidTransition, Ref, Workspace, canonical, digest


def user(text):
    return [{"role": "user", "text": text}]


def tool(text, call="c1"):
    return [{"role": "assistant", "text": "run check", "calls": [call]},
            {"role": "tool", "text": text, "call_id": call}]


class Contracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "session.jsonl"
        self.w = Workspace(self.path, "state_workspace")

    def consume(self, w=None, updates=None, boundary=False, keep=()):
        w = w or self.w
        plan = w.prepare()
        w.decide(expected_revision=plan.revision, source_cut=plan.source_cut,
                 consume=plan.unconsumed, updates=updates, boundary=boundary, keep=keep)
        return w.prepare()

    def test_policy_is_immutable(self):
        with self.assertRaises(InvalidTransition):
            Workspace(self.path, "evidence_workspace")
        with self.assertRaises(InvalidTransition):
            Workspace(self.path, "state_workspace", Budget(recent_units=3))

    def test_unknown_policy_does_not_fallback(self):
        with self.assertRaises(ValueError):
            Workspace(self.path, "summary")

    def test_open_batch_and_orphan_are_rejected(self):
        for messages in ([{"role": "assistant", "text": "run", "calls": ["a"]}],
                         [{"role": "tool", "text": "done", "call_id": "a"}],
                         [{"role": "assistant", "text": "", "calls": ["a", "a"]}]):
            with self.assertRaises(InvalidTransition):
                self.w.observe("bad", messages)
        self.assertEqual(self.w.order, [])

    def test_parallel_result_order_is_allowed(self):
        self.w.observe("batch", [{"role": "assistant", "text": "", "calls": ["a", "b"]},
                                 {"role": "tool", "text": "B", "call_id": "b"},
                                 {"role": "tool", "text": "A", "call_id": "a"}])
        self.assertIn("B", self.w.prepare().text)

    def test_unsupported_private_fields_are_not_silently_replayed(self):
        with self.assertRaises(InvalidTransition):
            self.w.observe("x", [{"role": "assistant", "text": "", "reasoning": "opaque"}])

    def test_utf8_boundaries_and_hashes(self):
        self.w.observe("u", user("中文 hello"))
        with self.assertRaises(InvalidTransition):
            self.w.ref("u", start=1, end=3)
        ref = self.w.ref("u", start=0, end=6)
        self.assertEqual(self.w.read(ref), "中文")
        with self.assertRaises(InvalidTransition):
            self.w.read(replace(ref, sha256="wrong"))

    def test_input_objects_cannot_mutate_history(self):
        messages = user("original")
        self.w.observe("u", messages)
        messages[0]["text"] = "changed"
        self.assertEqual(self.w.read(self.w.ref("u")), "original")

    def test_pinned_requirement_survives_consumption_and_release(self):
        self.w.observe("u", user("Do not change the public API"), pinned=True)
        self.consume(boundary=True)
        for i in range(15):
            self.w.observe(f"t{i}", tool("irrelevant output " * 100, f"c{i}"))
            self.consume(boundary=True)
        self.assertIn("Do not change the public API", self.w.prepare().text)

    def test_patch_is_incremental_not_full_rewrite(self):
        self.w.observe("u", user("Investigate initialization"))
        ref = self.w.ref("u").as_dict()
        self.consume(updates={"hypothesis": {"text": "Initialization order may be wrong", "sources": [ref]}}, boundary=True)
        before = canonical(self.w.claims["hypothesis"])
        self.w.observe("t", tool("more evidence"))
        self.consume(updates={"next": {"text": "Read startup", "sources": [ref]}}, boundary=True)
        self.assertEqual(canonical(self.w.claims["hypothesis"]), before)

    def test_oversize_state_patch_is_atomic(self):
        self.w.observe("u", user("task"))
        plan = self.w.prepare()
        count = len(self.w.journal.rows)
        with self.assertRaises(CapacityError):
            self.w.decide(expected_revision=plan.revision, source_cut=plan.source_cut,
                          consume=plan.unconsumed,
                          updates={"huge": {"text": "x" * 5000, "sources": [self.w.ref("u").as_dict()]}})
        self.assertEqual(len(self.w.journal.rows), count)
        self.assertFalse(self.w.consumed)
        self.assertFalse(self.w.claims)

    def test_unseen_middle_cannot_back_model_claim(self):
        self.w.observe("u", user("a" * 3000 + "SECRET" + "b" * 3000))
        plan = self.w.prepare()
        middle = self.w.ref("u", start=3000, end=3006)
        with self.assertRaises(InvalidTransition):
            self.w.decide(expected_revision=plan.revision, source_cut=plan.source_cut,
                          updates={"x": {"text": "SECRET", "sources": [middle.as_dict()]}})
        self.w.observe("recall", tool("read complete"), recall_of=[middle])
        self.consume(updates={"x": {"text": "SECRET", "sources": [middle.as_dict()]}}, boundary=True)
        self.assertEqual(self.w.claims["x"]["text"], "SECRET")

    def test_keep_middle_not_dropped_when_preview_contains_same_unit(self):
        self.w.observe("u", user("a" * 3000 + "CRITICAL" + "b" * 3000))
        middle = self.w.ref("u", start=3000, end=3008)
        self.w.observe("r", tool("read"), recall_of=[middle])
        self.consume(keep=[middle], boundary=True)
        self.w.observe("t", tool("next"))
        self.consume(boundary=True)
        self.assertIn("CRITICAL", self.w.prepare().text)

    def test_new_observation_invalidates_old_decision_cut(self):
        self.w.observe("u", user("old"))
        old = self.w.prepare()
        self.w.observe("u2", user("new requirement"), pinned=True)
        with self.assertRaises(InvalidTransition):
            self.w.decide(expected_revision=old.revision, source_cut=old.source_cut)

    def test_fact_freshness_changes_on_actual_version_update(self):
        self.w.observe("pass", tool("PASS r1"), versions={"file": "r1"},
                       fact={"key": "test", "message": 1, "start": 0, "end": 7, "revisions": {"file": "r1"}})
        self.consume(boundary=True)
        self.assertIn('"freshness":"current"', self.w.prepare().text)
        self.w.observe("edit", tool("edited"), versions={"file": "r2"})
        self.consume(boundary=True)
        self.assertIn('"freshness":"stale"', self.w.prepare().text)
        self.assertIn("PASS r1", self.w.prepare().text)  # historical fact is not rewritten

    def test_host_fact_cannot_come_from_assistant_plan(self):
        with self.assertRaises(InvalidTransition):
            self.w.observe("plan", [{"role": "assistant", "text": "tests will pass"}],
                           fact={"key": "test", "message": 0, "start": 0, "end": 5, "revisions": {}})

    def test_recall_has_no_freshness_or_index_side_effect(self):
        self.w.observe("u", user("ERR_EXACT_7"))
        original = self.w.ref("u")
        for i in range(4):
            self.w.observe(f"r{i}", tool("retrieved", f"c{i}"), recall_of=[original])
        self.assertEqual(len(self.w.search("ERR_EXACT_7")), 1)
        with self.assertRaises(InvalidTransition):
            self.w.observe("bad", tool("read"), recall_of=[original], versions={"file": "new"})
        with self.assertRaises(InvalidTransition):
            self.w.ref("r0")

    def test_frozen_prefix_and_bounded_deltas(self):
        self.w.observe("u", user("task"), pinned=True)
        first = self.w.prepare()
        self.w.decide(expected_revision=first.revision, source_cut=first.source_cut, consume=first.unconsumed)
        second = self.w.prepare()
        self.assertEqual(first.epoch, second.epoch)
        self.assertTrue(second.text.startswith(first.text))

    def test_rolling_views_remain_bounded_without_summarizer(self):
        for policy in ("state_workspace", "evidence_workspace"):
            w = Workspace(Path(self.temp.name) / (policy + ".jsonl"), policy)
            w.observe("goal", user("Preserve the API"), pinned=True)
            self.consume(w)
            for i in range(50):
                w.observe(f"t{i}", tool("payload " * 700, f"c{i}"), object_key="src/config.rs")
                plan = self.consume(w)
                self.assertLessEqual(plan.input_bytes, w.budget.hard_bytes)
                self.assertIn("Preserve the API", plan.text)
            self.assertGreater(w.epoch, 1)
            self.assertEqual(w.read(w.ref("t0", 1)), "payload " * 700)

    def test_evidence_policy_rejects_semantic_state(self):
        w = Workspace(Path(self.temp.name) / "evidence.jsonl", "evidence_workspace")
        w.observe("u", user("task"))
        plan = w.prepare()
        with self.assertRaises(InvalidTransition):
            w.decide(expected_revision=plan.revision, source_cut=plan.source_cut,
                     updates={"x": {"text": "claim", "sources": [w.ref("u").as_dict()]}})

    def test_protected_overflow_does_not_commit_view(self):
        self.w.observe("u", user("requirement " * 4000), pinned=True)
        before = len(self.w.journal.rows)
        with self.assertRaises(CapacityError):
            self.w.prepare()
        self.assertEqual(len(self.w.journal.rows), before)
        self.assertEqual(self.w.epoch, 0)

    def test_restore_exact_committed_request(self):
        self.w.observe("u", user("Task"), pinned=True)
        ref = self.w.ref("u").as_dict()
        expected = self.consume(updates={"next": {"text": "Inspect", "sources": [ref]}}, boundary=True)
        restored = Workspace(self.path, "state_workspace")
        self.assertEqual(restored.last_plan, expected)
        self.assertEqual(restored.prepare(), expected)

    def test_torn_tail_is_removed_but_committed_corruption_rejected(self):
        self.w.observe("u", user("Task"))
        expected = self.w.prepare()
        with self.path.open("ab") as stream:
            stream.write(b'{"type":"decis')
        restored = Workspace(self.path, "state_workspace")
        self.assertEqual(restored.last_plan, expected)
        with self.path.open("ab") as stream:
            stream.write(b'not-json\n')
        with self.assertRaises(InvalidTransition):
            Workspace(self.path, "state_workspace")

    def test_disk_failure_precedes_projection_change(self):
        with patch.object(self.w.journal, "append", side_effect=OSError("disk unavailable")):
            with self.assertRaises(OSError):
                self.w.observe("u", user("Task"))
        self.assertEqual(self.w.order, [])
        self.assertEqual(self.w.revision, 0)


if __name__ == "__main__":
    unittest.main()
