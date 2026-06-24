from __future__ import annotations

from collections.abc import Callable
from typing import Any

from singularity.context.models import CacheAttribution, CacheAttributionSource


class ContextUsageReporter:
    def __init__(
        self,
        *,
        run_id: str,
        store: Any,
        provider: Any | None = None,
        model_runner: Any | None = None,
        emit_context_event: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> None:
        self.run_id = run_id
        self.store = store
        self.provider = provider
        self.model_runner = model_runner
        self.emit_context_event = emit_context_event

    def record_model_usage(self, bundle: Any | None, *, result: Any) -> None:
        if bundle is None:
            return
        usage = getattr(result, "usage", None)
        if usage is None:
            return
        input_tokens = int(getattr(usage, "input_tokens", 0) or 0)
        cached_tokens = int(getattr(usage, "cached_input_tokens", 0) or 0)
        cache_hit_ratio = _ratio(cached_tokens, input_tokens)
        cache = dict(bundle.metadata.get("cache") or {})
        cache_attribution = dict(cache.get("cache_attribution") or {})
        result_cache = dict((getattr(result, "metadata", {}) or {}).get("cache") or {})
        result_attribution = (
            dict(result_cache.get("cache_attribution") or {})
            if isinstance(result_cache, dict)
            else {}
        )
        if result_attribution:
            cache_attribution.update(result_attribution)
        if cached_tokens > 0:
            cache_attribution.update(
                CacheAttribution(
                    source=CacheAttributionSource.PROVIDER_NATIVE,
                    confidence=1.0,
                    reasons=[
                        *[str(item) for item in cache_attribution.get("reasons") or []],
                        "usage.cached_input_tokens",
                    ],
                    evidence=[
                        *[str(item) for item in cache_attribution.get("evidence") or []],
                        "usage.cached_input_tokens",
                    ],
                    provider_name=cache_attribution.get("provider_name"),
                    model_name=cache_attribution.get("model_name"),
                ).to_dict()
            )
        elif not cache_attribution:
            cache_attribution = self.current_cache_attribution(last_bundle=bundle).to_dict()
        cache.update(
            {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": list(
                    (getattr(result, "metadata", {}) or {}).get("cache_miss_reasons")
                    or result_cache.get("cache_miss_reasons")
                    or cache.get("cache_miss_reasons")
                    or []
                ),
                "cache_attribution": cache_attribution,
            }
        )
        bundle.metadata["cache"] = cache
        report = dict(bundle.metadata.get("context_usage_report") or {})
        report.update(
            {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": cache["cache_miss_reasons"],
                "cache_attribution": cache_attribution,
            }
        )
        bundle.metadata["context_usage_report"] = report
        self.store.update_bundle_metadata(
            bundle_id=bundle.bundle_id,
            run_id=self.run_id,
            metadata=bundle.metadata,
        )
        if self.emit_context_event is not None:
            self.emit_context_event(
                "context.cache_usage_recorded",
                {
                    "bundle_id": bundle.bundle_id,
                    "input_tokens": input_tokens,
                    "cached_input_tokens": cached_tokens,
                    "cache_hit_ratio": cache_hit_ratio,
                    "cache_miss_reasons": cache["cache_miss_reasons"],
                    "cache_attribution": cache_attribution,
                },
            )

    def annotate_bundle_cache(
        self,
        bundle: Any,
        *,
        previous_bundle: Any | None,
        last_bundle: Any | None = None,
    ) -> None:
        cache = dict(bundle.metadata.get("cache") or {})
        reasons = self.cache_miss_reasons(bundle, previous_bundle=previous_bundle)
        cache["cache_miss_reasons"] = reasons
        cache.setdefault(
            "cache_attribution",
            self.current_cache_attribution(last_bundle=last_bundle).to_dict(),
        )
        bundle.metadata["cache"] = cache
        report = dict(bundle.metadata.get("context_usage_report") or {})
        report["cache_miss_reasons"] = reasons
        report.setdefault("cache_attribution", cache["cache_attribution"])
        bundle.metadata["context_usage_report"] = report

    def current_cache_attribution(
        self,
        *,
        last_bundle: Any | None = None,
        source_items: list[Any] | None = None,
        previous_summary: Any | None = None,
    ) -> CacheAttribution:
        provider_name = None
        model_name = None
        if self.model_runner is not None:
            config = getattr(self.model_runner, "config", None)
            model_name = getattr(config, "default_model", None) or getattr(config, "model", None)
        elif self.provider is not None:
            provider_name = getattr(self.provider, "provider_name", None) or getattr(self.provider, "name", lambda: None)()
        reasons = []
        evidence = []
        confidence = 0.0
        source = CacheAttributionSource.UNKNOWN
        if last_bundle is not None:
            cache = dict(last_bundle.metadata.get("cache") or {})
            cache_attribution = dict(cache.get("cache_attribution") or {})
            if cache.get("cached_input_tokens"):
                source = CacheAttributionSource.PROVIDER_NATIVE
                confidence = 1.0
                evidence.append("bundle_cache.cached_input_tokens")
                reasons.append("provider_native_cache_diagnostic_present")
            elif cache_attribution.get("source") == CacheAttributionSource.PROVIDER_NATIVE.value:
                source = CacheAttributionSource.PROVIDER_NATIVE
                confidence = float(cache_attribution.get("confidence") or 0.9)
                evidence.extend(str(item) for item in cache_attribution.get("evidence") or [])
                reasons.extend(str(item) for item in cache_attribution.get("reasons") or [])
            else:
                reasons.append("no_native_cache_diagnostics")
        else:
            reasons.append("no_bundle")
        if source == CacheAttributionSource.UNKNOWN and (source_items or previous_summary):
            source = CacheAttributionSource.COMPONENT_INFERRED
            confidence = 0.35 if source_items else 0.2
            if previous_summary is not None:
                evidence.append("previous_summary_present")
        return CacheAttribution(
            source=source,
            confidence=confidence,
            reasons=reasons,
            evidence=evidence,
            provider_name=provider_name,
            model_name=model_name,
        )

    @staticmethod
    def cache_miss_reasons(bundle: Any, *, previous_bundle: Any | None) -> list[str]:
        if previous_bundle is None:
            return ["first_request"]
        reasons: list[str] = []
        previous = previous_bundle.metadata or {}
        current = bundle.metadata or {}
        if previous.get("context_shape_hash") != current.get("context_shape_hash"):
            reasons.append("context_shape_change")
        if previous.get("context_ordering_hash") != current.get("context_ordering_hash"):
            reasons.append("ordering_change")
        if previous_bundle.compression_snapshot_id != bundle.compression_snapshot_id:
            reasons.append("compaction_change")
        previous_tool_tokens = getattr(previous_bundle.budget, "tool_schema_tokens", 0)
        if previous_tool_tokens != bundle.budget.tool_schema_tokens:
            reasons.append("tool_schema_change")
        return reasons

    def diagnostic(self, bundle: Any | None) -> dict[str, Any]:
        if bundle is None:
            bundle = self.store.latest_bundle(self.run_id)
        if bundle is None:
            return {
                "bundle_id": None,
                "layer_token_usage": {},
                "included_item_ids": [],
                "excluded_item_ids": [],
                "stale_item_ids": [],
                "summary_item_ids": [],
                "recent_tail_item_ids": [],
                "input_tokens": 0,
                "cached_input_tokens": 0,
                "cache_hit_ratio": 0.0,
                "cache_attribution": CacheAttribution().to_dict(),
                "cache_miss_reasons": [],
                "recommendations": [],
            }
        report = dict(bundle.metadata.get("context_usage_report") or {})
        cache = dict(bundle.metadata.get("cache") or {})
        cache_attribution = dict(
            report.get("cache_attribution")
            or cache.get("cache_attribution")
            or CacheAttribution().to_dict()
        )
        return {
            "bundle_id": bundle.bundle_id,
            "layer_token_usage": dict(report.get("layer_token_usage") or {}),
            "included_item_ids": list(report.get("included_item_ids") or bundle.included_item_ids),
            "excluded_item_ids": list(report.get("excluded_item_ids") or bundle.excluded_item_ids),
            "stale_item_ids": list(report.get("stale_item_ids") or []),
            "summary_item_ids": list(report.get("summary_item_ids") or []),
            "recent_tail_item_ids": list(report.get("recent_tail_item_ids") or []),
            "input_tokens": int(report.get("input_tokens") or cache.get("input_tokens") or 0),
            "cached_input_tokens": int(
                report.get("cached_input_tokens") or cache.get("cached_input_tokens") or 0
            ),
            "cache_hit_ratio": float(
                report.get("cache_hit_ratio") or cache.get("cache_hit_ratio") or 0.0
            ),
            "cache_attribution": cache_attribution,
            "cache_miss_reasons": list(
                report.get("cache_miss_reasons") or cache.get("cache_miss_reasons") or []
            ),
            "recommendations": list(report.get("recommendations") or []),
        }


def _ratio(numerator: int, denominator: int) -> float:
    if denominator <= 0:
        return 0.0
    return round(numerator / denominator, 4)
