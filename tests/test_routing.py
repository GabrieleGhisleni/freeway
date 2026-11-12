"""End-to-end routing against fake providers served through httpx.MockTransport."""

import json

import httpx
import pytest
from fastapi.testclient import TestClient

from llm_router.app import create_app
from llm_router.config import Settings

OK = {"id": "x", "object": "chat.completion", "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}]}


def settings(**over):
    base = {
        "tiers": [
            {"name": "smart", "min_score": 80, "fallback": ["fast"]},
            {"name": "fast", "min_score": 0},
        ],
        "providers": [
            {"name": "a", "kind": "openai", "base_url": "https://a.test/v1", "api_key_env": "KEY_A", "priority": 1},
            {"name": "b", "kind": "openai", "base_url": "https://b.test/v1", "api_key_env": "KEY_B", "priority": 2},
        ],
        "scores": None,
        "models": {"big-500b": {"score": 90}, "small-1b": {"score": 10}},
    }
    return Settings.model_validate({**base, **over})


class Fake:
    """Scripted providers. `behaviour[(host, model)]` is a callable returning an httpx.Response."""

    def __init__(self, listings, behaviour):
        self.listings, self.behaviour, self.calls = listings, behaviour, []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        host = request.url.host
        if request.url.path.endswith("/models"):
            return httpx.Response(200, json={"data": [{"id": m} for m in self.listings[host]]})
        body = json.loads(request.content)
        self.calls.append((host, body["model"]))
        return self.behaviour.get((host, body["model"]), lambda: httpx.Response(200, json=OK))()


@pytest.fixture(autouse=True)
def keys(monkeypatch):
    monkeypatch.setenv("KEY_A", "ka")
    monkeypatch.setenv("KEY_B", "kb")
    monkeypatch.setenv("ROUTER_TOKEN", "secret")


def client(fake, **over):
    http = httpx.AsyncClient(transport=httpx.MockTransport(fake))
    return TestClient(create_app(settings(**over), http))


AUTH = {"authorization": "Bearer secret"}


def chat(c, model, **kw):
    return c.post("/v1/chat/completions", json={"model": model, "messages": [{"role": "user", "content": "hi"}], **kw}, headers=AUTH)


def test_requires_token():
    fake = Fake({"a.test": ["big-500b"], "b.test": []}, {})
    with client(fake) as c:
        assert c.get("/v1/models").status_code == 401
        assert c.get("/v1/models", headers={"authorization": "Bearer nope"}).status_code == 401
        assert c.get("/v1/models", headers=AUTH).status_code == 200


def test_several_tokens_are_accepted(monkeypatch):
    monkeypatch.setenv("ROUTER_TOKENS", "alice-token, bob-token ,")
    fake = Fake({"a.test": ["big-500b"], "b.test": []}, {})
    with client(fake) as c:
        for ok in ("alice-token", "bob-token", "secret"):  # secret = legacy ROUTER_TOKEN
            assert c.get("/v1/models", headers={"authorization": f"Bearer {ok}"}).status_code == 200
        assert c.get("/v1/models", headers={"x-api-key": "bob-token"}).status_code == 200
        for bad in ("alice", "alice-token-x", ""):
            assert c.get("/v1/models", headers={"authorization": f"Bearer {bad}"}).status_code == 401


def test_same_model_is_merged_across_providers_and_falls_over():
    fake = Fake(
        {"a.test": ["org/big-500b:free"], "b.test": ["big-500b"]},
        {("a.test", "org/big-500b:free"): lambda: httpx.Response(429, json={"error": "slow down"}, headers={"retry-after": "30"})},
    )
    with client(fake) as c:
        models = [m for m in c.get("/v1/models", headers=AUTH).json()["data"] if m["owned_by"] == "router"]
        assert len(models) == 1 and len(models[0]["providers"]) == 2
        r = chat(c, "big-500b")
        assert r.status_code == 200
        assert r.headers["x-router-provider"] == "b"
        assert r.headers["x-router-attempts"] == "2"
        chat(c, "big-500b")
        assert fake.calls == [("a.test", "org/big-500b:free"), ("b.test", "big-500b"), ("b.test", "big-500b")]  # a is cooling down


def test_tier_falls_through_to_lower_tier():
    fake = Fake(
        {"a.test": ["big-500b", "small-1b"], "b.test": []},
        {("a.test", "big-500b"): lambda: httpx.Response(503, json={"error": "down"})},
    )
    with client(fake) as c:
        r = chat(c, "smart")
        assert r.status_code == 200
        assert r.headers["x-router-model"] == "small-1b"
        assert r.headers["x-router-tier"] == "fast"


def test_fallback_none_stays_in_tier():
    fake = Fake(
        {"a.test": ["big-500b", "small-1b"], "b.test": []},
        {("a.test", "big-500b"): lambda: httpx.Response(503, json={"error": "down"})},
    )
    with client(fake) as c:
        r = chat(c, "smart", )  # default: falls through
        assert r.status_code == 200
    fake.calls.clear()
    with client(fake) as c:
        r = c.post(
            "/v1/chat/completions",
            json={"model": "smart", "messages": [{"role": "user", "content": "hi"}]},
            headers={**AUTH, "x-router-fallback": "none"},
        )
        assert r.status_code == 502
        assert r.json()["error"]["type"] == "all_providers_failed"
        assert all(m == "big-500b" for _, m in fake.calls)


def test_named_model_does_not_leave_the_model_by_default():
    fake = Fake(
        {"a.test": ["big-500b", "small-1b"], "b.test": []},
        {("a.test", "big-500b"): lambda: httpx.Response(503, json={"error": "down"})},
    )
    with client(fake) as c:
        assert chat(c, "big-500b").status_code == 502


def test_account_wide_rate_limit_stops_asking_that_provider():
    limited = lambda: httpx.Response(429, json={"error": "free usage limit"}, headers={"retry-after": "30"})
    fake = Fake(
        {"a.test": ["big-500b", "big2-500b", "big3-500b"], "b.test": ["small-1b"]},
        {("a.test", m): limited for m in ("big-500b", "big2-500b", "big3-500b")},
    )
    over = {"models": {"big-500b": {"score": 90}, "big2-500b": {"score": 89}, "big3-500b": {"score": 88}, "small-1b": {"score": 10}}}
    with client(fake, **over) as c:
        r = chat(c, "smart")
        assert r.status_code == 200 and r.headers["x-router-model"] == "small-1b"
        assert fake.calls == [("a.test", "big-500b"), ("a.test", "big2-500b"), ("b.test", "small-1b")]


def test_unknown_model_is_404():
    fake = Fake({"a.test": ["big-500b"], "b.test": []}, {})
    with client(fake) as c:
        r = chat(c, "nope")
        assert r.status_code == 404
        assert "smart" in r.json()["error"]["message"]


def test_bad_request_is_passed_through_and_does_not_cool_down():
    err = {"error": {"message": "context too long"}}
    fake = Fake(
        {"a.test": ["big-500b"], "b.test": ["big-500b"]},
        {(h, "big-500b"): (lambda: httpx.Response(400, json=err)) for h in ("a.test", "b.test")},
    )
    with client(fake) as c:
        r = chat(c, "big-500b")
        assert r.status_code == 400 and r.json() == err
        assert chat(c, "big-500b").status_code == 400
        assert len(fake.calls) == 4  # tried both again: no cooldown for the caller's mistake


def test_error_inside_200_body_triggers_fallback():
    fake = Fake(
        {"a.test": ["big-500b"], "b.test": ["big-500b"]},
        {("a.test", "big-500b"): lambda: httpx.Response(200, json={"error": {"code": 429, "message": "rate limited"}})},
    )
    with client(fake) as c:
        assert chat(c, "big-500b").headers["x-router-provider"] == "b"


def test_all_cooling_down_gives_503_with_retry_after():
    limited = lambda: httpx.Response(429, json={"error": "x"}, headers={"retry-after": "45"})
    fake = Fake({"a.test": ["big-500b"], "b.test": []}, {("a.test", "big-500b"): limited})
    with client(fake) as c:
        assert chat(c, "big-500b").status_code == 502
        r = chat(c, "big-500b")
        assert r.status_code == 503
        assert 1 <= int(r.headers["retry-after"]) <= 45


def test_streaming_switches_provider_before_first_byte():
    sse = b'data: {"choices":[{"delta":{"content":"hi"}}]}\n\ndata: [DONE]\n\n'
    fake = Fake(
        {"a.test": ["big-500b"], "b.test": ["big-500b"]},
        {
            ("a.test", "big-500b"): lambda: httpx.Response(200, content=b'data: {"error":{"code":429,"message":"limit"}}\n\n'),
            ("b.test", "big-500b"): lambda: httpx.Response(200, content=sse, headers={"content-type": "text/event-stream"}),
        },
    )
    with client(fake) as c:
        r = chat(c, "big-500b", stream=True)
        assert r.status_code == 200
        assert r.headers["x-router-provider"] == "b"
        assert b"hi" in r.content


def test_tools_request_skips_endpoints_known_not_to_support_tools():
    from llm_router.catalog import Endpoint
    from llm_router.router import needs

    assert needs({"tools": [{}], "messages": []}) == (True, False)
    img = {"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]}
    assert needs(img) == (False, True)
    assert Endpoint("p", "m", "u", {"m"}).tools is None
