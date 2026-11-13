"""SQLite persistence; routing and score lookups remain entirely in memory."""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import sqlite3
import threading
from contextlib import asynccontextmanager
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
MIGRATION = """BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS storage_migrations (name TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS usage_totals (
 key_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
 requests INTEGER NOT NULL, completed INTEGER NOT NULL,
 unknown_prompt_requests INTEGER NOT NULL, unknown_completion_requests INTEGER NOT NULL,
 prompt_tokens INTEGER, completion_tokens INTEGER,
 PRIMARY KEY(key_id,provider,model));
INSERT INTO usage_totals
SELECT key_id,provider,model,COUNT(*),SUM(completed),
 COUNT(*)-COUNT(prompt_tokens),COUNT(*)-COUNT(completion_tokens),SUM(prompt_tokens),SUM(completion_tokens)
FROM usage_events WHERE NOT EXISTS (SELECT 1 FROM storage_migrations WHERE name='usage_totals_v1')
GROUP BY key_id,provider,model;
CREATE TRIGGER IF NOT EXISTS usage_totals_insert AFTER INSERT ON usage_events BEGIN
 INSERT INTO usage_totals VALUES (NEW.key_id,NEW.provider,NEW.model,1,NEW.completed,
 NEW.prompt_tokens IS NULL,NEW.completion_tokens IS NULL,NEW.prompt_tokens,NEW.completion_tokens)
 ON CONFLICT(key_id,provider,model) DO UPDATE SET
 requests=requests+1, completed=completed+excluded.completed,
 unknown_prompt_requests=unknown_prompt_requests+excluded.unknown_prompt_requests,
 unknown_completion_requests=unknown_completion_requests+excluded.unknown_completion_requests,
 prompt_tokens=CASE WHEN excluded.prompt_tokens IS NULL THEN usage_totals.prompt_tokens ELSE COALESCE(usage_totals.prompt_tokens,0)+excluded.prompt_tokens END,
 completion_tokens=CASE WHEN excluded.completion_tokens IS NULL THEN usage_totals.completion_tokens ELSE COALESCE(usage_totals.completion_tokens,0)+excluded.completion_tokens END;
END;
INSERT OR IGNORE INTO storage_migrations VALUES ('usage_totals_v1');
COMMIT;"""
SUMMARY = """SELECT k.id AS key_id,k.name,u.provider,u.model,u.requests,u.completed,
u.unknown_prompt_requests,u.unknown_completion_requests,u.prompt_tokens,u.completion_tokens
FROM usage_totals u JOIN access_keys k ON k.id=u.key_id
ORDER BY k.name,k.id,u.provider,u.model"""


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
        self.db.executescript(MIGRATION)

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
        self.record_batch([(key, provider, model, usage, completed)])

    def record_batch(self, events):
        def count(usage, name):
            v = usage.get(name) if isinstance(usage, dict) else None
            return v if type(v) is int and 0 <= v <= 9223372036854775807 else None

        try:
            with self.lock, self.db:
                for key, provider, model, usage, completed in events:
                    self.db.execute(
                        "INSERT INTO access_keys VALUES (?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
                        key,
                    )
                    self.db.execute(
                        "INSERT INTO usage_events(key_id,provider,model,prompt_tokens,completion_tokens,completed) VALUES (?,?,?,?,?,?)",
                        (key[0], provider, model, count(usage, "prompt_tokens"), count(usage, "completion_tokens"), int(completed)),
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


class UsageTicket:
    def __init__(self, writer):
        self.writer = writer
        self.owned = True
        self.deferred = False

    def release(self):
        if self.owned:
            self.owned = False
            self.writer.slots.release()

    def record(self, key, provider, model, usage, completed=True):
        if not self.owned:
            raise RuntimeError("usage ticket already consumed")
        # Copy only the two counters, never an arbitrarily large upstream object.
        counts = None
        if isinstance(usage, dict):
            counts = {
                k: v if type(v := usage.get(k)) is int and 0 <= v <= 9223372036854775807 else None
                for k in ("prompt_tokens", "completion_tokens")
            }
        self.writer.queue.put_nowait(((key, provider, model, counts, completed), None))
        self.owned = False


class UsageWriter:
    """Reservations bound both pending writes and requests holding accounting slots."""

    def __init__(self, store, capacity=1024):
        self.store = store
        self.slots = asyncio.Semaphore(capacity)
        self.queue = asyncio.Queue()
        self.task = asyncio.create_task(self._run())

    @asynccontextmanager
    async def slot(self):
        await self.slots.acquire()
        ticket = UsageTicket(self)
        try:
            yield ticket
        finally:
            if not ticket.deferred:
                ticket.release()

    async def flush(self):
        await self.slots.acquire()
        future = asyncio.get_running_loop().create_future()
        self.queue.put_nowait((None, future))
        await asyncio.shield(future)

    async def close(self):
        await self.flush()
        self.queue.put_nowait(None)
        await self.task

    async def _run(self):
        while True:
            item = await self.queue.get()
            if item is None:
                return
            batch = [item]
            while len(batch) < 64 and batch[-1][1] is None and not self.queue.empty():
                batch.append(self.queue.get_nowait())
            events = [event for event, _ in batch if event is not None]
            try:
                if events:
                    await asyncio.to_thread(self.store.record_batch, events)
            except Exception:
                logging.getLogger("freeway").exception("usage writer failed")
            finally:
                for _, future in batch:
                    self.slots.release()
                    if future is not None and not future.done():
                        future.set_result(None)


class UsageParser:
    """Bounded SSE line parser, independent of transport chunk boundaries."""

    def __init__(self):
        self.pending = bytearray()
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
                self.pending = bytearray()
                self.discarding = False
            if len(self.pending) + len(part) > 1048576:
                self.pending = bytearray()
                self.discarding = True
            elif not self.discarding:
                self.pending.extend(part)
