"""Intelligence scores from Artificial Analysis, cached on disk.

A model with no score is *unrated*: it is never picked by a tier, only by name.
"""

from __future__ import annotations

import json
import logging
import time
from pathlib import Path

import httpx

from . import normalize
from .config import ScoresConfig

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
    def __init__(self, cfg: ScoresConfig, client: httpx.AsyncClient):
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
        try:
            data = json.loads(self.cache_path.read_text())
            self.table, self.fetched_at = data["table"], data["fetched_at"]
        except (OSError, ValueError, KeyError):
            pass

    def _save_cache(self) -> None:
        try:
            self.cache_path.parent.mkdir(parents=True, exist_ok=True)
            self.cache_path.write_text(json.dumps({"fetched_at": self.fetched_at, "table": self.table}))
        except OSError as e:
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
            rows: list[dict] = []
            page = 1
            while True:
                r = await self.client.get(URL, params={"page": page}, headers={"x-api-key": key}, timeout=30)
                r.raise_for_status()
                body = r.json()
                rows += body.get("data", [])
                if not (body.get("pagination") or {}).get("has_more") or page >= 20:
                    break
                page += 1
            table = parse(rows)
            if not table:
                raise ValueError("no scores in the answer")
            self.table, self.fetched_at, self.error = table, time.time(), None
            self._save_cache()
            log.info("scores: %d models rated by Artificial Analysis", len(table))
        except Exception as e:  # keep serving with whatever we had
            self.error = f"{type(e).__name__}: {e}"
            log.warning("score refresh failed: %s (keeping %d cached scores)", self.error, len(self.table))

    def lookup(self, names: set[str]) -> float | None:
        found = [self.table[k] for n in names if (k := normalize.family_key(n)) in self.table]
        return min(found) if found else None
