from __future__ import annotations

from pathlib import Path

from singularity.code_index.models import ConfigFactRecord, Evidence, FileRecord, LanguageId, TrustLevel
from singularity.code_index.plugins.javascript import JavaScriptPlugin


class TypeScriptPlugin(JavaScriptPlugin):
    name = "typescript_static"
    version = "1.0.0"
    languages = ("typescript",)
    language = LanguageId.TYPESCRIPT

    def extract_config(self, workspace_root: Path, file: FileRecord) -> list[ConfigFactRecord]:
        facts = super().extract_config(workspace_root, file)
        if file.path == "tsconfig.json":
            facts.append(
                ConfigFactRecord(
                    path=file.path,
                    key="typescript.tsconfig",
                    value=True,
                    fact_type="typecheck_config",
                    language=LanguageId.TYPESCRIPT,
                    confidence=0.9,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
        return facts
