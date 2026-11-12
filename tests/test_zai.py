import json

from freeway.catalog import parse_zai
from freeway.config import ProviderConfig

PRICING = r"""
# Pricing

### Latest Models

| Model | Input | Cached Input | Cached Input Storage | Output |
| :- | :- | :- | :- | :- |
| GLM-5.3 | \$1.4 | \$0.26 | Limited-time Free | \$4.4 |

### Image Generation Models

| Model | Price |
| :- | :- |
| GLM-Image | Free |

> [accordion] Other Pricing
>
> ### Text Models
>
> | Model | Input | Cached Input | Cached Input Storage | Output |
> | :- | :- | :- | :- | :- |
> | GLM-4.7-FlashX | \$0.07 | \$0.01 | Limited-time Free | \$0.4 |
> | GLM-4-32B-0414-128K | \$0.1 | - | - | \$0.1 |
> | GLM-4.7-Flash | Free | Free | Free | Free |
> | GLM-4.5-Flash | Free | Free | Free | Free |
>
> ### Vision Models
>
> | Model | Input | Cached Input | Cached Input Storage | Output |
> | :- | :- | :- | :- | :- |
> | GLM-OCR | \$0.03 | \\ | \\ | \$0.03 |
> | GLM-4.5V | \$0.6 | \$0.11 | Limited-time Free | \$1.8 |
> | GLM-4.6V-Flash | Free | Free | Free | Free |
"""


def provider() -> ProviderConfig:
    return ProviderConfig(name="zai", kind="zai", base_url="https://z.test/v4", api_key_env="K")


def test_only_models_free_in_every_price_column_are_kept():
    eps = parse_zai(provider(), PRICING)
    assert [e.raw_id for e in eps] == ["glm-4.7-flash", "glm-4.5-flash", "glm-4.6v-flash"]


def test_non_token_tables_are_ignored():
    # "GLM-Image | Free" is a per-image price, not a chat model.
    assert "glm-image" not in [e.raw_id for e in parse_zai(provider(), PRICING)]


def test_vision_comes_from_the_section_heading():
    vision = {e.raw_id: e.vision for e in parse_zai(provider(), PRICING)}
    assert vision == {"glm-4.7-flash": False, "glm-4.5-flash": False, "glm-4.6v-flash": True}


def test_unrecognised_page_yields_nothing():
    assert parse_zai(provider(), "<html>moved</html>") == []


def test_probe_keeps_only_models_that_answer_a_chat():
    import asyncio

    import httpx

    from freeway.catalog import Catalog
    from freeway.config import Settings

    asked = []

    def fake(request: httpx.Request) -> httpx.Response:
        model = json.loads(request.content)["model"]
        asked.append(model)
        status = {"good": 200, "gone": 404, "not-chat": 400, "flaky": 429}[model]
        return httpx.Response(status, json={"choices": []})

    p = ProviderConfig(name="p", base_url="https://p.test/v1", api_key_env="K", probe=True)
    settings = Settings(providers=[p], tiers=[{"name": "fast", "min_score": 0}])

    async def run():
        async with httpx.AsyncClient(transport=httpx.MockTransport(fake)) as client:
            cat = Catalog(settings, client)
            eps = [parse_ep(p, m) for m in ("good", "gone", "not-chat", "flaky")]
            first = await cat._probe(p, eps)
            again = await cat._probe(p, eps)
            return first, again

    first, again = asyncio.run(run())
    assert [e.raw_id for e in first] == ["good"]  # 404/400 are final; a 429 proves nothing
    assert [e.raw_id for e in again] == ["good"]
    assert len(asked) == 4  # the second pass reused every verdict


def parse_ep(p, raw_id):
    from freeway import normalize
    from freeway.catalog import Endpoint

    return Endpoint(provider=p.name, raw_id=raw_id, base_url=p.base_url, keys=normalize.keys(raw_id))
