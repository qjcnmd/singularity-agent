"""Deterministic MECHANICS experiment, not an LLM benchmark.

The decision policy is scripted and sees only the current observation. It is
not proof that a model will choose/update/recall correctly. Baseline below is
unbounded replay, explicitly NOT Singularity's Summary implementation.
"""
import argparse
import json
import tempfile
from pathlib import Path
from statistics import mean

from engine import Workspace, canonical, size


def lcp_bytes(left: str, right: str) -> int:
    a, b = left.encode("utf-8"), right.encode("utf-8")
    i = 0
    while i < min(len(a), len(b)) and a[i] == b[i]:
        i += 1
    return i


def run(policy: str, steps: int, objects: int) -> dict:
    widths, prefixes, patch_bytes = [], [], 0
    raw_baseline_widths, raw_parts = [], []
    with tempfile.TemporaryDirectory() as directory:
        w = Workspace(Path(directory) / "trace.jsonl", policy)
        goal = [{"role": "user", "text": "Fix the implementation without changing public API behavior."}]
        w.observe("goal", goal, pinned=True)
        p = w.prepare()
        w.decide(expected_revision=p.revision, source_cut=p.source_cut, consume=p.unconsumed)
        previous = ""
        for i in range(steps):
            path = f"src/module_{i % objects}.rs"
            prefix = f"check={path}; revision=r{i}; result={'FAIL' if i % 3 else 'PASS'}\n"
            output = prefix + "compiler background detail\n" * 160 + f"EXACT_MIDDLE_{i}\n" + "footer\n" * 30
            messages = [{"role": "assistant", "text": "run verification", "calls": [f"call{i}"]},
                        {"role": "tool", "text": output, "call_id": f"call{i}"}]
            w.observe(f"step{i}", messages, object_key=path)
            p = w.prepare()
            widths.append(p.input_bytes)
            prefixes.append(lcp_bytes(previous, p.text))
            previous = p.text
            updates = None
            if policy == "state_workspace":
                ref = w.ref(f"step{i}", 1, 0, size(prefix))
                # Scripted local update; its correctness is supplied by the fixture.
                updates = {"current:" + path: {"text": prefix.rstrip(), "sources": [ref.as_dict()]}}
                patch_bytes += size(canonical(updates))
            w.decide(expected_revision=p.revision, source_cut=p.source_cut,
                     consume=p.unconsumed, updates=updates)
            raw_parts.append(messages)
            raw_baseline_widths.append(size(canonical(goal + [m for batch in raw_parts for m in batch])))
        probes = (0, steps // 2, steps - 1)
        recovered = sum(f"EXACT_MIDDLE_{i}" in w.read(w.ref(f"step{i}", 1)) for i in probes)
        # Reopening verifies the persisted chosen view, not newly regenerated reasoning.
        restored = Workspace(Path(directory) / "trace.jsonl", policy)
        exact_restore = restored.last_plan == w.last_plan and restored.claims == w.claims
        return {"policy": policy, "steps": steps, "object_count": objects,
                "mean_request_utf8_bytes": round(mean(widths), 2),
                "max_request_utf8_bytes": max(widths), "sum_request_utf8_bytes": sum(widths),
                "epochs": w.epoch, "scripted_semantic_patch_utf8_bytes": patch_bytes,
                "mean_common_prefix_bytes_NOT_cached_tokens": round(mean(prefixes), 2),
                "old_raw_source_reads": recovered, "raw_source_probes": len(probes),
                "exact_reopen": exact_restore,
                "unbounded_replay_mean_bytes_NOT_summary_baseline": round(mean(raw_baseline_widths), 2)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--steps", type=int, default=120)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.steps < 3:
        parser.error("--steps must be at least 3")
    report = {"experiment": "mechanics-only-v1", "unit": "UTF-8 bytes, NOT tokens",
              "paid_or_other_model_calls": 0,
              "not_measured": ["task success", "model state accuracy", "actual cached tokens", "latency", "dollar cost"],
              "warning": "Scripted decisions; no claim of superiority over real Summary or a real coding agent.",
              "runs": [run(policy, args.steps, objects) for objects in (1, 6)
                       for policy in ("state_workspace", "evidence_workspace")]}
    output = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output, encoding="utf-8")
    print(output, end="")


if __name__ == "__main__":
    main()
