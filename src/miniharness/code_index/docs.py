from __future__ import annotations

from miniharness.code_index.models import DocSectionRecord


def compact_doc_sections(sections: list[DocSectionRecord], *, limit: int = 20) -> list[dict[str, object]]:
    return [
        {
            "path": section.path,
            "title": section.title,
            "line_start": section.line_start,
            "line_end": section.line_end,
            "freshness": section.freshness.value,
            "confidence": section.confidence,
        }
        for section in sections[:limit]
    ]
