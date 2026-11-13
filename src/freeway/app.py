"""OpenAI-compatible HTTP surface."""

from __future__ import annotations

import asyncio
import hmac
import logging
import os
import time
from contextlib import asynccontextmanager

import httpx
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse

from .catalog import Catalog
from .config import Settings, load_settings
from .health import Health
from .router import Exhausted, Router, UnknownModel
from .scores import ScoreBook
from .storage import Store, UsageParser, UsageWriter, identity, named_tokens

log = logging.getLogger("freeway")


def _error(status: int, message: str, kind: str = "router_error", **extra) -> JSONResponse:
    return JSONResponse({"error": {"message": message, "type": kind, **extra}}, status_code=status)


def load_tokens() -> list[str]:
    """Tokens accepted from callers: ROUTER_TOKENS (comma separated) plus the older ROUTER_TOKEN."""
    raw = ",".join(os.environ.get(v, "") for v in ("ROUTER_TOKENS", "ROUTER_TOKEN"))
    return list(dict.fromkeys([*(t.strip() for t in raw.split(",") if t.strip()), *named_tokens().values()]))


def create_app(settings: Settings | None = None, client: httpx.AsyncClient | None = None) -> FastAPI:
    settings = settings or load_settings()
    tokens = load_tokens()
    names = named_tokens()
    owns_client = client is None

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        http = client or httpx.AsyncClient(follow_redirects=True)
        store = Store(settings.database_file)
        store.register(identity(token, names) for token in tokens)
        app.state.store = store
        writer = UsageWriter(store)
        app.state.writer = writer
        scorebook = ScoreBook(settings.scores, http, store) if settings.scores else None
        catalog = Catalog(settings, http, scorebook)
        health = Health()
        app.state.catalog = catalog
        app.state.health = health
        app.state.router = Router(settings, catalog, health, http)
        if not tokens:
            log.warning("ROUTER_TOKENS is not set: the server is open to anyone who can reach it")
        else:
            log.info("%d access token(s) configured", len(tokens))
        await catalog.refresh()

        async def loop():
            while True:
                await asyncio.sleep(settings.refresh_minutes * 60)
                try:
                    await catalog.refresh()
                except Exception:
                    log.exception("catalog refresh failed")

        task = asyncio.create_task(loop())
        try:
            yield
        finally:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            await writer.close()
            await asyncio.to_thread(store.close)
            if owns_client:
                await http.aclose()

    app = FastAPI(title="freeway", lifespan=lifespan)

    @app.middleware("http")
    async def auth(request: Request, call_next):
        given = ""
        if tokens:
            given = request.headers.get("authorization", "")
            given = given[7:] if given.lower().startswith("bearer ") else request.headers.get("x-api-key", "")
            # Compare against every token (no early exit) so timing does not reveal which one was close.
            matches = [hmac.compare_digest(given.encode(), t.encode()) for t in tokens]
            if not any(matches):
                return _error(401, "invalid or missing token", "authentication_error")
        request.state.key = identity(given, names)
        return await call_next(request)

    def tier_names() -> list[str]:
        return [t.name for t in settings.tiers]

    @app.get("/health")
    async def health():
        return {"status": "ok"}

    @app.get("/v1/models")
    async def list_models(request: Request):
        catalog: Catalog = request.app.state.catalog
        now = int(time.time())
        data = [{"id": t, "object": "model", "created": now, "owned_by": "router-tier"} for t in tier_names()]
        data += [
            {
                "id": m.name,
                "object": "model",
                "created": now,
                "owned_by": "router",
                "tier": m.tier,
                "score": m.score,
                "providers": [e.ref for e in m.endpoints],
            }
            for m in sorted(catalog.models.values(), key=lambda m: (-(m.score or -1), m.name))
        ]
        return {"object": "list", "data": data}

    @app.get("/admin/status")
    async def status(request: Request):
        catalog: Catalog = request.app.state.catalog
        h: Health = request.app.state.health
        book = catalog.scorebook
        return {
            "providers": {
                "active": [p.name for p in catalog.active_providers()],
                "skipped": catalog.skipped,
                "discovery_errors": catalog.errors,
            },
            "scores": {
                "source": "artificial_analysis",
                "entries": len(book.table) if book else 0,
                "fetched_at": book.fetched_at if book else None,
                "error": book.error if book else "scores disabled in config",
            },
            # Callable by name only: no benchmark score and no manual rating.
            "unrated": [m.name for m in catalog.unrated()],
            "tiers": {
                t: [
                    {
                        "model": m.name,
                        "score": m.score,
                        "endpoints": [
                            {
                                "ref": e.ref,
                                "cooldown_seconds": round(h.cooldown(e.provider, e.raw_id)),
                                **(
                                    {"ok": s.ok, "failed": s.failed, "last_error": s.last_error, "last_latency": s.last_latency}
                                    if (s := h.stats.get((e.provider, e.raw_id)))
                                    else {}
                                ),
                            }
                            for e in m.endpoints
                        ],
                    }
                    for m in catalog.in_tier(t)
                ]
                for t in tier_names()
            },
        }

    @app.get("/admin/usage")
    async def usage(request: Request):
        await request.app.state.writer.flush()
        return {"data": await asyncio.to_thread(request.app.state.store.summary)}

    @app.post("/admin/refresh")
    async def refresh(request: Request):
        await request.app.state.catalog.refresh()
        return {"models": len(request.app.state.catalog.models)}

    @app.post("/v1/chat/completions")
    async def chat(request: Request):
        try:
            body = await request.json()
        except ValueError:
            return _error(400, "body is not valid JSON", "invalid_request_error")
        if not isinstance(body, dict) or not body.get("model") or not isinstance(body.get("messages"), list):
            return _error(400, "`model` and `messages` are required", "invalid_request_error")

        router: Router = request.app.state.router
        session = request.headers.get("x-router-session")
        fallback = request.headers.get("x-router-fallback", "auto").lower()
        if fallback not in ("auto", "none", "tier"):
            return _error(400, "x-router-fallback must be auto, none or tier", "invalid_request_error")
        if body.get("stream"):
            # Needed so streamed answers still carry token counts when providers support it.
            body.setdefault("stream_options", {"include_usage": True})

        async with request.app.state.writer.slot() as ticket:
            try:
                result = await router.complete(body, fallback=fallback, session=session)
            except UnknownModel as e:
                return _error(
                    404,
                    f"unknown model or tier {str(e)!r}. Tiers: {', '.join(tier_names())}. See GET /v1/models.",
                    "invalid_request_error",
                )
            except Exhausted as e:
                if e.last_body is not None and e.last_status:
                    # Every endpoint rejected the request itself: pass the explanation through.
                    return JSONResponse(e.last_body, status_code=e.last_status)
                tried = [
                    {"provider": a.provider, "model": a.upstream_model, "kind": a.kind, "status": a.status, "error": a.error}
                    for a in e.attempts
                ]
                if not e.plan.candidates and e.plan.skipped:
                    retry = max(1, round(e.plan.retry_in or 1))
                    return JSONResponse(
                        {"error": {"message": "every matching endpoint is cooling down", "type": "all_providers_busy", "retry_in_seconds": retry, "tried": tried}},
                        status_code=503,
                        headers={"retry-after": str(retry)},
                    )
                if not e.plan.candidates:
                    return _error(
                        503, "no endpoint can serve this request (no providers loaded, or none supports the tools/images it needs)", "no_endpoint"
                    )
                return JSONResponse(
                    {"error": {"message": "all providers failed", "type": "all_providers_failed", "tried": tried}}, status_code=503
                )

            key = request.state.key
            provider = result.candidate.endpoint.provider
            model = result.candidate.endpoint.raw_id
            if result.stream is not None:
                async def tracked():
                    parser = UsageParser()
                    try:
                        async for chunk in result.stream:
                            parser.feed(chunk)
                            yield chunk
                    finally:
                        ticket.record(key, provider, model, parser.usage, parser.done and not parser.failed)
                ticket.deferred = True
                return StreamingResponse(tracked(), media_type="text/event-stream", headers={**result.headers, "cache-control": "no-cache"})
            ticket.record(key, provider, model, (result.data or {}).get("usage"))
            return JSONResponse(result.data, headers=result.headers)

    return app
