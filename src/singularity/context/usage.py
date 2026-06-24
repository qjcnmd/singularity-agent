from __future__ import annotations

from typing import Any

from singularity.context.models import CacheAttribution, CacheAttributionSource


class ContextUsageReporter:
    def __init__(self, manager: Any) -> None:
        self.manager = manager

    def record_model_usage(self, result: Any) -> None:
        if self.manager.last_bundle is None:
            return
        usage = getattr(result, "usage", None)
        if usage is None:
            return
        input_tokens = int(getattr(usage, "input_tokens", 0) or 0)
        cached_tokens = int(getattr(usage, "cached_input_tokens", 0) or 0)
        cache_hit_ratio = _ratio(cached_tokens, input_tokens)
        cache = dict(self.manager.last_bundle.metadata.get("cache") or {})
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
            cache_attribution = self.manager._current_cache_attribution().to_dict()
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
        self.manager.last_bundle.metadata["cache"] = cache
        report = dict(self.manager.last_bundle.metadata.get("context_usage_report") or {})
        report.update(
            {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": cache["cache_miss_reasons"],
                "cache_attribution": cache_attribution,
            }
        )
        self.manager.last_bundle.metadata["context_usage_report"] = report
        self.manager.store.update_bundle_metadata(
            bundle_id=self.manager.last_bundle.bundle_id,
            run_id=self.manager.run_id,
            metadata=self.manager.last_bundle.metadata,
        )
        self.manager._emit_context_event(
            "context.cache_usage_recorded",
            {
                "bundle_id": self.manager.last_bundle.bundle_id,
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": cache["cache_miss_reasons"],
                "cache_attribution": cache_attribution,
            },
        )

    def diagnostic(self) -> dict[str, Any]:
        bundle = self.manager.last_bundle or self.manager.store.latest_bundle(self.manager.run_id)
        if bundle is None:
            return {
                "bundle_id": None,
                "layer_token_usage": {},
                "included_item_ids": [],
                "excluded_item_ids": [],
                "stale_item_ids": [],
                "summary_item_ids": [],
                "recent_tail_item_ids": [],
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
