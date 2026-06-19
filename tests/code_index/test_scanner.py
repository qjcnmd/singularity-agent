from pathlib import Path

import pytest

from miniharness.code_index import FileRole, LanguageId, WorkspaceScanner
from miniharness.code_index.exceptions import PathOutsideWorkspaceError


def test_scanner_respects_ignore_roles_hash_binary_and_path_safety(tmp_path: Path) -> None:
    (tmp_path / ".git").mkdir()
    (tmp_path / ".git" / "ignored.py").write_text("bad = True\n", encoding="utf-8")
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "app.py").write_text("def main():\n    pass\n", encoding="utf-8")
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests" / "test_app.py").write_text("def test_main():\n    pass\n", encoding="utf-8")
    (tmp_path / "pyproject.toml").write_text("[project]\nname='x'\n", encoding="utf-8")
    (tmp_path / "image.bin").write_bytes(b"\x00\x01\x02")

    scanner = WorkspaceScanner(tmp_path)
    records = {record.path: record for record in scanner.scan()}

    assert ".git/ignored.py" not in records
    assert records["src/app.py"].language == LanguageId.PYTHON
    assert FileRole.SOURCE in records["src/app.py"].roles
    assert records["src/app.py"].sha256
    assert FileRole.TEST in records["tests/test_app.py"].roles
    assert FileRole.CONFIG in records["pyproject.toml"].roles
    assert records["image.bin"].is_binary is True

    with pytest.raises(PathOutsideWorkspaceError):
        scanner.resolve(tmp_path.parent / "outside.py")
