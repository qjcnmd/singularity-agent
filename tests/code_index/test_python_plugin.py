from pathlib import Path

from miniharness.code_index import WorkspaceScanner
from miniharness.code_index.plugins.python import PythonPlugin


def test_python_plugin_extracts_symbols_imports_entrypoints_and_pytest_mapping(tmp_path: Path) -> None:
    (tmp_path / "src" / "pkg").mkdir(parents=True)
    (tmp_path / "tests").mkdir()
    (tmp_path / "src" / "pkg" / "__init__.py").write_text("", encoding="utf-8")
    (tmp_path / "src" / "pkg" / "cli.py").write_text(
        "import os\nfrom pathlib import Path\nimport typer\napp = typer.Typer()\n\n"
        "class Service:\n    pass\n\n"
        "def run(value):\n    return Path(value)\n",
        encoding="utf-8",
    )
    (tmp_path / "tests" / "test_cli.py").write_text(
        "def test_run():\n    assert True\n",
        encoding="utf-8",
    )

    files = WorkspaceScanner(tmp_path).scan()
    file = next(record for record in files if record.path == "src/pkg/cli.py")
    plugin = PythonPlugin()

    symbols = plugin.extract_symbols(tmp_path, file)
    deps = plugin.extract_dependencies(tmp_path, file)
    entries = plugin.extract_entrypoints(tmp_path, file)
    mappings = plugin.extract_tests(tmp_path, file, files)

    assert any(symbol.name == "Service" and symbol.kind.value == "class" for symbol in symbols)
    assert any(symbol.name == "run" for symbol in symbols)
    assert any(dep.imported == "pathlib.Path" for dep in deps)
    assert any(entry.kind == "framework_entrypoint" for entry in entries)
    assert any(mapping.test_path == "tests/test_cli.py" for mapping in mappings)
