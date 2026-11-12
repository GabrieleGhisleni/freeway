import json

import httpx
import pytest

from freeway import scores
from freeway.catalog import Endpoint, reconcile
from freeway.config import ScoresConfig, Settings


def row(slug, value):
    return {"slug": slug, "evaluations": {"artificial_analysis_intelligence_index": value}}


def settings(**over):
    base = {
        "tiers": [{"name": "smart", "min_score": 30}, {"name": "fast", "min_score": 0}],
        "providers": [],
    }
    return Settings.model_validate({**base, **over})


def book(tmp_path, rows):
    b = scores.ScoreBook(ScoresConfig(cache_file=str(tmp_path / "s.json")), httpx.AsyncClient())
    b.table = scores.parse(rows)
    return b


def ep(raw):
    from freeway import normalize

    return Endpoint("p", raw, "https://p.test/v1", normalize.keys(raw))


def test_parse_takes_lowest_variant_and_skips_nulls():
    table = scores.parse([row("model-x-high", 40), row("model-x-low", 31), row("model-y", None), {"slug": None}])
    assert table == {"modelx": 31}


def test_unrated_models_belong_to_no_tier(tmp_path):
    b = book(tmp_path, [row("known-model", 35)])
    models = reconcile([ep("known-model:free"), ep("mystery-model")], settings(), b)
    assert models["known-model"].tier == "smart" and models["known-model"].score == 35
    assert models["mystery-model"].tier is None and models["mystery-model"].score is None


def test_manual_score_rates_an_unrated_model(tmp_path):
    b = book(tmp_path, [])
    models = reconcile([ep("mystery-model")], settings(models={"mystery-model": {"score": 5}}), b)
    assert models["mystery-model"].tier == "fast"


def test_manual_override_beats_the_benchmark(tmp_path):
    b = book(tmp_path, [row("known-model", 35)])
    models = reconcile([ep("known-model")], settings(models={"known-model": {"tier": "fast"}}), b)
    assert models["known-model"].tier == "fast"


@pytest.mark.parametrize("pages", [1, 3])
async def test_fetch_follows_pagination_and_writes_cache(tmp_path, monkeypatch, pages):
    monkeypatch.setenv("ARTIFICIAL_ANALYSIS_API_KEY", "k")
    seen = []

    def handler(request: httpx.Request) -> httpx.Response:
        page = int(request.url.params["page"])
        seen.append((page, request.headers["x-api-key"]))
        return httpx.Response(
            200, json={"pagination": {"has_more": page < pages}, "data": [row(f"model-{page}", 10 * page)]}
        )

    cfg = ScoresConfig(cache_file=str(tmp_path / "cache.json"))
    b = scores.ScoreBook(cfg, httpx.AsyncClient(transport=httpx.MockTransport(handler)))
    await b.ensure_fresh()
    assert len(seen) == pages and all(k == "k" for _, k in seen)
    assert len(b.table) == pages
    # a fresh instance starts from the cache without calling the API
    again = scores.ScoreBook(cfg, httpx.AsyncClient(transport=httpx.MockTransport(lambda r: pytest.fail("no call"))))
    assert again.table == b.table and not again.stale()


async def test_failed_refresh_keeps_cached_scores(tmp_path, monkeypatch):
    monkeypatch.setenv("ARTIFICIAL_ANALYSIS_API_KEY", "k")
    cache = tmp_path / "cache.json"
    cache.write_text(json.dumps({"fetched_at": 1.0, "table": {"modelx": 33.0}}))
    cfg = ScoresConfig(cache_file=str(cache))
    b = scores.ScoreBook(cfg, httpx.AsyncClient(transport=httpx.MockTransport(lambda r: httpx.Response(500))))
    assert b.stale()
    await b.ensure_fresh()
    assert b.table == {"modelx": 33.0} and b.error
