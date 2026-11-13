from __future__ import annotations

import os
from pathlib import Path
from typing import Literal

import yaml
from pydantic import BaseModel, Field, model_validator


class ProviderConfig(BaseModel):
    name: str
    # How the model list is read:
    #   openrouter - public listing, keeps only models priced at 0
    #   opencode   - OpenCode Zen listing, keeps only models priced at 0
    #   openai     - plain GET {base_url}/models; every listed model counts as free
    #   zai        - Z.ai pricing page (models_url, Markdown); keeps models whose prices are all "Free"
    kind: Literal["openrouter", "opencode", "openai", "zai"] = "openai"
    base_url: str
    models_url: str | None = None
    api_key_env: str
    # Lower is tried first when the same model is served by several providers.
    priority: int = 100
    include: list[str] = Field(default_factory=list)  # globs on the provider's model id
    exclude: list[str] = Field(default_factory=list)
    models: list[str] = Field(default_factory=list)  # static list; skips discovery
    # For listings that include dead or non-chat models (NVIDIA lists 83, about 10 answer):
    # send each listed model a 1-token chat and keep only those that answer.
    probe: bool = False
    headers: dict[str, str] = Field(default_factory=dict)
    enabled: bool = True

    @property
    def api_key(self) -> str | None:
        return os.environ.get(self.api_key_env) or None


class TierConfig(BaseModel):
    name: str
    min_score: float
    # Tiers tried, in order, when nothing in this tier can answer.
    fallback: list[str] = Field(default_factory=list)


class ModelOverride(BaseModel):
    tier: str | None = None
    score: float | None = None
    disabled: bool = False


class ScoresConfig(BaseModel):
    api_key_env: str = "ARTIFICIAL_ANALYSIS_API_KEY"
    refresh_hours: float = 24
    cache_file: str = "data/scores.json"

    @property
    def api_key(self) -> str | None:
        return os.environ.get(self.api_key_env) or None


class Settings(BaseModel):
    database_file: str = "data/freeway.sqlite3"
    host: str = "127.0.0.1"
    port: int = 4100
    refresh_minutes: float = 30
    connect_timeout: float = 10
    request_timeout: float = 120  # non-streaming: whole response
    first_token_timeout: float = 30  # streaming: time allowed before the first chunk
    stream_idle_timeout: float = 90  # streaming: max silence between chunks
    max_attempts: int = 8
    session_ttl_minutes: float = 120
    tiers: list[TierConfig]
    providers: list[ProviderConfig]
    scores: ScoresConfig | None = Field(default_factory=ScoresConfig)
    # canonical name -> other names that must be treated as the same model
    aliases: dict[str, list[str]] = Field(default_factory=dict)
    # canonical name (or any alias) -> manual tier/score/disable
    models: dict[str, ModelOverride] = Field(default_factory=dict)
    exclude: list[str] = Field(default_factory=list)  # globs on canonical names

    @model_validator(mode="after")
    def _check(self) -> Settings:
        names = [t.name for t in self.tiers]
        if len(set(names)) != len(names):
            raise ValueError("duplicate tier names")
        for t in self.tiers:
            for f in t.fallback:
                if f not in names:
                    raise ValueError(f"tier {t.name!r} falls back to unknown tier {f!r}")
        for key, ov in self.models.items():
            if ov.tier and ov.tier not in names:
                raise ValueError(f"model override {key!r} uses unknown tier {ov.tier!r}")
        pnames = [p.name for p in self.providers]
        if len(set(pnames)) != len(pnames):
            raise ValueError("duplicate provider names")
        return self

    def tier(self, name: str) -> TierConfig | None:
        return next((t for t in self.tiers if t.name == name), None)

    def tier_for_score(self, score: float) -> str | None:
        ranked = sorted(self.tiers, key=lambda t: t.min_score, reverse=True)
        return next((t.name for t in ranked if score >= t.min_score), None)


def load_settings(path: str | Path | None = None) -> Settings:
    path = Path(path or os.environ.get("ROUTER_CONFIG") or "config.yaml")
    with path.open() as f:
        return Settings.model_validate(yaml.safe_load(f))
