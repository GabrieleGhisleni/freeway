"""Discovery of free models on each provider and reconciliation into one catalog."""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from fnmatch import fnmatch

import httpx

from . import normalize
from .config import ProviderConfig, Settings
from .scores import ScoreBook

log = logging.getLogger("llm_router.catalog")

# Listed by generic providers next to chat models, but not chat models.
_NON_CHAT = ["*whisper*", "*tts*", "*guard*", "*embed*", "*rerank*", "*moderation*", "*speech*"]


@dataclass
class Endpoint:
    """One model as served by one provider."""

    provider: str
    raw_id: str  # what the provider wants in the `model` field
    base_url: str
    keys: set[str]
    priority: int = 100
    context: int | None = None
    tools: bool | None = None  # None = provider does not say
    vision: bool | None = None

    @property
    def ref(self) -> str:
        return f"{self.provider}/{self.raw_id}"


@dataclass
class Model:
    """The same model across every provider that serves it."""

    name: str
    keys: set[str]
    score: float | None  # None = unrated: never chosen through a tier
    tier: str | None
    endpoints: list[Endpoint] = field(default_factory=list)


def _wanted(p: ProviderConfig, raw_id: str) -> bool:
    if p.include and not any(fnmatch(raw_id, g) for g in p.include):
        return False
    return not any(fnmatch(raw_id.lower(), g) for g in [*p.exclude, *_NON_CHAT])


def _is_zero(value) -> bool:
    try:
        return float(value) == 0
    except (TypeError, ValueError):
        return False


def parse_openrouter(p: ProviderConfig, payload: dict) -> list[Endpoint]:
    out = []
    for m in payload.get("data", []):
        pricing = m.get("pricing") or {}
        if not (_is_zero(pricing.get("prompt")) and _is_zero(pricing.get("completion"))):
            continue
        arch = m.get("architecture") or {}
        if (arch.get("output_modalities") or ["text"]) != ["text"]:
            continue  # image / audio / music generators are not chat models
        params = m.get("supported_parameters")
        out.append(
            Endpoint(
                provider=p.name,
                raw_id=m["id"],
                base_url=p.base_url,
                keys=normalize.keys(m["id"], m.get("canonical_slug"), m.get("hugging_face_id")),
                priority=p.priority,
                context=m.get("context_length"),
                tools=("tools" in params) if isinstance(params, list) else None,
                vision="image" in (arch.get("input_modalities") or []),
            )
        )
    return out


def parse_opencode(p: ProviderConfig, payload: dict) -> list[Endpoint]:
    out = []
    for m in payload.get("data", []):
        cost = (m.get("cost") or {}).get("text") or {}
        if not (
            _is_zero((cost.get("input") or {}).get("price"))
            and _is_zero((cost.get("output") or {}).get("price"))
        ):
            continue
        if m.get("type", "chat") != "chat":
            continue
        api = m.get("api")
        chat = (api or {}).get("chat-completions")
        if api and not chat:
            continue  # only reachable through a non-OpenAI wire format
        caps = m.get("capabilities") or {}
        tools = (caps.get("tools") or {}).get("supported")
        inputs = (m.get("modalities") or {}).get("input") or []
        out.append(
            Endpoint(
                provider=p.name,
                raw_id=m["id"],
                base_url=(chat or {}).get("base_url") or p.base_url,
                keys=normalize.keys(m["id"], m.get("canonical_id")),
                priority=p.priority,
                context=(m.get("limit") or {}).get("context"),
                tools=tools if isinstance(tools, bool) else None,
                vision=any(str(i).startswith("image") for i in inputs),
            )
        )
    return out


def parse_openai(p: ProviderConfig, payload: dict) -> list[Endpoint]:
    out = []
    for m in payload.get("data", []):
        if not m.get("id") or not m.get("active", True):
            continue
        # Google lists "models/gemini-..." but wants the bare name in requests.
        raw_id = m["id"].removeprefix("models/")
        out.append(
            Endpoint(
                provider=p.name,
                raw_id=raw_id,
                base_url=p.base_url,
                keys=normalize.keys(raw_id),
                priority=p.priority,
                context=m.get("context_window") or m.get("context_length"),
            )
        )
    return out


_PARSERS = {"openrouter": parse_openrouter, "opencode": parse_opencode, "openai": parse_openai}


def reconcile(endpoints: list[Endpoint], settings: Settings, scorebook: ScoreBook | None = None) -> dict[str, Model]:
    """Group endpoints that share a key into models, then rate each model.

    A score comes from the benchmark table or from `models:` in the config.
    With neither the model is unrated and belongs to no tier.
    """
    parent: dict[str, str] = {}

    def find(k: str) -> str:
        parent.setdefault(k, k)
        while parent[k] != k:
            parent[k] = parent[parent[k]]
            k = parent[k]
        return k

    def union(a: str, b: str) -> None:
        parent[find(a)] = find(b)

    forced_name: dict[str, str] = {}  # any alias key -> display name
    for name, others in settings.aliases.items():
        group = [normalize.key(name), *(normalize.key(o) for o in others)]
        group = [g for g in group if g]
        for g in group:
            union(group[0], g)
            forced_name[g] = name

    for ep in endpoints:
        first, *rest = sorted(ep.keys)
        find(first)
        for k in rest:
            union(first, k)

    groups: dict[str, list[Endpoint]] = {}
    for ep in endpoints:
        groups.setdefault(find(next(iter(ep.keys))), []).append(ep)

    overrides = {normalize.key(k): v for k, v in settings.models.items()}
    models: dict[str, Model] = {}
    for eps in groups.values():
        all_keys = set().union(*(e.keys for e in eps))
        name = next((forced_name[k] for k in sorted(all_keys) if k in forced_name), None)
        name = name or min(all_keys, key=lambda k: (len(k), k))
        if any(fnmatch(k, g) for k in all_keys | {name} for g in settings.exclude):
            continue
        override = next((overrides[k] for k in sorted(all_keys) if k in overrides), None)
        if override and override.disabled:
            continue
        score = scorebook.lookup(all_keys | {name}) if scorebook else None
        tier = settings.tier_for_score(score) if score is not None else None
        if override:
            if override.score is not None:
                score = override.score
                tier = settings.tier_for_score(score)
            if override.tier:
                tier = override.tier
        eps.sort(key=lambda e: (e.priority, e.provider, e.raw_id))
        models[name] = Model(name=name, keys=all_keys | {normalize.key(name)}, score=score, tier=tier, endpoints=eps)
    return models


class Catalog:
    def __init__(self, settings: Settings, client: httpx.AsyncClient, scorebook: ScoreBook | None = None):
        self.settings = settings
        self.client = client
        self.scorebook = scorebook
        self.models: dict[str, Model] = {}
        self._by_key: dict[str, Model] = {}
        self._by_provider: dict[str, list[Endpoint]] = {}
        self.errors: dict[str, str] = {}  # provider -> last discovery error
        self.skipped: dict[str, str] = {}  # provider -> why it is not in use
        self._lock = asyncio.Lock()

    def active_providers(self) -> list[ProviderConfig]:
        self.skipped = {}
        active = []
        for p in self.settings.providers:
            if not p.enabled:
                self.skipped[p.name] = "disabled in config"
            elif not p.api_key:
                self.skipped[p.name] = f"{p.api_key_env} is not set"
            else:
                active.append(p)
        return active

    async def _discover(self, p: ProviderConfig) -> list[Endpoint]:
        if p.models:
            eps = [
                Endpoint(provider=p.name, raw_id=m, base_url=p.base_url, keys=normalize.keys(m), priority=p.priority)
                for m in p.models
            ]
        else:
            url = p.models_url or p.base_url.rstrip("/") + "/models"
            headers = {"Authorization": f"Bearer {p.api_key}", **p.headers}
            r = await self.client.get(url, headers=headers, timeout=20)
            r.raise_for_status()
            eps = _PARSERS[p.kind](p, r.json())
        return [e for e in eps if e.keys and _wanted(p, e.raw_id)]

    async def refresh(self) -> None:
        async with self._lock:
            if self.scorebook:
                await self.scorebook.ensure_fresh()
            providers = self.active_providers()
            results = await asyncio.gather(*(self._discover(p) for p in providers), return_exceptions=True)
            by_provider: dict[str, list[Endpoint]] = {}
            for p, res in zip(providers, results):
                if isinstance(res, BaseException):
                    self.errors[p.name] = f"{type(res).__name__}: {res}"
                    log.warning("discovery failed for %s: %s (keeping previous list)", p.name, res)
                    by_provider[p.name] = self._by_provider.get(p.name, [])
                else:
                    self.errors.pop(p.name, None)
                    by_provider[p.name] = res
            self._by_provider = by_provider
            self.models = reconcile([e for eps in by_provider.values() for e in eps], self.settings, self.scorebook)
            self._by_key = {k: m for m in self.models.values() for k in m.keys}
            log.info(
                "catalog: %d models from %d providers (%s)",
                len(self.models),
                len(providers),
                ", ".join(f"{n}={len(e)}" for n, e in by_provider.items()) or "none",
            )

    def find_model(self, requested: str) -> Model | None:
        return self.models.get(requested) or self._by_key.get(normalize.key(requested))

    def find_endpoint(self, requested: str) -> tuple[Model, Endpoint] | None:
        """Match `<provider>/<provider model id>` exactly."""
        for m in self.models.values():
            for e in m.endpoints:
                if e.ref == requested:
                    return m, e
        return None

    def in_tier(self, tier: str) -> list[Model]:
        found = [m for m in self.models.values() if m.tier == tier]
        return sorted(found, key=lambda m: (-(m.score or 0), m.name))

    def unrated(self) -> list[Model]:
        return sorted((m for m in self.models.values() if m.tier is None), key=lambda m: m.name)
