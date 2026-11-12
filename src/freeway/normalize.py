"""Turning provider-specific model ids into comparable names.

Every provider names the same weights differently
(`nvidia/nemotron-3.5-super-vl:free`, `nemotron-3.5-super-vl-free`,
`nvidia/nemotron-3.5-super-vl-20261015`). `key()` reduces each of those to
`nemotron-3.5-super-vl`; two listings are the same model when they share at
least one key. Anything the heuristics cannot join goes under `aliases:` in
config.yaml.
"""

from __future__ import annotations

import re

# Trailing tokens that describe packaging, not the model.
_NOISE_TOKENS = {
    "free", "bf16", "fp8", "fp16", "fp32", "gguf", "awq", "gptq", "int4", "int8",
    "hf", "latest",
}
# Trailing tokens that describe how a benchmark ran the model (reasoning effort).
_VARIANT_TOKENS = {
    "reasoning", "non", "thinking", "adaptive", "default", "fallback",
    "minimal", "low", "medium", "high", "xhigh", "max",
}
_DATE_TOKEN = re.compile(r"^(?:\d{4}|\d{6}|\d{8})$")
_ISO_DATE_TAIL = re.compile(r"-\d{4}-\d{2}-\d{2}$")


def key(model_id: str) -> str:
    """Comparable form of a model id. Empty string when nothing is left."""
    s = model_id.strip().lower()
    if not s:
        return ""
    s = s.replace(":free", "").replace(":", "-")
    if "/" in s:
        parts = [p for p in s.split("/") if p]
        if not parts:
            return ""
        s = parts[-1]  # the vendor prefix is dropped: listings disagree on whether it is there
    s = re.sub(r"[_\s]+", "-", s)
    s = re.sub(r"-{2,}", "-", s).strip("-")
    s = _ISO_DATE_TAIL.sub("", s)
    tokens = s.split("-")
    while len(tokens) > 1 and (tokens[-1] in _NOISE_TOKENS or _DATE_TOKEN.match(tokens[-1])):
        tokens.pop()
    return "-".join(tokens)


def keys(*model_ids: str | None) -> set[str]:
    return {k for m in model_ids if m for k in [key(m)] if k}


def family_key(model_id: str) -> str:
    """Looser key used to look a model up in benchmark tables.

    Benchmarks write `gemini-3-5-flash-high` where providers write
    `gemini-3.5-flash`: drop reasoning-effort suffixes and every punctuation mark.
    """
    tokens = key(model_id).split("-")
    while len(tokens) > 1 and (tokens[-1] in _VARIANT_TOKENS or _DATE_TOKEN.match(tokens[-1])):
        tokens.pop()
    return re.sub(r"[^a-z0-9]", "", "".join(tokens))
