import json
import threading
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.policy import PolicyConfig, PolicyEngine, ResourceRef
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.tools import (
    ToolCachePolicy,
    ToolPolicy,
    ToolRegistry,
    ToolExecutor,
    ToolSpec,
)
from singularity.tools.mutation import register_mutation_tools
from singularity.workspace import WorkspaceMutationManager
from singularity.jsonl_trace import JsonlTraceRecorder
from tests.tool_executor_helpers import make_test_policy_engine


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def make_tool_call(
    name: str,
    arguments: dict[str, Any] | None = None,
    *,
    tool_call_id: str | None = None,
) -> dict[str, Any]:
    return {
        "id": tool_call_id or f"call_{name}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments or {})},
    }


def test_read_file_cache_invalidates_when_file_changes(tmp_path: Path) -> None:
    path = tmp_path / "README.md"
    path.write_text("first", encoding="utf-8")
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md"}, tool_call_id="call_read_first")
    )
    path.write_text("second", encoding="utf-8")
    second = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md"}, tool_call_id="call_read_second")
    )

    assert first.content["content"] == "first"
    assert second.content["content"] == "second"
    assert second.metadata["cache_hit"] is False


def test_sensitive_result_is_not_cached(tmp_path: Path) -> None:
    calls: list[int] = []

    def handler(_args: EmptyInput) -> dict[str, str]:
        calls.append(1)
        return {"token": "secret-token"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="secretish",
            description="secretish",
            input_model=EmptyInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
            sensitivity="secret",
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("secretish", tool_call_id="call_secret_1"))
    component.execute_tool_call(make_tool_call("secretish", tool_call_id="call_secret_2"))

    assert len(calls) == 2


def test_idempotent_false_is_not_cached(tmp_path: Path) -> None:
    calls: list[int] = []

    def handler(_args: EmptyInput) -> dict[str, int]:
        calls.append(1)
        return {"count": len(calls)}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="non_idempotent",
            description="non idempotent",
            input_model=EmptyInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
            idempotent=False,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("non_idempotent", tool_call_id="call_non_idem_1"))
    second = component.execute_tool_call(make_tool_call("non_idempotent", tool_call_id="call_non_idem_2"))

    assert second.content["count"] == 2
    assert second.metadata["cache_hit"] is False


def test_bounded_lru_evicts_old_entries(tmp_path: Path) -> None:
    calls: list[str] = []

    class ValueInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        value: str

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="bounded",
            description="bounded",
            input_model=ValueInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True, max_entries=2),
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    for index, value in enumerate(["a", "b", "c", "a"], start=1):
        component.execute_tool_call(
            make_tool_call("bounded", {"value": value}, tool_call_id=f"call_bounded_{index}")
        )

    assert calls == ["a", "b", "c", "a"]


def test_cache_can_be_invalidated_by_path(tmp_path: Path) -> None:
    path = tmp_path / "a.txt"
    path.write_text("first", encoding="utf-8")
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    component.execute_tool_call(
        make_tool_call("read_file", {"path": "a.txt"}, tool_call_id="call_cache_path_1")
    )
    path.write_text("second", encoding="utf-8")

    component.invalidate_paths(["a.txt"])
    result = component.execute_tool_call(
        make_tool_call("read_file", {"path": "a.txt"}, tool_call_id="call_cache_path_2")
    )

    assert result.content["content"] == "second"
    assert result.metadata["cache_hit"] is False


def test_file_invalidation_evicts_parent_directory_cache_entry(tmp_path: Path) -> None:
    calls: list[int] = []

    class DirInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        path: str

    def handler(args: DirInput) -> dict[str, int]:
        calls.append(1)
        return {"count": len(calls)}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="scan_dir",
            description="scan dir",
            input_model=DirInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
            resource_resolver=lambda args, _root: [
                ResourceRef("directory", args["path"], workspace_relative=True)
            ],
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("scan_dir", {"path": "src"}, tool_call_id="call_scan_1"))
    component.invalidate_paths(["src/app.py"])
    result = component.execute_tool_call(
        make_tool_call("scan_dir", {"path": "src"}, tool_call_id="call_scan_2")
    )

    assert result.content["count"] == 2
    assert result.metadata["cache_hit"] is False


def test_cacheable_tool_call_id_conflict_is_rejected(tmp_path: Path) -> None:
    calls: list[str] = []

    class ValueInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        value: str

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="cached_echo",
            description="cached echo",
            input_model=ValueInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(make_tool_call("cached_echo", {"value": "x"}))
    second = component.execute_tool_call(make_tool_call("cached_echo", {"value": "y"}))

    assert first.ok is True
    assert second.ok is False
    assert second.error_code == "conflicting_replay"
    assert calls == ["x"]


def test_cache_hit_still_records_tool_call_id_for_conflict_detection(tmp_path: Path) -> None:
    calls: list[str] = []

    class ValueInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        value: str

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="cached_echo",
            description="cached echo",
            input_model=ValueInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(
        make_tool_call("cached_echo", {"value": "x"}, tool_call_id="call_first")
    )
    cached = component.execute_tool_call(
        make_tool_call("cached_echo", {"value": "x"}, tool_call_id="call_cached")
    )
    conflict = component.execute_tool_call(
        make_tool_call("cached_echo", {"value": "y"}, tool_call_id="call_cached")
    )

    assert first.ok is True
    assert cached.ok is True
    assert cached.metadata["cache_hit"] is True
    assert conflict.ok is False
    assert conflict.error_code == "conflicting_replay"
    assert calls == ["x"]


# ---------------------------------------------------------------------------
# Task 8.5: resource boundary tests (read_file size cap, search_text skip)
# ---------------------------------------------------------------------------


def test_read_file_truncates_large_file_without_loading_full_content(tmp_path: Path) -> None:
    # Create a file larger than max_bytes to verify the size check kicks in
    # before the full content is loaded into memory. We keep max_bytes modest
    # because the in-process tool backend sends results through a pipe whose
    # buffer can deadlock the subprocess for very large payloads (pre-existing
    # executor behavior, unrelated to this change).
    payload = "A" * 20000
    big = tmp_path / "big.txt"
    big.write_text(payload, encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call(
            "read_file",
            {"path": "big.txt", "max_bytes": 5000},
            tool_call_id="call_big_read",
        )
    )

    assert result.ok is True
    content = result.content
    assert content["truncated"] is True
    assert content["bytes_read"] == 5000
    assert content["bytes_total"] == len(payload)
    assert len(content["content"]) == 5000
    assert content["content"] == "A" * 5000


def test_read_file_returns_full_content_when_under_limit(tmp_path: Path) -> None:
    small = tmp_path / "small.txt"
    small.write_text("hello world", encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call(
            "read_file",
            {"path": "small.txt", "max_bytes": 200000},
            tool_call_id="call_small_read",
        )
    )

    assert result.ok is True
    assert result.content["truncated"] is False
    assert result.content["content"] == "hello world"
    assert result.content["bytes_total"] == 11


def test_search_text_skips_oversized_files(tmp_path: Path) -> None:
    # File larger than max_file_bytes should be skipped and recorded.
    big = tmp_path / "big.log"
    big.write_text("NEEDLE\n" + "x" * 5000, encoding="utf-8")  # > 5000 bytes

    small = tmp_path / "small.log"
    small.write_text("NEEDLE found here\n", encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call(
            "search_text",
            {"query": "NEEDLE", "path": ".", "max_file_bytes": 200},
            tool_call_id="call_search_big",
        )
    )

    assert result.ok is True
    skipped = result.content["skipped_files"]
    skipped_names = {entry["path"] for entry in skipped}
    assert "big.log" in skipped_names
    # small.log should still be scanned and produce a match.
    match_paths = {match["path"] for match in result.content["matches"]}
    assert "small.log" in match_paths
    assert "big.log" not in match_paths


def test_search_text_scans_file_when_under_limit(tmp_path: Path) -> None:
    target = tmp_path / "notes.md"
    target.write_text("findme\nline two\n", encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call(
            "search_text",
            {"query": "findme", "path": "notes.md", "max_file_bytes": 10_000_000},
            tool_call_id="call_search_small",
        )
    )

    assert result.ok is True
    assert len(result.content["matches"]) == 1
    assert result.content["matches"][0]["text"] == "findme"
    assert result.content["skipped_files"] == []


# ---------------------------------------------------------------------------
# Task 8.4: incremental cache invalidation after write tool execution
# ---------------------------------------------------------------------------


def _make_write_capable_executor(tmp_path: Path) -> ToolExecutor:
    policy_engine = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=PermissionProfile.default_for_workspace(
                tmp_path,
                profile=PermissionProfileName.DANGER_FULL_ACCESS,
            ),
        )
    )
    registry = ToolRegistry(tmp_path)
    mutation = WorkspaceMutationManager(
        tmp_path,
        policy_engine=policy_engine,
    )
    register_mutation_tools(registry, mutation)
    return ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=policy_engine,
    )


def test_write_tool_invalidates_only_affected_cache_entries(tmp_path: Path) -> None:
    (tmp_path / "a.txt").write_text("alpha", encoding="utf-8")
    (tmp_path / "b.txt").write_text("beta", encoding="utf-8")

    component = _make_write_capable_executor(tmp_path)

    # Populate cache for both files.
    first_a = component.execute_tool_call(
        make_tool_call("read_file", {"path": "a.txt"}, tool_call_id="call_read_a_1")
    )
    first_b = component.execute_tool_call(
        make_tool_call("read_file", {"path": "b.txt"}, tool_call_id="call_read_b_1")
    )
    assert first_a.content["content"] == "alpha"
    assert first_b.content["content"] == "beta"
    assert len(component._cache) == 2

    # Mutate only a.txt via a write tool.
    mutation_result = component.execute_tool_call(
        make_tool_call(
            "workspace_replace_text",
            {"path": "a.txt", "old_text": "alpha", "new_text": "alpha2"},
            tool_call_id="call_mutate_a",
        )
    )
    assert mutation_result.ok is True

    # The a.txt cache entry should have been invalidated, but b.txt preserved.
    assert len(component._cache) == 1

    second_a = component.execute_tool_call(
        make_tool_call("read_file", {"path": "a.txt"}, tool_call_id="call_read_a_2")
    )
    second_b = component.execute_tool_call(
        make_tool_call("read_file", {"path": "b.txt"}, tool_call_id="call_read_b_2")
    )

    assert second_a.content["content"] == "alpha2"
    assert second_a.metadata["cache_hit"] is False
    # b.txt was not touched by the write - its cache entry must survive.
    assert second_b.metadata["cache_hit"] is True
    assert second_b.content["content"] == "beta"


def test_write_tool_create_file_invalidates_parent_directory_cache(tmp_path: Path) -> None:
    (tmp_path / "existing.txt").write_text("keep", encoding="utf-8")

    component = _make_write_capable_executor(tmp_path)

    # Cache a directory listing.
    first_list = component.execute_tool_call(
        make_tool_call("list_files", {"path": "."}, tool_call_id="call_list_1")
    )
    assert first_list.ok is True
    assert "existing.txt" in first_list.content["files"]
    assert len(component._cache) == 1

    # Create a new file - this changes the directory listing.
    create_result = component.execute_tool_call(
        make_tool_call(
            "workspace_create_file",
            {"path": "new.txt", "content": "fresh"},
            tool_call_id="call_create",
        )
    )
    assert create_result.ok is True

    # The directory listing cache entry overlaps with the created file path,
    # so it must be invalidated.
    second_list = component.execute_tool_call(
        make_tool_call("list_files", {"path": "."}, tool_call_id="call_list_2")
    )
    assert second_list.ok is True
    assert second_list.metadata["cache_hit"] is False
    assert "new.txt" in second_list.content["files"]


# ---------------------------------------------------------------------------
# Task 9.2: multi-threaded concurrent access tests
# ---------------------------------------------------------------------------


def test_concurrent_read_only_calls_do_not_raise_runtime_error(tmp_path: Path) -> None:
    # Concurrent access to the OrderedDict-backed cache (move_to_end, popitem,
    # iteration during invalidate_paths) previously could raise RuntimeError
    # ("OrderedDict mutated during iteration") or similar. The RLock guards
    # should prevent that.
    (tmp_path / "shared.txt").write_text("payload\n", encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    errors: list[BaseException] = []

    def worker() -> None:
        try:
            for index in range(40):
                component.execute_tool_call(
                    make_tool_call(
                        "read_file",
                        {"path": "shared.txt"},
                        tool_call_id=f"call_thread_{threading.get_ident()}_{index}",
                    )
                )
        except BaseException as exc:  # noqa: BLE001 - record any failure
            errors.append(exc)

    threads = [threading.Thread(target=worker) for _ in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == [], f"concurrent cache access raised: {errors}"


def test_concurrent_cache_mixed_hits_and_misses_stay_consistent(tmp_path: Path) -> None:
    (tmp_path / "a.txt").write_text("A" * 100, encoding="utf-8")
    (tmp_path / "b.txt").write_text("B" * 100, encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    results: list[Any] = []
    results_lock = threading.Lock()

    def worker(path: str) -> None:
        local: list[Any] = []
        for index in range(25):
            result = component.execute_tool_call(
                make_tool_call(
                    "read_file",
                    {"path": path},
                    tool_call_id=f"call_{path}_{threading.get_ident()}_{index}",
                )
            )
            local.append(result)
        with results_lock:
            results.extend(local)

    threads = [
        threading.Thread(target=worker, args=("a.txt",)),
        threading.Thread(target=worker, args=("b.txt",)),
        threading.Thread(target=worker, args=("a.txt",)),
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    # Every result must be correct for its path - no cross-contamination.
    for result in results:
        assert result.ok is True
        content = result.content["content"]
        if result.content["path"] == "a.txt":
            assert content == "A" * 100
        elif result.content["path"] == "b.txt":
            assert content == "B" * 100
        else:
            raise AssertionError(f"unexpected path: {result.content['path']}")


def test_concurrent_invalidate_and_read_does_not_corrupt_cache(tmp_path: Path) -> None:
    (tmp_path / "target.txt").write_text("v1", encoding="utf-8")

    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    errors: list[BaseException] = []

    def reader() -> None:
        try:
            for index in range(50):
                component.execute_tool_call(
                    make_tool_call(
                        "read_file",
                        {"path": "target.txt"},
                        tool_call_id=f"call_reader_{threading.get_ident()}_{index}",
                    )
                )
        except BaseException as exc:  # noqa: BLE001
            errors.append(exc)

    def invalidator() -> None:
        try:
            for _ in range(50):
                component.invalidate_paths(["target.txt"])
        except BaseException as exc:  # noqa: BLE001
            errors.append(exc)

    threads = [
        threading.Thread(target=reader),
        threading.Thread(target=reader),
        threading.Thread(target=invalidator),
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == [], f"concurrent invalidate/read raised: {errors}"
