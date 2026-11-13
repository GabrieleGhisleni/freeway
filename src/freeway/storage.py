"""SQLite persistence; routing and score lookups remain entirely in memory."""

from __future__ import annotations

import hashlib
import json
import logging
import os
import sqlite3
import threading
from pathlib import Path

SCHEMA = """
CREATE TABLE IF NOT EXISTS access_keys (id TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS usage_events (
 id INTEGER PRIMARY KEY, created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
 key_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
 prompt_tokens INTEGER, completion_tokens INTEGER, completed INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS usage_key ON usage_events(key_id);
CREATE TABLE IF NOT EXISTS score_cache (cache_key TEXT PRIMARY KEY, fetched_at REAL NOT NULL, scores TEXT NOT NULL);
"""
SUMMARY = """SELECT k.id AS key_id, k.name, u.provider, u.model, COUNT(*) AS requests,
SUM(u.completed) AS completed, COUNT(*)-COUNT(u.prompt_tokens) AS unknown_prompt_requests,
COUNT(*)-COUNT(u.completion_tokens) AS unknown_completion_requests,
SUM(u.prompt_tokens) AS prompt_tokens, SUM(u.completion_tokens) AS completion_tokens
FROM usage_events u JOIN access_keys k ON k.id=u.key_id
GROUP BY k.id,k.name,u.provider,u.model ORDER BY k.name,k.id,u.provider,u.model"""


def named_tokens() -> dict[str, str]:
    try:
        value = json.loads(os.environ.get("ROUTER_NAMED_TOKENS") or "{}")
        if not isinstance(value, dict) or any(
            not isinstance(k, str)
            or not k.strip()
            or not isinstance(v, str)
            or not v.strip()
            for k, v in value.items()
        ):
            raise ValueError()
        if len(set(value.values())) != len(value):
            raise ValueError()
        return value
    except (ValueError, TypeError):
        raise ValueError(
            "ROUTER_NAMED_TOKENS must be a JSON object of unique non-empty name: token pairs"
        ) from None


def identity(token: str, names: dict[str, str]) -> tuple[str, str]:
    if not token:
        return "anonymous", "anonymous"
    return hashlib.sha256(token.encode()).hexdigest(), next(
        (n for n, t in names.items() if t == token), "unnamed"
    )


class Store:
    def __init__(self, path: str):
        if path != ":memory:":
            Path(path).parent.mkdir(parents=True, exist_ok=True)
        self.lock = threading.Lock()
        self.db = sqlite3.connect(path, timeout=5, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.executescript(SCHEMA)

    def close(self):
        with self.lock:
            self.db.close()

    def register(self, keys):
        with self.lock, self.db:
            self.db.executemany(
                "INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
                keys,
            )

    def record(self, key, provider, model, usage, completed=True):
        def count(name):
            v = usage.get(name) if isinstance(usage, dict) else None
            return v if type(v) is int and 0 <= v <= 9223372036854775807 else None

        try:
            with self.lock, self.db:
                self.db.execute(
                    "INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
                    key,
                )
                self.db.execute(
                    "INSERT INTO usage_events(key_id,provider,model,prompt_tokens,completion_tokens,completed) VALUES (?,?,?,?,?,?)",
                    (
                        key[0],
                        provider,
                        model,
                        count("prompt_tokens"),
                        count("completion_tokens"),
                        int(completed),
                    ),
                )
        except sqlite3.Error:
            logging.getLogger("freeway").exception("could not persist usage")

    def summary(self):
        with self.lock:
            return [dict(r) for r in self.db.execute(SUMMARY)]

    def load_scores(self, key):
        with self.lock:
            row = self.db.execute(
                "SELECT fetched_at,scores FROM score_cache WHERE cache_key=?", (key,)
            ).fetchone()
        return {"fetched_at": row[0], "table": json.loads(row[1])} if row else None

    def save_scores(self, key, table, at):
        with self.lock, self.db:
            self.db.execute(
                "INSERT INTO score_cache VALUES (?,?,?) ON CONFLICT(cache_key) DO UPDATE SET fetched_at=excluded.fetched_at,scores=excluded.scores",
                (key, at, json.dumps(table)),
            )


class UsageParser:
    """Bounded SSE line parser, independent of transport chunk boundaries."""

    def __init__(self):
        self.pending = b""
        self.usage = None
        self.done = False
        self.failed = False
        self.discarding = False

    def feed(self, chunk):
        for i, part in enumerate(chunk.split(b"\n")):
            if i:
                if not self.discarding:
                    line = self.pending.rstrip(b"\r")
                    if line.startswith(b"data:"):
                        data = line[5:].strip()
                        if data == b"[DONE]":
                            self.done = True
                        else:
                            try:
                                obj = json.loads(data)
                                if isinstance(obj, dict):
                                    self.failed |= "error" in obj
                                    if isinstance(obj.get("usage"), dict):
                                        self.usage = obj["usage"]
                            except (ValueError, UnicodeDecodeError):
                                pass
                self.pending = b""
                self.discarding = False
            if len(self.pending) + len(part) > 1048576:
                self.pending = b""
                self.discarding = True
            elif not self.discarding:
                self.pending += part
