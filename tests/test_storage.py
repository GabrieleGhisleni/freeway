import json

import httpx
import pytest
from fastapi.testclient import TestClient
from test_routing import AUTH, OK, Fake, chat, settings

from freeway.app import create_app, load_tokens
from freeway.config import ScoresConfig
from freeway.scores import ScoreBook
from freeway.storage import Store, UsageParser, identity, named_tokens


def test_named_tokens_are_additive_and_validate_without_leaking(monkeypatch):
    monkeypatch.setenv("ROUTER_TOKENS", "legacy")
    monkeypatch.delenv("ROUTER_TOKEN", raising=False)
    monkeypatch.setenv("ROUTER_NAMED_TOKENS", '{"eHub":"secret"}')
    assert set(load_tokens()) == {"legacy", "secret"}
    for bad in ("[]", '{"x":""}', '{"x":"secret","y":"secret"}', '{"x":3}', "secret"):
        monkeypatch.setenv("ROUTER_NAMED_TOKENS", bad)
        with pytest.raises(ValueError, match="ROUTER_NAMED_TOKENS") as exc:
            named_tokens()
        assert "secret" not in str(exc.value)


def test_accounting_survives_restart_and_rename(tmp_path):
    path = str(tmp_path / "state.sqlite3")
    key = identity("secret", {"eHub": "secret"})
    store = Store(path)
    store.record(key, "p", "m", {"prompt_tokens": 12, "completion_tokens": 5})
    store.record(identity("other", {}), "p", "m", None, False)
    store.close()
    store = Store(path)
    store.register([identity("secret", {"books": "secret"})])
    assert next(r for r in store.summary() if r["key_id"] == key[0])["name"] == "books"
    store.record(identity("secret", {"books": "secret"}), "p", "m", None, False)
    rows = {r["key_id"]: r for r in store.summary()}
    row = rows[key[0]]
    assert row["name"] == "books" and row["requests"] == 2
    assert row["completed"] == 1 and row["prompt_tokens"] == 12
    assert row["unknown_prompt_requests"] == 1
    assert rows[identity("other", {})[0]]["prompt_tokens"] is None
    store.close()
    assert b"secret" not in (tmp_path / "state.sqlite3").read_bytes()


def test_sse_parser_handles_every_split_and_bounded_lines():
    sse = b'data: {"choices":[]}\r\n\r\ndata: {"usage":{"prompt_tokens":3,"completion_tokens":7}}\n\ndata: [DONE]\n\n'
    for split in range(len(sse)):
        parser = UsageParser()
        parser.feed(sse[:split])
        parser.feed(sse[split:])
        assert parser.done and parser.usage["completion_tokens"] == 7
    parser = UsageParser()
    parser.feed(b"x" * 1100000 + b'\ndata: {"error":{}}\n\ndata: [DONE]\n\n')
    assert parser.failed and parser.done and len(parser.pending) < 1048576


@pytest.mark.parametrize("stream", [False, True])
def test_named_key_usage_api(tmp_path, monkeypatch, stream):
    monkeypatch.setenv("KEY_A", "ka")
    monkeypatch.setenv("KEY_B", "kb")
    monkeypatch.setenv("ROUTER_NAMED_TOKENS", '{"eHub":"secret"}')
    usage = {"prompt_tokens": 4, "completion_tokens": 9}
    sse = (
        b'data: {"choices":[{"delta":{"content":"hi"}}]}\n\ndata: '
        + json.dumps({"usage": usage}).encode()
        + b"\n\ndata: [DONE]\n\n"
    )
    reply = (
        (
            lambda: httpx.Response(
                200, content=sse, headers={"content-type": "text/event-stream"}
            )
        )
        if stream
        else (lambda: httpx.Response(200, json={**OK, "usage": usage}))
    )
    fake = Fake({"a.test": ["big-500b"], "b.test": []}, {("a.test", "big-500b"): reply})
    config = settings(database_file=str(tmp_path / "app.sqlite3"))
    with TestClient(
        create_app(config, httpx.AsyncClient(transport=httpx.MockTransport(fake)))
    ) as c:
        assert c.get("/admin/usage").status_code == 401
        assert chat(c, "nope").status_code == 404
        assert chat(c, "big-500b", stream=stream).status_code == 200
        (row,) = c.get("/admin/usage", headers=AUTH).json()["data"]
        assert row["name"] == "eHub" and row["requests"] == 1
        assert row["prompt_tokens"] == 4 and row["completion_tokens"] == 9
        assert row["completed"] == 1 and "secret" not in json.dumps(row)


def test_json_cache_imported_once_and_replaced_atomically(tmp_path):
    legacy = tmp_path / "scores.json"
    legacy.write_text(json.dumps({"fetched_at": 1, "table": {"modelx": 33}}))
    path = str(tmp_path / "state.sqlite3")
    store = Store(path)
    cfg = ScoresConfig(cache_file=str(legacy))
    b = ScoreBook(cfg, httpx.AsyncClient(), store)
    assert b.table == {"modelx": 33}
    b.table, b.fetched_at = {"modely": 40}, 2
    b._save_cache()
    store.close()
    store = Store(path)
    b = ScoreBook(cfg, httpx.AsyncClient(), store)
    assert b.table == {"modely": 40} and b.fetched_at == 2
    assert json.loads(legacy.read_text())["table"] == {"modelx": 33}
    store.close()


def test_legacy_usage_migrates_once_and_old_writers_update_totals(tmp_path):
    import sqlite3

    from freeway.storage import SCHEMA

    path = str(tmp_path / "legacy.sqlite3")
    with sqlite3.connect(path) as db:
        db.executescript(SCHEMA)
        db.execute("INSERT INTO access_keys VALUES ('k','old')")
        db.executemany(
            "INSERT INTO usage_events(key_id,provider,model,prompt_tokens,completion_tokens,completed) VALUES ('k','p','m',?,?,?)",
            [(4, None, 1), (None, 8, 0)],
        )
    store = Store(path)
    (row,) = store.summary()
    assert (row["requests"], row["prompt_tokens"], row["completion_tokens"], row["completed"]) == (2, 4, 8, 1)
    store.close()
    # A previous version writing only the event table still updates the totals.
    with sqlite3.connect(path) as db:
        db.execute("INSERT INTO usage_events(key_id,provider,model,completed) VALUES ('k','p','m',0)")
    store = Store(path)
    store.register([("k", "renamed")])
    (row,) = store.summary()
    assert row["requests"] == 3 and row["name"] == "renamed"
    assert row["unknown_prompt_requests"] == row["unknown_completion_requests"] == 2
    # Summary must not read the ever-growing event table.
    store.db.set_authorizer(lambda action, table, *args: sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_READ and table == "usage_events" else sqlite3.SQLITE_OK)
    assert store.summary() == [row]
    store.close()


async def test_writer_bounds_pending_work_and_flushes_on_close():
    import asyncio
    import threading

    from freeway.storage import UsageWriter

    entered, release = threading.Event(), threading.Event()
    store = Store(":memory:")
    original = store.record_batch
    batches = []

    def blocked(events):
        batches.append(len(events))
        entered.set()
        assert release.wait(5)
        original(events)

    store.record_batch = blocked
    writer = UsageWriter(store, capacity=2)
    key = ("k", "name")
    async with writer.slot() as ticket:
        ticket.record(key, "p", "m", {"prompt_tokens": 3})
    assert await asyncio.to_thread(entered.wait, 5)
    async with writer.slot() as ticket:
        ticket.record(key, "p", "m", None, False)
    acquired = asyncio.Event()

    async def next_request():
        async with writer.slot() as ticket:
            acquired.set()
            ticket.record(key, "p", "m", {"completion_tokens": 4})

    waiting = asyncio.create_task(next_request())
    await asyncio.sleep(0)
    assert not acquired.is_set()  # persisted writes still own the capacity
    release.set()
    await asyncio.wait_for(waiting, 5)
    await writer.close()
    (row,) = store.summary()
    assert row["requests"] == 3 and row["prompt_tokens"] == 3 and row["completion_tokens"] == 4
    assert all(n <= 64 for n in batches)
    store.close()


async def test_writer_batches_ready_events():
    from freeway.storage import UsageWriter

    store = Store(":memory:")
    commits = []
    store.db.set_trace_callback(lambda sql: commits.append(sql) if sql == "COMMIT" else None)
    writer = UsageWriter(store)
    for _ in range(130):
        async with writer.slot() as ticket:
            ticket.record(("k", "n"), "p", "m", None)
    await writer.close()
    assert store.summary()[0]["requests"] == 130
    assert len(commits) == 3
    store.close()
