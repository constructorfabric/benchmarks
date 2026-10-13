"""Builds the smoke-test server config from config/e2e-local.yaml.

The mini-chat section is replaced so the gear talks to the local mock provider
(`mock_provider.py`) through OAGW over plain HTTP. Usage:

    python3 make_config.py <out.yaml> <home_dir> <api_port> <mock_port> [options]

Options (used by the smoke suite to build per-module config variants):

    --file-max-kb N         rag.uploaded_file_max_size_kb
    --image-max-kb N        rag.uploaded_image_max_size_kb
    --premium-daily N       static policy default_premium_limits.limit_daily_credits_micro
    --total-daily N         static policy default_standard_limits.limit_daily_credits_micro
    --disable-web-search    kill switch disable_web_search
    --disable-images        kill switch disable_images
    --ping-secs N           streaming.sse_ping_interval_seconds (5..=60)
    --web-search-daily N    quota.web_search_daily_quota
    --proxy-timeout-secs N  oagw proxy_timeout_secs (e2e-local.yaml uses 2 s, which also
                            bounds the idle time between two provider stream chunks)
"""

import argparse
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[5]


def model(mid, tier, mock_alias_provider, **over):
    entry = {
        "id": mid,
        "provider_model_id": mid,
        "display_name": mid.upper(),
        "description": f"{mid} test model",
        "provider_id": mock_alias_provider,
        "provider_display_name": "Mock",
        "tier": tier,
        "enabled": True,
        "system_prompt": "You are a test assistant.",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128000,
        "max_output_tokens": 4096,
        "max_input_tokens": 100000,
        "input_tokens_credit_multiplier_micro": 1000000,
        "output_tokens_credit_multiplier_micro": 3000000,
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 2,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0,
                           "presence_penalty": 0.0, "stop": []},
            "features": {"streaming": True, "structured_output": False},
            "tool_support": {"web_search": True, "file_search": True, "image_generation": False,
                             "code_interpreter": True, "mcp": False},
            "supported_endpoints": {"chat_completions": False, "responses": True, "embeddings": False,
                                    "image_generation": False, "audio_speech_generation": False,
                                    "audio_transcription": False, "audio_translation": False},
        },
        "multiplier_display": "1x",
        "preference": {"is_default": tier == "Premium", "sort_order": 0 if tier == "Premium" else 1},
    }
    entry.update(over)
    return entry


def parse_args(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("home")
    ap.add_argument("api_port", type=int)
    ap.add_argument("mock_port", type=int)
    ap.add_argument("--file-max-kb", type=int)
    ap.add_argument("--image-max-kb", type=int)
    ap.add_argument("--premium-daily", type=int)
    ap.add_argument("--total-daily", type=int)
    ap.add_argument("--disable-web-search", action="store_true")
    ap.add_argument("--disable-images", action="store_true")
    ap.add_argument("--ping-secs", type=int)
    ap.add_argument("--proxy-timeout-secs", type=int)
    ap.add_argument("--web-search-daily", type=int)
    return ap.parse_args(argv)


def main():
    args = parse_args(sys.argv[1:])
    out, home, api_port, mock_port = args.out, args.home, args.api_port, args.mock_port
    cfg = yaml.safe_load((ROOT / "config" / "e2e-local.yaml").read_text())
    cfg["server"]["home_dir"] = home
    gears = cfg["gears"]
    # AM tenant types are not part of the mini-chat feature set.
    gears["types-registry"].get("config", {}).pop("entities", None)
    gears["api-gateway"]["config"]["bind_addr"] = f"127.0.0.1:{api_port}"
    gears["mini-chat"]["config"].update({
        "providers": {
            "mock": {
                "kind": "openai_responses",
                "host": "127.0.0.1",
                "port": mock_port,
                "use_http": True,
                "upstream_alias": "mock-openai",
                "api_path": "/v1/responses",
                "storage_kind": "openai",
            }
        },
        "orphan_watchdog": {"timeout_secs": 90, "scan_interval_secs": 5},
        "upload_reaper": {"stale_after_secs": 60, "scan_interval_secs": 5},
    })
    mc = gears["mini-chat"]["config"]
    rag = {}
    if args.file_max_kb is not None:
        rag["uploaded_file_max_size_kb"] = args.file_max_kb
    if args.image_max_kb is not None:
        rag["uploaded_image_max_size_kb"] = args.image_max_kb
    if rag:
        mc["rag"] = rag
    if args.web_search_daily is not None:
        mc["quota"] = {"web_search_daily_quota": args.web_search_daily}
    if args.proxy_timeout_secs is not None:
        gears["oagw"]["config"]["proxy_timeout_secs"] = args.proxy_timeout_secs
    if args.ping_secs is not None:
        mc["streaming"] = {"sse_ping_interval_seconds": args.ping_secs}
    gears["static-mini-chat-audit-plugin"] = {"config": {"vendor": "constructorfabric", "priority": 100}}
    gears["static-mini-chat-model-policy-plugin"] = {"config": {
        "vendor": "constructorfabric",
        "priority": 100,
        "model_catalog": [
            model("gpt-4.1", "Premium", "mock"),
            model("gpt-4.1-mini", "Standard", "mock"),
            model("text-only", "Standard", "mock", multimodal_capabilities=[]),
            model("disabled-model", "Standard", "mock", enabled=False),
        ],
        "kill_switches": {"disable_premium_tier": False, "force_standard_tier": False,
                          "disable_web_search": args.disable_web_search, "disable_file_search": False,
                          "disable_images": args.disable_images, "disable_code_interpreter": False},
    }}
    policy = gears["static-mini-chat-model-policy-plugin"]["config"]
    if args.premium_daily is not None:
        policy["default_premium_limits"] = {"limit_daily_credits_micro": args.premium_daily,
                                            "limit_monthly_credits_micro": 500_000_000}
    if args.total_daily is not None:
        policy["default_standard_limits"] = {"limit_daily_credits_micro": args.total_daily,
                                             "limit_monthly_credits_micro": 1_000_000_000}
    Path(out).write_text(yaml.safe_dump(cfg, sort_keys=False))


if __name__ == "__main__":
    main()
