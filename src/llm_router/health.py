"""Cooldowns: which provider endpoints are currently not worth trying."""

from __future__ import annotations

import time
from dataclasses import dataclass

# failure kind -> (base seconds, cap seconds, doubles on repeat, whole provider)
_POLICY = {
    "rate_limit": (60, 900, True, False),
    "auth": (600, 600, False, True),
    "unavailable": (1800, 1800, False, False),  # 402 / 403 / 404
    "server": (20, 600, True, False),  # 5xx, timeouts, network, malformed answers
    "bad_request": (0, 0, False, False),  # the request's fault, not the endpoint's
}
_MAX_RETRY_AFTER = 6 * 3600


@dataclass
class Stat:
    ok: int = 0
    failed: int = 0
    strikes: int = 0  # consecutive failures
    last_error: str | None = None
    last_latency: float | None = None


class Health:
    def __init__(self, clock=time.time):
        self._clock = clock
        self._until: dict[tuple[str, str | None], float] = {}
        self.stats: dict[tuple[str, str], Stat] = {}

    def _stat(self, provider: str, raw_id: str) -> Stat:
        return self.stats.setdefault((provider, raw_id), Stat())

    def cooldown(self, provider: str, raw_id: str) -> float:
        """Seconds until this endpoint may be tried again (0 = usable now)."""
        now = self._clock()
        until = max(self._until.get((provider, raw_id), 0), self._until.get((provider, None), 0))
        return max(0.0, until - now)

    def ok(self, provider: str, raw_id: str, latency: float) -> None:
        s = self._stat(provider, raw_id)
        s.ok += 1
        s.strikes = 0
        s.last_latency = round(latency, 3)
        self._until.pop((provider, raw_id), None)

    def cool_provider(self, provider: str, seconds: float) -> None:
        """Pause every endpoint of a provider (its limit is on the account, not on one model)."""
        if seconds > 0:
            key = (provider, None)
            self._until[key] = max(self._until.get(key, 0), self._clock() + seconds)

    def fail(self, provider: str, raw_id: str, kind: str, error: str, retry_after: float | None = None) -> float:
        s = self._stat(provider, raw_id)
        s.failed += 1
        s.strikes += 1
        s.last_error = error[:300]
        base, cap, doubles, whole_provider = _POLICY[kind]
        seconds = min(cap, base * 2 ** (s.strikes - 1)) if doubles else base
        if retry_after is not None and retry_after > 0:
            # The provider knows its own limits better than our backoff does.
            seconds = min(max(1.0, retry_after), _MAX_RETRY_AFTER)
        if seconds > 0:
            self._until[(provider, None if whole_provider else raw_id)] = self._clock() + seconds
        return seconds
