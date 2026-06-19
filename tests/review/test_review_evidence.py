from __future__ import annotations

from dataclasses import dataclass

from miniharness.review.evidence import collect_review_evidence, stable_payload_hash, to_bounded_plain


@dataclass
class Snapshot:
    id: str
    secret: str
    values: list[int]


def test_evidence_normalization_bounds_payload_and_redacts_secret_like_keys() -> None:
    evidence = collect_review_evidence(
        edit_result=Snapshot(id="edit_1", secret="TOKEN=abc", values=[1, 2]),
        verification_result={
            "completion_assessment": {"status": "ready"},
            "raw_output": "x" * 5000,
        },
    )

    assert [item.source for item in evidence] == ["edit_result", "verification_result"]
    assert evidence[0].payload["secret"] == "[redacted]"
    assert len(str(evidence[1].payload)) < 2500
    assert evidence[0].payload_hash == stable_payload_hash(evidence[0].payload)


def test_to_bounded_plain_handles_model_dump_and_to_dict() -> None:
    class ModelLike:
        def model_dump(self, mode="json"):
            return {"mode": mode, "value": "ok"}

    class DictLike:
        def to_dict(self):
            return {"value": "ok"}

    assert to_bounded_plain(ModelLike()) == {"mode": "json", "value": "ok"}
    assert to_bounded_plain(DictLike()) == {"value": "ok"}
