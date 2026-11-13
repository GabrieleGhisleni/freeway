"""Intelligence scores from Artificial Analysis, held in memory and persisted in SQLite.

A model with no score is *unrated*: it is never picked by a tier, only by name.
"""

from __future__ import annotations

import asyncio
import json
import logging
import math
import sqlite3
import time
from pathlib import Path

import httpx

from . import normalize
from .config import ScoresConfig
from .storage import Store

log = logging.getLogger("freeway.scores")

URL = "https://artificialanalysis.ai/api/v2/language/models/free"


def parse(rows: list[dict]) -> dict[str, float]:
    """family key -> intelligence index.

    One model appears several times (one row per reasoning effort). The tier is
    a floor, so the lowest variant wins.
    """
    table: dict[str, float] = {}
    for r in rows:
        value = (r.get("evaluations") or {}).get("artificial_analysis_intelligence_index")
        if value is None or not r.get("slug"):
            continue
        k = normalize.family_key(r["slug"])
        if k:
            table[k] = min(table.get(k, value), value)
    return table


class ScoreBook:
    def __init__(self, cfg: ScoresConfig, client: httpx.AsyncClient, store: Store | None = None):
        self.store = store or Store(str(Path(cfg.cache_file).with_suffix(".sqlite3")))
        self.cfg = cfg
        self.client = client
        self.table: dict[str, float] = {}
        self.fetched_at: float | None = None
        self.error: str | None = None
        self._load_cache()

    @property
    def cache_path(self) -> Path:
        return Path(self.cfg.cache_file)

    def _load_cache(self) -> None:
        data = None
        try:
            data = self.store.load_scores(self.cfg.cache_file)
        except (ValueError, sqlite3.Error) as e:
            log.warning("could not read score cache: %s", e)
        imported = data is None
        try:
            if imported:
                data = json.loads(self.cache_path.read_text())
            if not isinstance(data, dict) or not isinstance(data.get("table"), dict):
                return
            at = data.get("fetched_at")
            if type(at) not in (int, float) or not math.isfinite(at):
                return
            self.table = {k: v for k, v in data["table"].items() if type(v) in (int, float) and math.isfinite(v)}
            self.fetched_at = at
            if imported:
                self._save_cache()
        except (OSError, ValueError, TypeError):
            pass

    def _save_cache(self) -> None:
        try:
            self.store.save_scores(self.cfg.cache_file, self.table, self.fetched_at)
        except (OSError, sqlite3.Error) as e:
            log.warning("could not write score cache: %s", e)

    def stale(self) -> bool:
        return self.fetched_at is None or time.time() - self.fetched_at > self.cfg.refresh_hours * 3600

    async def ensure_fresh(self) -> None:
        if not self.stale():
            return
        key = self.cfg.api_key
        if not key:
            self.error = f"{self.cfg.api_key_env} is not set" + (" (using cached scores)" if self.table else "")
            return
        try:
            table: dict[str, float] = {}
            page = 1
            while True:
                r = await self.client.get(URL, params={"page": page}, headers={"x-api-key": key}, timeout=30)
                r.raise_for_status()
                body = r.json()
                for name, score in parse(body.get("data", [])).items():
                    table[name] = min(table.get(name, score), score)
                if not (body.get("pagination") or {}).get("has_more") or page >= 20:
                    break
                page += 1
            if not table:
                raise ValueError("no scores in the answer")
            self.table, self.fetched_at, self.error = table, time.time(), None
            await asyncio.to_thread(self._save_cache)
            log.info("scores: %d models rated by Artificial Analysis", len(table))
        except (httpx.HTTPError, ValueError, TypeError, KeyError) as e:
            self.error = f"{type(e).__name__}: {e}"
            log.warning("score refresh failed: %s (keeping %d cached scores)", self.error, len(self.table))

    def lookup(self, names: set[str]) -> float | None:
        found = [self.table[k] for n in names if (k := normalize.family_key(n)) in self.table]
        return min(found) if found else None
