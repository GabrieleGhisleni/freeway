from freeway import normalize as n


def test_key_strips_vendor_suffix_and_date():
    assert n.key("google/gemma-4-31b-it:free") == "gemma-4-31b-it"
    assert n.key("google/gemma-4-31b-it-20260402") == "gemma-4-31b-it"
    assert n.key("nemotron-3.5-lightning-free") == "nemotron-3.5-lightning"


def test_key_keeps_vendor_named_models_intact():
    assert n.key("deepseek-v4.1-flash-free") == "deepseek-v4.1-flash"
    assert n.key("deepseek/deepseek-v4.1-flash-1017") == "deepseek-v4.1-flash"
    assert n.key("nvidia/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-BF16") == "nvidia-nemotron-3.5-lightning-30b-a3b"


def test_key_edge_cases():
    assert n.key("") == ""
    assert n.key("/") == ""
    assert n.key("free") == "free"


def test_google_models_prefix_is_stripped_from_upstream_id():
    from freeway.catalog import parse_openai
    from freeway.config import ProviderConfig

    p = ProviderConfig(name="google", base_url="https://g.test/v1", api_key_env="K")
    [ep] = parse_openai(p, {"data": [{"id": "models/gemini-3.8-flash"}]})
    assert ep.raw_id == "gemini-3.8-flash"
    assert ep.keys == {"gemini-3.8-flash"}


def test_family_key_joins_provider_and_benchmark_spellings():
    assert n.family_key("gemini-3.5-flash") == n.family_key("gemini-3-5-flash-high") == "gemini35flash"
    assert n.family_key("deepseek-v4.1-flash-free") == n.family_key("deepseek-v4-1-flash-1017-non-reasoning")
    assert n.family_key("nemotron-3-nano-omni-30b-a3b-reasoning") == n.family_key("nemotron-3-nano-omni-30b-a3b")


def test_family_key_keeps_distinct_models_distinct():
    assert n.family_key("glm-5-3-flash") != n.family_key("glm-5-3")
    assert n.family_key("gemma-4-31b-it") != n.family_key("gemma-4-26b-a4b-it")
