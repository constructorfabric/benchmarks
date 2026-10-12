"""Model catalog served by the bundled static model-policy plugin in this suite.

Three entries cover what the black-box tests need:

* ``mock-premium``  - premium tier, the ``is_default`` model, vision input and
  every tool (web search, file search, code interpreter);
* ``mock-standard`` - standard tier, text only, no web search / code interpreter;
* ``mock-disabled`` - ``enabled: false``: never visible through the Models API.

``public_projection`` is the Models-API view of an entry (DESIGN 3.3 "Models
API"): the only fields a client may see.
"""

from __future__ import annotations

import copy
from typing import Any, Dict, List, Optional

PROVIDER_ID = "openai"

# Provider-side marker values that must never appear in a Models-API response.
SECRET_PROVIDER_MODEL_PREFIX = "gpt-mock-deployment-"
SECRET_SYSTEM_PROMPT = "MOCK-SECRET-SYSTEM-PROMPT"


def _entry(model_id: str, *, tier: str, enabled: bool, display_name: str, description: str,
           multiplier_display: str, in_mult: int, out_mult: int, capabilities: List[str],
           tools: Dict[str, bool], is_default: bool, sort_order: int,
           context_window: int = 128_000) -> Dict[str, Any]:
    return {
        "id": model_id,
        "provider_model_id": SECRET_PROVIDER_MODEL_PREFIX + model_id,
        "display_name": display_name,
        "description": description,
        "provider_id": PROVIDER_ID,
        "provider_display_name": "Mock Provider Inc.",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": SECRET_SYSTEM_PROMPT,
        "thread_summary_prompt": "",
        "multimodal_capabilities": capabilities,
        "context_window": context_window,
        "max_output_tokens": 4096,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": in_mult,
        "output_tokens_credit_multiplier_micro": out_mult,
        "multiplier_display": multiplier_display,
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50,
        },
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 4,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {
                "temperature": 0.7,
                "top_p": 1.0,
                "frequency_penalty": 0.0,
                "presence_penalty": 0.0,
                "stop": [],
            },
            "features": {"streaming": True, "structured_output": False},
            "tool_support": {
                "web_search": tools.get("web_search", False),
                "file_search": tools.get("file_search", False),
                "image_generation": False,
                "code_interpreter": tools.get("code_interpreter", False),
                "mcp": False,
            },
            "supported_endpoints": {
                "chat_completions": False,
                "responses": True,
                "embeddings": False,
                "image_generation": False,
                "audio_speech_generation": False,
                "audio_transcription": False,
                "audio_translation": False,
            },
        },
        "preference": {"is_default": is_default, "sort_order": sort_order},
    }


CATALOG: List[Dict[str, Any]] = [
    _entry(
        "mock-premium", tier="Premium", enabled=True, display_name="Mock Premium",
        description="Premium test model", multiplier_display="3x",
        in_mult=3_000_000, out_mult=15_000_000,
        capabilities=["VISION_INPUT", "RAG"],
        tools={"web_search": True, "file_search": True, "code_interpreter": True},
        is_default=True, sort_order=0),
    _entry(
        "mock-standard", tier="Standard", enabled=True, display_name="Mock Standard",
        description="Standard test model", multiplier_display="1x",
        in_mult=1_000_000, out_mult=3_000_000,
        capabilities=["RAG"],
        tools={"file_search": True},
        is_default=False, sort_order=1),
    _entry(
        "mock-disabled", tier="Standard", enabled=False, display_name="Mock Disabled",
        description="Disabled test model", multiplier_display="1x",
        in_mult=1_000_000, out_mult=3_000_000,
        capabilities=[], tools={},
        is_default=False, sort_order=2),
]

PREMIUM_ID = "mock-premium"
STANDARD_ID = "mock-standard"
DISABLED_ID = "mock-disabled"
ENABLED_IDS = [e["id"] for e in CATALOG if e["enabled"]]


def catalog_entries() -> List[Dict[str, Any]]:
    """A deep copy, safe to mutate before it is written into a server config."""
    return copy.deepcopy(CATALOG)


def entry(model_id: str) -> Dict[str, Any]:
    for e in CATALOG:
        if e["id"] == model_id:
            return e
    raise KeyError(model_id)


def public_projection(model_id: str) -> Dict[str, Any]:
    """The Models-API item for ``model_id`` (``description`` is included: all entries set one)."""
    e = entry(model_id)
    return {
        "model_id": e["id"],
        "display_name": e["display_name"],
        "tier": e["tier"].lower(),
        "multiplier_display": e["multiplier_display"],
        "description": e["description"],
        "multimodal_capabilities": list(e["multimodal_capabilities"]),
        "context_window": e["context_window"],
    }


# --------------------------------------------------------------------------- #
# Streaming slice: the catalog of the "restricted" stack
# --------------------------------------------------------------------------- #
#
# Models that only exist on the extra server the streaming tests start with
# `stack_factory` (a tiny context window, a tiny input limit, a huge system prompt)
# so budget rules can be hit with a few bytes. The stack also turns the
# `disable_web_search` kill switch on. The shared server keeps the three-model
# catalog above (the Models API tests count its entries).

TINY_INPUT_ID = "tiny-input"        # max_input_tokens = 400 (input budget 300 after the overhead)
TINY_WINDOW_ID = "tiny-window"      # applied output cap (4096) >= context window (1000)
BIG_SYSTEM_ID = "big-system"        # 4000-byte system prompt in a 600-token window
FIT_ZERO_ID = "fit-zero"            # window computed so that no history message fits
FIT_THREE_ID = "fit-three"          # ... exactly three 400-byte messages fit
FIT_FOUR_ID = "fit-four"            # ... exactly four fit
FIT_SIX_ID = "fit-six"              # ... six fit
FIT_SYSTEM_PROMPT = "You are tiny."
FIT_CURRENT_MESSAGE = "now"
FIT_MESSAGE_BYTES = 400
FIT_MAX_OUTPUT = 100


def estimate_tokens(text: str, bytes_per_token: int = 4, overhead: int = 100,
                    margin_pct: int = 10) -> int:
    """DESIGN 5.5.4: ``ceil((ceil(bytes / bpt) + overhead) * (100 + margin) / 100)``."""
    size = len(text.encode("utf-8"))
    base = -(-size // bytes_per_token) + overhead
    return -(-(base * (100 + margin_pct)) // 100)


def fit_window(messages_that_fit: int) -> int:
    """Context window of a model whose input budget is exactly the mandatory items (the
    system prompt and the current message) plus ``messages_that_fit`` 400-byte messages:
    ``budget = window - max_output - fixed_overhead``."""
    budget = (estimate_tokens(FIT_SYSTEM_PROMPT) + estimate_tokens(FIT_CURRENT_MESSAGE)
              + messages_that_fit * estimate_tokens("x" * FIT_MESSAGE_BYTES))
    return budget + FIT_MAX_OUTPUT + 100


def variant(model_id: str, **patch: Any) -> Dict[str, Any]:
    """A deep copy of the standard entry (text only, no web search) under another id."""
    value = copy.deepcopy(entry(STANDARD_ID))
    value["id"] = model_id
    value["provider_model_id"] = SECRET_PROVIDER_MODEL_PREFIX + model_id
    value["preference"] = {"is_default": False, "sort_order": 10}
    value.update(patch)
    return value


def fit_model(model_id: str, messages_that_fit: int) -> Dict[str, Any]:
    return variant(model_id, context_window=fit_window(messages_that_fit),
                   max_output_tokens=FIT_MAX_OUTPUT, max_input_tokens=0,
                   system_prompt=FIT_SYSTEM_PROMPT)


def restricted_catalog() -> List[Dict[str, Any]]:
    """The shared catalog plus the budget-probing models."""
    return catalog_entries() + [
        variant(TINY_INPUT_ID, max_input_tokens=400),
        variant(TINY_WINDOW_ID, context_window=1000, max_output_tokens=4096, max_input_tokens=0),
        variant(BIG_SYSTEM_ID, context_window=600, max_output_tokens=100, max_input_tokens=0,
                system_prompt="p" * 4000),
        fit_model(FIT_ZERO_ID, 0),
        fit_model(FIT_THREE_ID, 3),
        fit_model(FIT_FOUR_ID, 4),
        fit_model(FIT_SIX_ID, 6),
    ]


# --------------------------------------------------------------------------- #
# Quota slice: credit arithmetic, reserve estimate and the stacks' limits
# --------------------------------------------------------------------------- #
#
# The e2e catalog prices ``mock-premium`` at 3_000_000 / 15_000_000 micro-credits per 1M input /
# output tokens and ``mock-standard`` at 1_000_000 / 3_000_000 (``multiplier_display`` 3x / 1x);
# both have ``max_output_tokens`` 4096 and the default estimation budgets (4 bytes per token,
# overhead 100, margin 10 %, image 1000, file search 500, web search 500, code interpreter 1000,
# floor 50). A short message therefore reserves ``(113 + 4096)`` tokens: 61_779 micro-credits on
# the premium model and 12_401 on the standard one.

PREMIUM_IN_MULT = 3_000_000
PREMIUM_OUT_MULT = 15_000_000
STANDARD_IN_MULT = 1_000_000
STANDARD_OUT_MULT = 3_000_000
MINIMAL_GENERATION_FLOOR = 50
STREAMING_MAX_OUTPUT_TOKENS = 32_768  # gear default: caps nothing for the 4096-token models

# Limits (micro-credits) of the quota stacks: bucket ``total`` is the plugin's "standard" limit,
# bucket ``tier:premium`` its "premium" limit (DESIGN 5.4.2).
QUOTA_LIMITS = {
    "total": {"daily": 2_000_000, "monthly": 20_000_000},
    "tier:premium": {"daily": 1_000_000, "monthly": 10_000_000},
}
WARNING_THRESHOLD_PCT = 75
WEB_SEARCH_DAILY_QUOTA = 3
WEB_SEARCH_MAX_CALLS_PER_MESSAGE = 2
CODE_INTERPRETER_MAX_CALLS_PER_MESSAGE = 2


def credits_micro(input_tokens: int, output_tokens: int, in_mult: int, out_mult: int) -> int:
    """DESIGN 5.3: ``ceil_div(i * in_mult, 1e6) + ceil_div(o * out_mult, 1e6)`` (per component)."""
    def part(tokens: int, mult: int) -> int:
        return -(-(tokens * mult) // 1_000_000)
    return part(input_tokens, in_mult) + part(output_tokens, out_mult)


def model_credits(model_id: str, input_tokens: int, output_tokens: int) -> int:
    e = entry(model_id)
    return credits_micro(input_tokens, output_tokens, e["input_tokens_credit_multiplier_micro"],
                         e["output_tokens_credit_multiplier_micro"])


def reserve_for(model_id: str, text: str, *, prior_context: int = 0, surcharge_tokens: int = 0,
                streaming_max_output: int = STREAMING_MAX_OUTPUT_TOKENS) -> Dict[str, int]:
    """The reserve ``model_id`` books for ``text`` (DESIGN 5.4.1): estimated input tokens (message
    estimate + ``prior_context`` + ``surcharge_tokens``), the output cap, the token reserve and the
    credits reserve."""
    e = entry(model_id)
    max_out = min(e["max_output_tokens"], streaming_max_output)
    est_in = estimate_tokens(text) + prior_context + surcharge_tokens
    return {
        "estimated_input_tokens": est_in,
        "max_output_tokens_applied": max_out,
        "reserve_tokens": est_in + max_out,
        "reserved_credits_micro": model_credits(model_id, est_in, max_out),
    }


def estimated_charge(model_id: str, text: str, *, prior_context: int = 0,
                     surcharge_tokens: int = 0, floor: int = MINIMAL_GENERATION_FLOOR) -> int:
    """The estimated settlement of a turn without usage (DESIGN 5.8): credits of the input
    estimate plus the minimal generation floor (the floor is capped by the output cap)."""
    r = reserve_for(model_id, text, prior_context=prior_context, surcharge_tokens=surcharge_tokens)
    return model_credits(model_id, r["estimated_input_tokens"],
                         min(floor, r["max_output_tokens_applied"]))


def quota_overrides(*, kill_switches: Optional[Dict[str, bool]] = None) -> Dict[str, Any]:
    """Server config overrides of a quota stack: small limits, the quota keys the tests drive and
    optional static kill switches."""
    plugin: Dict[str, Any] = {
        "default_standard_limits": {
            "limit_daily_credits_micro": QUOTA_LIMITS["total"]["daily"],
            "limit_monthly_credits_micro": QUOTA_LIMITS["total"]["monthly"]},
        "default_premium_limits": {
            "limit_daily_credits_micro": QUOTA_LIMITS["tier:premium"]["daily"],
            "limit_monthly_credits_micro": QUOTA_LIMITS["tier:premium"]["monthly"]},
    }
    if kill_switches:
        plugin["kill_switches"] = dict(kill_switches)
    return {"gears": {
        "static-mini-chat-model-policy-plugin": {"config": plugin},
        "mini-chat": {"config": {"quota": {
            "warning_threshold_pct": WARNING_THRESHOLD_PCT,
            "web_search_daily_quota": WEB_SEARCH_DAILY_QUOTA,
            "web_search_max_calls_per_message": WEB_SEARCH_MAX_CALLS_PER_MESSAGE,
            "code_interpreter_max_calls_per_message": CODE_INTERPRETER_MAX_CALLS_PER_MESSAGE,
        }}},
    }}
