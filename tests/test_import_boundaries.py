from __future__ import annotations

import ast
from pathlib import Path


def _imports_from(path: Path) -> set[str]:
    tree = ast.parse(path.read_text(encoding="utf-8"))
    imports: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            imports.update(alias.name for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module:
            imports.add(node.module)
    return imports


def test_error_mapping_does_not_import_tool_protocol_package() -> None:
    imports = _imports_from(Path("src/singularity/error_mapping.py"))

    assert not any(name.startswith("singularity.tool_protocol") for name in imports)
