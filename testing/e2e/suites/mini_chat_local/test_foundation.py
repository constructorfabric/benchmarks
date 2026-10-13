"""Foundation checks of the running stack: the example server starts the gear against a
real SQLite file, applies the P1 migrations and wires the bundled plugins.

Source: DESIGN 3.2 "Gear lifecycle" (migrations include the outbox migrations),
3.7 (tables; no ``mcp_*`` tables), B.1 (bundled plugins), "Robust startup".
"""

from __future__ import annotations

from . import catalog
from .support import OPENAI_SECRET_VALUE, GearDb

P1_TABLES = [
    "chats", "messages", "chat_turns", "attachments", "message_attachments",
    "thread_summaries", "chat_vector_stores", "quota_usage", "message_reactions",
]


def test_server_is_healthy_and_serves_the_gear(server, api):
    assert server.alive()
    assert api.get("/v1/models").status_code == 200


def test_p1_tables_are_created_by_the_gear_migrations(mini_chat_db: GearDb):
    tables = set(mini_chat_db.tables())
    missing = [t for t in P1_TABLES if t not in tables]
    assert not missing, f"tables missing from the gear database: {missing}; have {sorted(tables)}"


def test_no_mcp_tables_exist(mini_chat_db: GearDb):
    tables = mini_chat_db.tables()
    assert not [t for t in tables if t.startswith("mcp") or t in ("role_mcp_servers",)], tables
    assert "tenant_closure" not in tables


def test_outbox_tables_are_created_with_the_gear_migrations(mini_chat_db: GearDb):
    outbox = [t for t in mini_chat_db.tables() if "outbox" in t]
    assert any("incoming" in t for t in outbox), outbox
    assert any("outgoing" in t for t in outbox), outbox


def test_listing_models_is_read_only(api, mini_chat_db: GearDb):
    # The Models API reads the catalog from the policy plugin; it must not create chats,
    # turns or quota rows (counts are compared, so other slices' tests may share the session).
    tables = ("chats", "messages", "chat_turns", "quota_usage")
    before = {t: mini_chat_db.count(t) for t in tables}
    api.get("/v1/models")
    api.get(f"/v1/models/{catalog.PREMIUM_ID}")
    assert {t: mini_chat_db.count(t) for t in tables} == before


def test_quota_usage_has_the_bucket_unique_key(mini_chat_db: GearDb):
    cols = mini_chat_db.columns("quota_usage")
    for col in ("tenant_id", "user_id", "period_type", "period_start", "bucket",
                "spent_credits_micro", "reserved_credits_micro"):
        assert col in cols, cols


def test_the_configured_catalog_reaches_the_api_through_the_static_plugin(api):
    ids = {i["model_id"] for i in api.get("/v1/models").json()["items"]}
    assert ids == set(catalog.ENABLED_IDS)


def test_provider_secret_never_appears_in_api_responses(api):
    assert OPENAI_SECRET_VALUE not in api.get("/v1/models").text
