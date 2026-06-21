from pathlib import Path

from singularity.context import ObservationStore, ReferenceResolver
from singularity.context.models import ContextFreshness, ContextReference


def test_reference_resolver_handles_file_transaction_policy_and_verification_refs(
    tmp_path: Path,
) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    refs = [
        ContextReference(
            ref_id="ref_file",
            ref_type="file",
            target="README.md",
            path="README.md",
            line_start=1,
            line_end=3,
            digest="abc",
            source_item_id="item_file",
        ),
        ContextReference(
            ref_id="ref_tx",
            ref_type="transaction",
            target="tx_1",
            source_item_id="item_tx",
        ),
        ContextReference(
            ref_id="ref_policy",
            ref_type="policy_decision",
            target="decision_1",
            source_item_id="item_policy",
        ),
        ContextReference(
            ref_id="ref_verify",
            ref_type="verification",
            target="check_1",
            source_item_id="item_verify",
        ),
    ]
    for ref in refs:
        store.save_reference(ref)
    resolver = ReferenceResolver(store)

    assert resolver.resolve("ref_file").path == "README.md"
    assert resolver.resolve_many(["ref_file", "missing"])[0].ref_id == "ref_file"
    assert resolver.references_for_file("README.md")[0].ref_id == "ref_file"
    assert resolver.references_for_transaction("tx_1")[0].ref_id == "ref_tx"
    assert resolver.references_for_policy_decision("decision_1")[0].ref_id == "ref_policy"
    assert resolver.references_for_verification("check_1")[0].ref_id == "ref_verify"
    assert "ref_file" in resolver.render_reference_for_model("ref_file")


def test_reference_resolver_marks_path_refs_stale(tmp_path: Path) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    store.save_reference(
        ContextReference(
            ref_id="ref_file",
            ref_type="file",
            target="README.md",
            path="README.md",
            digest="old",
            source_item_id="item_file",
        )
    )
    resolver = ReferenceResolver(store)

    resolver.mark_references_stale_for_path("README.md", reason="file changed")

    ref = resolver.resolve("ref_file")
    assert ref.freshness == ContextFreshness.STALE
    assert resolver.validate_reference_freshness("ref_file") is False

