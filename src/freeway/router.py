"""Picking which provider endpoints to try for a request, and trying them in order."""

from __future__ import annotations

import asyncio
import json
import logging
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass

import httpx

from .catalog import Catalog, Endpoint, Model
from .config import Settings
from .health import Health

log = logging.getLogger("freeway")


@dataclass
class Candidate:
    model: Model
    endpoint: Endpoint


@dataclass
class Attempt:
    provider: str
    upstream_model: str
    model: str
    kind: str
    status: int | None
    error: str
    cooldown_seconds: float


@dataclass
class Plan:
    candidates: list[Candidate]
    retry_in: float | None = None  # soonest cooldown expiry among skipped endpoints
    skipped: int = 0


@dataclass
class Success:
    candidate: Candidate
    attempts: list[Attempt]
    data: dict | None = None  # non-streaming answer
    stream: AsyncIterator[bytes] | None = None

    @property
    def headers(self) -> dict[str, str]:
        c = self.candidate
        return {
            "x-router-model": c.model.name,
            "x-router-tier": c.model.tier or "unrated",
            "x-router-provider": c.endpoint.provider,
            "x-router-upstream-model": c.endpoint.raw_id,
            "x-router-attempts": str(len(self.attempts) + 1),
        }


class UnknownModel(Exception):
    pass


@dataclass
class Exhausted(Exception):
    attempts: list[Attempt]
    plan: Plan
    last_body: object = None  # upstream body of the last failure, when it was the request's fault
    last_status: int | None = None


@dataclass
class UpstreamFailure(Exception):
    kind: str
    status: int | None
    message: str
    retry_after: float | None = None
    body: object = None


def classify(status: int) -> str:
    if status == 429:
        return "rate_limit"
    if status == 401:
        return "auth"
    if status in (402, 403, 404):
        return "unavailable"
    if status == 408 or status >= 500:
        return "server"
    return "bad_request"


def _seconds_from(value) -> float | None:
    """Read a Retry-After style value: delta seconds, or an epoch in s or ms."""
    try:
        n = float(value)
    except (TypeError, ValueError):
        return None
    if n > 1e12:
        n = n / 1000 - time.time()
    elif n > 1e9:
        n = n - time.time()
    return n if n > 0 else None


def _retry_after(headers, body) -> float | None:
    found = _seconds_from(headers.get("retry-after")) or _seconds_from(headers.get("x-ratelimit-reset"))
    if found is None and isinstance(body, dict):
        err = body.get("error")
        meta = (err.get("metadata") or {}) if isinstance(err, dict) else {}
        hdrs = meta.get("headers") or {}
        found = _seconds_from(hdrs.get("Retry-After")) or _seconds_from(hdrs.get("X-RateLimit-Reset"))
    return found


def _error_text(body, fallback: str) -> str:
    if isinstance(body, dict):
        err = body.get("error", body)
        if isinstance(err, dict):
            return str(err.get("message") or err)
        return str(err)
    return fallback


def _failure_from_error_body(body: dict, headers) -> UpstreamFailure:
    """Some providers answer 200 and put the real status inside `error.code`."""
    err = body.get("error")
    code = err.get("code") if isinstance(err, dict) else None
    kind = classify(code) if isinstance(code, int) and code >= 400 else "server"
    return UpstreamFailure(kind, code if isinstance(code, int) else 200, _error_text(body, "error"), _retry_after(headers, body), body)


def needs(body: dict) -> tuple[bool, bool]:
    """(needs tool calling, needs image input) for an OpenAI chat request."""
    tools = bool(body.get("tools"))
    vision = False
    for msg in body.get("messages") or []:
        content = msg.get("content") if isinstance(msg, dict) else None
        if isinstance(content, list):
            vision = vision or any(isinstance(p, dict) and p.get("type") in ("image_url", "input_image") for p in content)
    return tools, vision


def _first_event(buf: bytes) -> tuple[str, object] | None:
    """Look at the complete SSE lines received so far.

    Returns None while no `data:` line has fully arrived, else
    ("ok" | "empty" | "error", payload).
    """
    lines = buf.split(b"\n")[:-1]
    for line in lines:
        line = line.strip()
        if not line.startswith(b"data:"):
            continue
        payload = line[5:].strip()
        if payload == b"[DONE]":
            return "empty", None
        try:
            obj = json.loads(payload)
        except ValueError:
            return "ok", None
        if isinstance(obj, dict) and obj.get("error"):
            return "error", obj
        return "ok", None
    return None


class Router:
    def __init__(self, settings: Settings, catalog: Catalog, health: Health, client: httpx.AsyncClient):
        self.settings = settings
        self.catalog = catalog
        self.health = health
        self.client = client
        self._sessions: dict[str, tuple[str, float]] = {}

    # ---- choosing -------------------------------------------------------

    def _chain(self, tier: str, with_fallback: bool) -> list[Model]:
        cfg = self.settings.tier(tier)
        tiers = [tier, *(cfg.fallback if cfg and with_fallback else [])]
        return [m for t in tiers for m in self.catalog.in_tier(t)]

    def _sticky(self, session: str | None) -> str | None:
        if not session:
            return None
        entry = self._sessions.get(session)
        if not entry:
            return None
        name, expires = entry
        if expires < time.time():
            self._sessions.pop(session, None)
            return None
        return name

    def _remember(self, session: str | None, model: Model) -> None:
        if not session:
            return
        now = time.time()
        if len(self._sessions) >= 5000:
            self._sessions = {k: v for k, v in self._sessions.items() if v[1] > now}
        if session not in self._sessions and len(self._sessions) >= 5000:
            oldest = min(self._sessions, key=lambda k: self._sessions[k][1])
            del self._sessions[oldest]
        self._sessions[session] = (model.name, now + self.settings.session_ttl_minutes * 60)

    def plan(
        self,
        requested: str,
        *,
        fallback: str = "auto",
        session: str | None = None,
        needs_tools: bool = False,
        needs_vision: bool = False,
    ) -> Plan:
        """Ordered endpoints to try for `requested`.

        `requested` is a tier name, a model name (any spelling the catalog
        knows), or `<provider>/<provider model id>` to pin one endpoint.
        fallback: "auto" (tiers fall through their chain, named models do not),
        "none" (never leave the requested tier/model), "tier" (a named model
        falls back to the rest of its tier chain).
        """
        pairs: list[tuple[Model, Endpoint]] = []
        if self.settings.tier(requested):
            models = self._chain(requested, fallback != "none")
            sticky = self._sticky(session)
            # A conversation keeps the model that answered it last time, while it is usable.
            models.sort(key=lambda m: m.name != sticky)
            pairs = [(m, e) for m in models for e in m.endpoints]
        else:
            pinned = self.catalog.find_endpoint(requested)
            model = pinned[0] if pinned else self.catalog.find_model(requested)
            if not model:
                raise UnknownModel(requested)
            if pinned:
                pairs = [pinned]
                if fallback == "tier":
                    pairs += [(model, e) for e in model.endpoints if e is not pinned[1]]
            else:
                pairs = [(model, e) for e in model.endpoints]
            if fallback == "tier" and model.tier:  # an unrated model has no tier to fall back through
                pairs += [(m, e) for m in self._chain(model.tier, True) if m is not model for e in m.endpoints]

        plan = Plan(candidates=[])
        for model, ep in pairs:
            if (needs_tools and ep.tools is False) or (needs_vision and ep.vision is False):
                continue
            wait = self.health.cooldown(ep.provider, ep.raw_id)
            if wait > 0:
                plan.skipped += 1
                plan.retry_in = wait if plan.retry_in is None else min(plan.retry_in, wait)
                continue
            plan.candidates.append(Candidate(model, ep))
        return plan

    # ---- calling --------------------------------------------------------

    def _request(self, cand: Candidate, body: dict) -> tuple[str, dict, dict]:
        provider = next(p for p in self.settings.providers if p.name == cand.endpoint.provider)
        url = cand.endpoint.base_url.rstrip("/") + "/chat/completions"
        headers = {"Authorization": f"Bearer {provider.api_key}", **provider.headers}
        return url, headers, {**body, "model": cand.endpoint.raw_id}

    async def _call_json(self, cand: Candidate, body: dict) -> dict:
        url, headers, payload = self._request(cand, body)
        timeout = httpx.Timeout(self.settings.request_timeout, connect=self.settings.connect_timeout)
        try:
            r = await self.client.post(url, json=payload, headers=headers, timeout=timeout)
        except httpx.TimeoutException as e:
            raise UpstreamFailure("server", None, f"timeout ({type(e).__name__})") from e
        except httpx.HTTPError as e:
            raise UpstreamFailure("server", None, f"network error: {e!r}") from e
        try:
            data = r.json()
        except ValueError:
            data = None
        if r.status_code != 200:
            raise UpstreamFailure(
                classify(r.status_code), r.status_code, _error_text(data, r.text[:300]), _retry_after(r.headers, data), data
            )
        if not isinstance(data, dict):
            raise UpstreamFailure("server", 200, "answer is not JSON")
        if data.get("error"):
            raise _failure_from_error_body(data, r.headers)
        if not data.get("choices"):
            raise UpstreamFailure("server", 200, "answer has no choices")
        return data

    async def _call_stream(self, cand: Candidate, body: dict) -> AsyncIterator[bytes]:
        """Open the stream and wait for the first real chunk before committing.

        Once bytes have gone to the caller there is no switching provider, so
        everything that can be detected early is detected here.
        """
        url, headers, payload = self._request(cand, body)
        timeout = httpx.Timeout(self.settings.stream_idle_timeout, connect=self.settings.connect_timeout)
        req = self.client.build_request("POST", url, json=payload, headers=headers, timeout=timeout)
        resp: httpx.Response | None = None
        buf = b""
        try:
            async with asyncio.timeout(self.settings.first_token_timeout):
                resp = await self.client.send(req, stream=True)
                if resp.status_code != 200:
                    raw = await resp.aread()
                    try:
                        data = json.loads(raw)
                    except ValueError:
                        data = None
                    raise UpstreamFailure(
                        classify(resp.status_code),
                        resp.status_code,
                        _error_text(data, raw[:300].decode(errors="replace")),
                        _retry_after(resp.headers, data),
                        data,
                    )
                chunks = resp.aiter_bytes()
                verdict = None
                async for chunk in chunks:
                    if len(buf) + len(chunk) > 1048576:
                        raise UpstreamFailure("server", 200, "stream preamble exceeds 1 MiB")
                    buf += chunk
                    verdict = _first_event(buf)
                    if verdict:
                        break
            if verdict is None:  # stream ended without a single data line
                try:
                    data = json.loads(buf)
                except ValueError:
                    data = None
                if isinstance(data, dict) and data.get("error"):
                    raise _failure_from_error_body(data, resp.headers)
                raise UpstreamFailure("server", 200, "stream ended before any data")
            if verdict[0] == "error":
                raise _failure_from_error_body(verdict[1], resp.headers)
            if verdict[0] == "empty":
                raise UpstreamFailure("server", 200, "stream finished with no content")
        except TimeoutError as e:
            if resp is not None:
                await resp.aclose()
            raise UpstreamFailure("server", None, f"no first token within {self.settings.first_token_timeout:g}s") from e
        except httpx.HTTPError as e:
            if resp is not None:
                await resp.aclose()
            raise UpstreamFailure("server", None, f"network error: {e!r}") from e
        except UpstreamFailure:
            if resp is not None:
                await resp.aclose()
            raise

        ep = cand.endpoint

        async def relay() -> AsyncIterator[bytes]:
            try:
                yield buf
                async for chunk in chunks:
                    yield chunk
            except httpx.HTTPError as e:
                self.health.fail(ep.provider, ep.raw_id, "server", f"stream broke: {e!r}")
                log.warning("stream from %s broke mid-answer: %r", ep.ref, e)
                err = {"error": {"message": f"upstream stream interrupted ({ep.ref})", "type": "upstream_error"}}
                yield b"data: " + json.dumps(err).encode() + b"\n\n"
            finally:
                await resp.aclose()

        return relay()

    async def complete(self, body: dict, *, fallback: str = "auto", session: str | None = None) -> Success:
        requested = str(body.get("model") or "")
        stream = bool(body.get("stream"))
        needs_tools, needs_vision = needs(body)
        plan = self.plan(requested, fallback=fallback, session=session, needs_tools=needs_tools, needs_vision=needs_vision)
        attempts: list[Attempt] = []
        last: UpstreamFailure | None = None
        rate_limited: dict[str, int] = {}
        for cand in plan.candidates[: self.settings.max_attempts]:
            ep = cand.endpoint
            if self.health.cooldown(ep.provider, ep.raw_id) > 0:
                continue  # paused earlier in this very request (provider-wide limit)
            started = time.monotonic()
            try:
                if stream:
                    result = Success(cand, attempts, stream=await self._call_stream(cand, body))
                else:
                    result = Success(cand, attempts, data=await self._call_json(cand, body))
            except UpstreamFailure as f:
                last = f
                cooldown = self.health.fail(ep.provider, ep.raw_id, f.kind, f.message, f.retry_after)
                if f.kind == "rate_limit":
                    # Two different models limited in a row: the limit is the account's, so stop asking this provider.
                    rate_limited[ep.provider] = rate_limited.get(ep.provider, 0) + 1
                    if rate_limited[ep.provider] >= 2:
                        self.health.cool_provider(ep.provider, cooldown)
                attempts.append(Attempt(ep.provider, ep.raw_id, cand.model.name, f.kind, f.status, f.message[:300], round(cooldown)))
                log.info(
                    "%s -> %s failed: %s %s (%s)%s",
                    requested, ep.ref, f.kind, f.status or "-", f.message[:120],
                    f", cooling down {cooldown:.0f}s" if cooldown else "",
                )
                continue
            elapsed = time.monotonic() - started
            self.health.ok(ep.provider, ep.raw_id, elapsed)
            self._remember(session, cand.model)
            log.info(
                "%s -> %s [%s, tier %s] ok in %.1fs after %d failed attempt(s)",
                requested, ep.ref, cand.model.name, cand.model.tier or "unrated", elapsed, len(attempts),
            )
            return result
        client_fault = bool(attempts) and all(a.kind == "bad_request" for a in attempts)
        raise Exhausted(
            attempts,
            plan,
            last_body=last.body if last and client_fault else None,
            last_status=last.status if last and client_fault else None,
        )
