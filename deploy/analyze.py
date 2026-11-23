"""Summarize `GET /admin/info` (JSON on stdin): which providers and models fail, and why.

Counters in /admin/info are cumulative since the health file was created, so every row
also shows how long ago the last failure and the last success were.
Run it with `make models`.
"""
import json
import sys
import time
from collections import Counter, defaultdict


def ago(ts, now):
    if not ts:
        return "never"
    h = (now - ts) / 3600
    return f"{h * 60:.0f}m ago" if h < 1 else f"{h:.0f}h ago" if h < 48 else f"{h / 24:.0f}d ago"


def verdict(m):
    kinds = m["failures_by_kind"]
    if m["ok"] == 0 and kinds.get("bad_request"):
        return "BROKEN: always rejected (request/config problem)"
    if m["ok"] == 0 and kinds.get("rate_limit"):
        return "QUOTA: free tier exhausted"
    if m["ok"] == 0:
        return "never worked"
    if kinds.get("server", 0) > kinds.get("rate_limit", 0) and m["success_rate"] < 0.9:
        return "flaky upstream (5xx/timeouts)"
    if m["success_rate"] < 0.9:
        return "rate-limited"
    return "ok"


def main():
    d = json.load(sys.stdin)
    now = d.get("now", time.time())

    print("PROVIDERS")
    for p, v in sorted(d["providers"].items(), key=lambda kv: kv[1]["success_rate"]):
        print(f"  {p:11} ok={v['ok']:4} failed={v['failed']:4} success={v['success_rate']:.0%}")

    kinds = defaultdict(Counter)
    for m in d["models"]:
        for k, n in m["failures_by_kind"].items():
            kinds[m["endpoint"].split("/")[0]][k] += n
    print("\nFAILURES BY KIND")
    for p, c in kinds.items():
        print(f"  {p:11} {dict(c)}")

    print("\nMODELS WITH FAILURES (worst first)")
    bad = [m for m in d["models"] if m["failed"]]
    for m in sorted(bad, key=lambda m: (m["success_rate"], -m["failed"])):
        print(f"  {m['endpoint'][:50]:50} tier={str(m['tier']):8} ok={m['ok']:3} failed={m['failed']:3} "
              f"last_fail={ago(m['last_failed_at'], now):9} last_ok={ago(m['last_ok_at'], now)}")
        print(f"      {verdict(m)}")
        if m["last_error"]:
            print(f"      last error: {m['last_error'][:140]}")

    if d["paused"]:
        print("\nPAUSED NOW")
        for p in d["paused"]:
            print(f"  {p}")


if __name__ == "__main__":
    main()
