from pathlib import Path
import sqlite3

import pytest

from singularity.context import ContextVersionConflict, ObservationStore
from singularity.context.models import (
    ContextAuthority,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextReference,
    ContextSource,
    ContextSensitivity,
    ContextSnapshot,
)


def make_item(
    item_id: str,
    *,
    phase_id: str = "inspect",
    layer: ContextLayer = ContextLayer.EVIDENCE,
    item_type: ContextItemType = ContextItemType.TOOL_OBSERVATION,
    content: object = "content",
) -> ContextItem:
    return ContextItem(
        item_id=item_id,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id=phase_id,
        layer=layer,
        source_component=ContextSource.TOOL,
        item_type=item_type,
        content=content,
        authority=ContextAuthority.TOOL,
        sensitivity=ContextSensitivity.WORKSPACE,
        token_count=4,
    )


def test_store_appends_and_queries_context_items_by_layer_type_and_phase(
    tmp_path: Path,
) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    store.append_item(make_item("item_1", phase_id="inspect"))
    store.append_item(
        make_item(
            "item_2",
            phase_id="verify",
            layer=ContextLayer.VERIFICATION,
            item_type=ContextItemType.VERIFICATION_EVIDENCE,
        )
    )

    queried = store.query_items(
        run_id="run_1",
        task_id="task_1",
        phase_id="verify",
        layer=ContextLayer.VERIFICATION,
        item_type=ContextItemType.VERIFICATION_EVIDENCE,
        source_component=ContextSource.TOOL,
    )

    assert [item.item_id for item in queried] == ["item_2"]
    assert store.load_item("item_1") is not None
    assert store.events_for_run("run_1")[-1]["event_type"] == "context.item_added"


def test_store_detects_optimistic_version_conflict_for_context_items(
    tmp_path: Path,
) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    version = store.current_version("run_1")
    store.append_item(make_item("item_1"), expected_version=version)

    with pytest.raises(ContextVersionConflict):
        store.append_item(make_item("item_2"), expected_version=version)


def test_store_snapshot_bundle_reference_and_migration_are_idempotent(
    tmp_path: Path,
) -> None:
    db_path = tmp_path / "context.sqlite3"
    store = ObservationStore(db_path)
    reference = ContextReference(
        ref_id="ref_readme",
        ref_type="file",
        target="README.md",
        path="README.md",
        digest="abc",
        source_item_id="item_1",
    )
    item = make_item("item_1")
    item.references.append(reference)
    store.append_item(item)
    store.save_reference(reference)
    snapshot = ContextSnapshot(
        snapshot_id="snap_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        goal="inspect",
        summary="summary",
        retained_item_ids=["item_1"],
        known_observation_ids=["item_1"],
        version=store.current_version("run_1"),
    )
    store.save_snapshot(snapshot)

    reopened = ObservationStore(db_path)

    assert reopened.latest_snapshot("run_1").summary == "summary"
    assert reopened.references_for_target("README.md")[0].ref_id == "ref_readme"
    assert reopened.resolve_reference("ref_readme").path == "README.md"


def test_store_migrates_legacy_reference_and_snapshot_tables(tmp_path: Path) -> None:
    import sqlite3

    db_path = tmp_path / "legacy.sqlite3"
    connection = sqlite3.connect(db_path)
    connection.executescript(
        """
        create table runs(run_id text primary key, version integer not null default 0, created_at text not null);
        create table messages(
            id integer primary key autoincrement,
            run_id text not null,
            seq integer not null,
            role text,
            content text,
            payload text not null,
            tool_call_id text,
            created_at text not null
        );
        create table observations(id text primary key, run_id text);
        create table context_references(
            id text primary key,
            observation_id text not null,
            type text not null,
            path text,
            line_start integer,
            line_end integer,
            digest text
        );
        create table snapshots(
            id text primary key,
            run_id text not null,
            goal text not null,
            summary text not null,
            retained_messages text not null,
            known_observation_ids text not null,
            version integer not null,
            created_at text not null
        );
        insert into context_references(id, observation_id, type, path, digest)
        values('ref_old', 'obs_1', 'file', 'README.md', 'abc');
        insert into snapshots(id, run_id, goal, summary, retained_messages, known_observation_ids, version, created_at)
        values('snap_old', 'run_1', 'goal', 'legacy summary', '[]', '["obs_1"]', 1, '2026-01-01T00:00:00+00:00');
        """
    )
    connection.commit()
    connection.close()

    store = ObservationStore(db_path)
    store.save_reference(
        ContextReference(
            ref_id="ref_new",
            ref_type="file",
            target="pyproject.toml",
            path="pyproject.toml",
            source_item_id="item_new",
        )
    )

    assert store.resolve_reference("ref_old").ref_type == "file"
    assert store.resolve_reference("ref_new").path == "pyproject.toml"
    assert store.latest_snapshot("run_1").summary == "legacy summary"


def test_store_redacts_secret_content_by_default(tmp_path: Path) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    secret_item = make_item(
        "item_secret",
        content="OPENAI_API_KEY=sk-secret-123456789",
    )
    secret_item.sensitivity = ContextSensitivity.SECRET
    store.append_item(secret_item)

    loaded = store.load_item("item_secret")

    assert loaded is not None
    assert "sk-secret" not in str(loaded.content)
    assert "<redacted:" in str(loaded.content)
    assert loaded.content_digest == secret_item.content_digest
    assert loaded.sensitivity == ContextSensitivity.SECRET


def test_store_redacts_common_github_and_npm_tokens(tmp_path: Path) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    item = make_item(
        "item_token",
        content={
            "stdout": "token ghp_abcdefghijklmnopqrstuvwxyz123456 npm_abcdefghijklmnop"
        },
    )
    store.append_item(item)

    loaded = store.load_item("item_token")

    assert loaded is not None
    rendered = str(loaded.content)
    assert "ghp_" not in rendered
    assert "npm_" not in rendered
    assert "<redacted:" in rendered


def test_mark_stale_and_supersede_preserve_append_only_events(tmp_path: Path) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    store.append_item(make_item("item_old"))
    store.append_item(make_item("item_new"))

    store.mark_stale("item_old", reason="file changed")
    store.supersede_item("item_old", superseded_by="item_new")

    loaded = store.load_item("item_old")
    events = [event["event_type"] for event in store.events_for_run("run_1")]
    assert loaded is not None
    assert loaded.freshness == ContextFreshness.OBSOLETE
    assert loaded.metadata["superseded_by"] == "item_new"
    assert "context.item_stale" in events
    assert "context.item_superseded" in events


def test_pin_item_updates_priority_without_marking_item_obsolete(tmp_path: Path) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")
    store.append_item(make_item("item_pin"))

    store.set_item_pinned("item_pin", pinned=True)

    loaded = store.load_item("item_pin")
    events = [event["event_type"] for event in store.events_for_run("run_1")]
    assert loaded is not None
    assert loaded.pinned is True
    assert loaded.freshness == ContextFreshness.CURRENT
    assert "context.item_pinned" in events


def test_store_close_releases_sqlite_connection(tmp_path: Path) -> None:
    store = ObservationStore(tmp_path / "context.sqlite3")

    store.close()

    with pytest.raises(sqlite3.ProgrammingError):
        store.connection.execute("select 1")


def test_store_supports_concurrent_reads_and_writes_without_error(tmp_path: Path) -> None:
    import threading

    store = ObservationStore(tmp_path / "context.sqlite3")
    store.append_item(make_item("item_seed"))
    errors: list[BaseException] = []

    def writer() -> None:
        try:
            for index in range(40):
                store.append_item(make_item(f"item_writer_{index}"))
        except BaseException as exc:
            errors.append(exc)

    def reader() -> None:
        try:
            for _ in range(80):
                items = store.query_items(run_id="run_1")
                events = store.events_for_run("run_1")
                loaded = store.load_item("item_seed")
                assert loaded is not None
                assert isinstance(items, list)
                assert isinstance(events, list)
        except BaseException as exc:
            errors.append(exc)

    threads = [threading.Thread(target=reader) for _ in range(4)]
    threads.append(threading.Thread(target=writer))
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    items = store.query_items(run_id="run_1")
    item_ids = {item.item_id for item in items}
    assert "item_seed" in item_ids
    for index in range(40):
        assert f"item_writer_{index}" in item_ids
    events = store.events_for_run("run_1")
    assert len(events) >= 41


def test_store_concurrent_reads_are_consistent_after_writes(tmp_path: Path) -> None:
    import threading

    store = ObservationStore(tmp_path / "context.sqlite3")
    for index in range(20):
        store.append_item(make_item(f"item_init_{index}"))

    consistency_errors: list[BaseException] = []

    def reader() -> None:
        try:
            for _ in range(50):
                items = store.query_items(run_id="run_1")
                events = store.events_for_run("run_1")
                # events count should always be >= items count (each item_added emits an event)
                assert len(events) >= len(items)
        except BaseException as exc:
            consistency_errors.append(exc)

    def writer() -> None:
        try:
            for index in range(30):
                store.append_item(make_item(f"item_concurrent_{index}"))
        except BaseException as exc:
            consistency_errors.append(exc)

    threads = [threading.Thread(target=reader) for _ in range(3)]
    threads.append(threading.Thread(target=writer))
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert consistency_errors == []
    final_items = store.query_items(run_id="run_1")
    assert len(final_items) == 50
