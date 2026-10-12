"""``GET /v1/chats/{id}/messages`` black-box: the message list contract (identity,
attachments, reaction and optional fields), ordering, OData filtering and cursor pagination.

Sending messages belongs to a later slice, so messages, attachments and reactions are seeded
directly in the SQLite database (UUIDs as 16-byte blobs, DESIGN 3.7).

Source: DESIGN 3.3 "List Messages" (+ ``request_id`` semantics and the NULL ``request_id``
implementation guard), 3.7 ``messages`` / ``message_attachments`` / ``attachments`` /
``message_reactions``, the REST error table, OpenAPI ``MiniChatMessageDto`` /
``Page_MiniChatMessageDto`` / ``AttachmentSummaryDto`` / ``ImgThumbnailDto``.
"""

from __future__ import annotations

import uuid

import pytest

from .support import (
    DAY,
    MIXED_WIDTH_INSTANTS,
    RESOURCE_TYPES,
    TENANT_A_ID,
    THUMBNAIL_BYTES,
    THUMBNAIL_HEIGHT,
    THUMBNAIL_WIDTH,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    TENANT_B_ID,
    assert_problem,
    decode_thumbnail,
    iso,
    list_all,
    parse_ts,
    qs,
    wire_ts,
)

MESSAGE_KEYS = {"id", "request_id", "role", "content", "attachments", "my_reaction",
                "created_at", "model", "input_tokens", "output_tokens"}
REQUIRED_KEYS = {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}
ODATA_TYPE = "gts.cf.core.odata.query.v1~"


def ids(items):
    return [i["id"] for i in items]


def by_id(items, message_id):
    (found,) = [m for m in items if m["id"] == message_id]
    return found


def new_chat(seed, **kwargs):
    return seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, **kwargs)


def message(seed, chat, role, **kwargs):
    return seed.insert_message(tenant=TENANT_A_ID, chat=chat, role=role, **kwargs)


def listing(api, chat, user="a", **query):
    return api.get(f"/v1/chats/{chat}/messages" + qs(**query), user=user)


def history(seed):
    chat = new_chat(seed)
    request = str(uuid.uuid4())
    user_msg = message(seed, chat, "user", content="What is Rust?", request_id=request, created=10)
    assistant = message(seed, chat, "assistant", content="A systems language.",
                        request_id=request, created=11, model="mock-premium",
                        input_tokens=120, output_tokens=45)
    system = message(seed, chat, "system", content="summary", created=12)
    return chat, user_msg, assistant, system


def has_violation(problem, field, reason):
    return any(v.get("field") == field and v.get("reason") == reason
               for v in problem["context"]["field_violations"])


def assert_odata_problem(resp, field, reason):
    problem = assert_problem(resp, 400, "invalid_argument")
    assert problem["context"]["resource_type"] == ODATA_TYPE, problem
    assert has_violation(problem, field, reason), problem


# --------------------------------------------------------------------------- #
# envelope and item contract
# --------------------------------------------------------------------------- #


@pytest.mark.smoke
def test_list_returns_a_page_of_messages_in_chronological_order(api, seed):
    chat, user_msg, assistant, system = history(seed)

    resp = listing(api, chat)

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    page = resp.json()
    assert set(page) == {"items", "page_info"}
    assert page["page_info"]["limit"] == 20
    assert not page["page_info"].get("next_cursor")
    assert not page["page_info"].get("prev_cursor")
    assert ids(page["items"]) == [user_msg, assistant, system]


def test_list_of_a_chat_without_messages_is_an_empty_page(api, seed):
    page = listing(api, new_chat(seed)).json()
    assert page["items"] == []
    assert page["page_info"]["limit"] == 20


def test_every_message_has_the_required_fields_and_only_documented_ones(api, seed):
    chat, *_ = history(seed)
    for item in listing(api, chat).json()["items"]:
        keys = set(item)
        assert REQUIRED_KEYS <= keys, f"missing {REQUIRED_KEYS - keys} in {item}"
        assert keys <= MESSAGE_KEYS, f"undocumented fields exposed: {keys - MESSAGE_KEYS}"


def test_message_values_come_from_the_stored_rows(api, seed):
    chat, user_msg, assistant, system = history(seed)
    items = listing(api, chat).json()["items"]

    u = by_id(items, user_msg)
    assert (u["role"], u["content"]) == ("user", "What is Rust?")
    assert parse_ts(u["created_at"]) == parse_ts(iso(10))
    a = by_id(items, assistant)
    assert (a["role"], a["content"]) == ("assistant", "A systems language.")
    assert parse_ts(a["created_at"]) == parse_ts(iso(11))
    s = by_id(items, system)
    assert (s["role"], s["content"]) == ("system", "summary")


def test_request_id_is_always_present_and_shared_by_the_messages_of_a_turn(api, seed):
    chat, user_msg, assistant, system = history(seed)
    items = listing(api, chat).json()["items"]
    for item in items:
        assert isinstance(item["request_id"], str)
        uuid.UUID(item["request_id"])
    assert by_id(items, user_msg)["request_id"] == by_id(items, assistant)["request_id"]
    assert by_id(items, system)["request_id"] != by_id(items, user_msg)["request_id"]


def test_a_stored_null_request_id_fails_the_request_with_an_internal_error(api, seed):
    chat = new_chat(seed)
    message(seed, chat, "user", created=1)
    message(seed, chat, "assistant", request_id=None, created=2)

    resp = listing(api, chat)

    problem = assert_problem(resp, 500, "internal")
    assert "items" not in resp.json(), "a message is never serialized with a null request_id"
    assert "request_id" not in resp.text, problem


def test_model_and_token_counts_appear_only_when_present(api, seed):
    chat, user_msg, assistant, _system = history(seed)
    items = listing(api, chat).json()["items"]
    a = by_id(items, assistant)
    assert (a["model"], a["input_tokens"], a["output_tokens"]) == ("mock-premium", 120, 45)
    u = by_id(items, user_msg)
    for key in ("model", "input_tokens", "output_tokens"):
        assert key not in u, f"`{key}` is omitted when absent/0: {u}"


def test_zero_token_counts_are_omitted_even_on_assistant_messages(api, seed):
    chat = new_chat(seed)
    only_input = message(seed, chat, "assistant", model="mock-standard", input_tokens=7,
                         created=1)
    none = message(seed, chat, "assistant", created=2)
    items = listing(api, chat).json()["items"]
    a = by_id(items, only_input)
    assert a["input_tokens"] == 7 and "output_tokens" not in a
    n = by_id(items, none)
    assert not ({"model", "input_tokens", "output_tokens"} & set(n)), n


# --------------------------------------------------------------------------- #
# attachments[]
# --------------------------------------------------------------------------- #


def test_attachments_is_always_an_array_empty_when_there_are_none(api, seed):
    chat, *_ = history(seed)
    for item in listing(api, chat).json()["items"]:
        assert item["attachments"] == []


def test_attachment_summaries_list_the_linked_non_deleted_attachments(api, seed):
    chat = new_chat(seed)
    msg = message(seed, chat, "user", created=1)
    plain = message(seed, chat, "user", created=2)
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="report.pdf")
    img = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="photo.png", kind="image", thumbnail=True)
    removed = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                     filename="removed.pdf", deleted=True)
    seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="unlinked.pdf")
    for attachment in (doc, img, removed):
        seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=msg, attachment=attachment)

    items = listing(api, chat).json()["items"]

    listed = sorted(a["attachment_id"] for a in by_id(items, msg)["attachments"])
    assert listed == sorted([doc, img]), "soft-deleted and unlinked attachments are not listed"
    assert by_id(items, plain)["attachments"] == []


def test_attachment_summary_has_the_documented_fields_and_nothing_internal(api, seed):
    chat = new_chat(seed)
    msg = message(seed, chat, "user")
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="report.pdf")
    seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=msg, attachment=doc)

    (summary,) = by_id(listing(api, chat).json()["items"], msg)["attachments"]

    assert summary["attachment_id"] == doc
    assert (summary["kind"], summary["filename"], summary["status"]) == (
        "document", "report.pdf", "ready")
    assert set(summary) <= {"attachment_id", "kind", "filename", "status", "img_thumbnail"}
    assert summary.get("img_thumbnail") is None, "a document has no thumbnail"
    assert "file-" not in str(summary), "provider file ids never leave the service"


def test_ready_image_attachments_carry_the_thumbnail(api, seed):
    chat = new_chat(seed)
    msg = message(seed, chat, "user")
    img = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="photo.png", kind="image", thumbnail=True)
    seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=msg, attachment=img)

    (summary,) = by_id(listing(api, chat).json()["items"], msg)["attachments"]

    assert (summary["kind"], summary["status"]) == ("image", "ready")
    thumb = summary["img_thumbnail"]
    assert set(thumb) == {"content_type", "width", "height", "data_base64"}
    assert thumb["content_type"] == "image/webp"
    assert (thumb["width"], thumb["height"]) == (THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT)
    assert decode_thumbnail(thumb) == THUMBNAIL_BYTES


@pytest.mark.parametrize("status", ["pending", "uploaded", "failed"])
def test_thumbnails_are_present_only_for_ready_images(api, seed, status):
    chat = new_chat(seed)
    msg = message(seed, chat, "user")
    img = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="img.png", kind="image", status=status, thumbnail=True)
    seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=msg, attachment=img)

    (summary,) = by_id(listing(api, chat).json()["items"], msg)["attachments"]

    assert summary["status"] == status
    assert summary.get("img_thumbnail") is None


def test_one_attachment_can_be_listed_on_several_messages(api, seed):
    chat = new_chat(seed)
    first = message(seed, chat, "user", created=1)
    second = message(seed, chat, "user", created=2)
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="shared.pdf")
    for msg in (first, second):
        seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=msg, attachment=doc)
    items = listing(api, chat).json()["items"]
    for msg in (first, second):
        assert by_id(items, msg)["attachments"][0]["attachment_id"] == doc


# --------------------------------------------------------------------------- #
# my_reaction
# --------------------------------------------------------------------------- #


def test_my_reaction_is_always_present_and_null_without_a_reaction(api, seed):
    chat, *_ = history(seed)
    for item in listing(api, chat).json()["items"]:
        assert "my_reaction" in item
        assert item["my_reaction"] is None


def test_my_reaction_shows_the_callers_reaction_on_assistant_messages(api, seed):
    chat = new_chat(seed)
    liked = message(seed, chat, "assistant", created=1)
    disliked = message(seed, chat, "assistant", created=2)
    untouched = message(seed, chat, "assistant", created=3)
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=liked, reaction="like")
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=disliked, reaction="dislike")

    items = listing(api, chat).json()["items"]

    assert by_id(items, liked)["my_reaction"] == "like"
    assert by_id(items, disliked)["my_reaction"] == "dislike"
    assert by_id(items, untouched)["my_reaction"] is None


def test_my_reaction_is_always_null_for_user_and_system_messages(api, seed):
    chat = new_chat(seed)
    user_msg = message(seed, chat, "user", created=1)
    system = message(seed, chat, "system", created=2)
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=user_msg, reaction="like")
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=system, reaction="dislike")

    items = listing(api, chat).json()["items"]

    assert by_id(items, user_msg)["my_reaction"] is None
    assert by_id(items, system)["my_reaction"] is None


def test_my_reaction_never_shows_another_users_reaction(api, seed):
    chat = new_chat(seed)
    msg = message(seed, chat, "assistant")
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, message=msg,
                         reaction="dislike")
    seed.insert_reaction(tenant=TENANT_B_ID, user=USER_B_ID, message=msg, reaction="like")
    assert by_id(listing(api, chat).json()["items"], msg)["my_reaction"] is None

    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=msg, reaction="like")
    assert by_id(listing(api, chat).json()["items"], msg)["my_reaction"] == "like"


# --------------------------------------------------------------------------- #
# what is listed
# --------------------------------------------------------------------------- #


def test_soft_deleted_messages_are_not_listed(api, seed):
    chat = new_chat(seed)
    kept = message(seed, chat, "user", created=1)
    message(seed, chat, "assistant", created=2, deleted=True)
    assert ids(listing(api, chat).json()["items"]) == [kept]


def test_messages_of_other_chats_are_not_listed(api, seed):
    chat, other = new_chat(seed), new_chat(seed)
    mine = message(seed, chat, "user", created=1)
    message(seed, other, "user", created=1)
    assert ids(listing(api, chat).json()["items"]) == [mine]


def test_child_queries_are_not_blanked_by_the_owner_narrowed_chat_scope(api, seed):
    chat, user_msg, assistant, _system = history(seed)
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="d.pdf")
    seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=user_msg, attachment=doc)
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=assistant, reaction="like")

    items = listing(api, chat).json()["items"]

    assert len(items) == 3
    assert len(by_id(items, user_msg)["attachments"]) == 1
    assert by_id(items, assistant)["my_reaction"] == "like"


@pytest.mark.parametrize("deleted", [False, True])
def test_unknown_or_deleted_chat_is_404_for_the_chat_resource(api, seed, deleted):
    chat = new_chat(seed, deleted=True) if deleted else str(uuid.uuid4())
    problem = assert_problem(listing(api, chat), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


# --------------------------------------------------------------------------- #
# ordering and pagination
# --------------------------------------------------------------------------- #


def numbered_chat(seed, count):
    """A chat with ``count`` messages (user, assistant, user, ...) created 10 s apart."""
    chat = new_chat(seed)
    out = [message(seed, chat, "user" if n % 2 else "assistant", content=f"m{n}", created=n * 10)
           for n in range(1, count + 1)]
    return chat, out


def test_default_order_is_created_at_ascending_regardless_of_insertion_order(api, seed):
    chat = new_chat(seed)
    late = message(seed, chat, "user", created=300)
    early = message(seed, chat, "user", created=100)
    mid = message(seed, chat, "user", created=200)
    assert ids(listing(api, chat).json()["items"]) == [early, mid, late]


def test_messages_with_the_same_timestamp_are_ordered_deterministically_across_pages(api, seed):
    chat = new_chat(seed)
    for _ in range(5):
        message(seed, chat, "user", created=50)
    full = ids(list_all(api, f"/v1/chats/{chat}/messages", page_size=100))
    assert len(full) == 5
    assert ids(list_all(api, f"/v1/chats/{chat}/messages", page_size=2)) == full


def test_limit_defaults_to_20_and_is_clamped_to_100(api, seed):
    chat, _ = numbered_chat(seed, 105)

    default = listing(api, chat).json()
    assert len(default["items"]) == 20 and default["page_info"]["limit"] == 20
    assert default["page_info"].get("next_cursor")

    resp = listing(api, chat, limit=500)
    assert resp.status_code == 200, resp.text
    clamped = resp.json()
    assert len(clamped["items"]) == 100 and clamped["page_info"]["limit"] == 100

    three = listing(api, chat, limit=3).json()
    assert len(three["items"]) == 3 and three["page_info"]["limit"] == 3


def test_limit_zero_is_400_invalid_limit(api, seed):
    chat, _ = numbered_chat(seed, 2)
    assert_odata_problem(listing(api, chat, limit=0), "$top", "INVALID_LIMIT")


@pytest.mark.parametrize("limit", ["abc", "-1"])
def test_non_numeric_limit_is_a_400_odata_error(api, seed, limit):
    chat, _ = numbered_chat(seed, 2)
    problem = assert_problem(listing(api, chat, limit=limit), 400, "invalid_argument")
    assert problem["context"]["resource_type"] == ODATA_TYPE


def test_cursor_pagination_walks_forward_and_backward(api, seed):
    chat, msgs = numbered_chat(seed, 5)

    p1 = listing(api, chat, limit=2).json()
    assert ids(p1["items"]) == msgs[0:2]
    assert not p1["page_info"].get("prev_cursor")
    next1 = p1["page_info"]["next_cursor"]

    p2 = listing(api, chat, limit=2, cursor=next1).json()
    assert ids(p2["items"]) == msgs[2:4]
    prev2, next2 = p2["page_info"]["prev_cursor"], p2["page_info"]["next_cursor"]
    assert prev2 and next2

    p3 = listing(api, chat, limit=2, cursor=next2).json()
    assert ids(p3["items"]) == msgs[4:5]
    assert not p3["page_info"].get("next_cursor")
    prev3 = p3["page_info"]["prev_cursor"]

    assert ids(listing(api, chat, limit=2, cursor=prev3).json()["items"]) == msgs[2:4]
    assert ids(listing(api, chat, limit=2, cursor=prev2).json()["items"]) == msgs[0:2]


@pytest.mark.parametrize("cursor", ["not-a-cursor!!", "e30", "AAAA"])
def test_malformed_cursor_is_400_invalid_cursor(api, seed, cursor):
    chat, _ = numbered_chat(seed, 2)
    assert_odata_problem(listing(api, chat, cursor=cursor), "cursor", "INVALID_CURSOR")


def test_cursor_combined_with_orderby_is_400_order_with_cursor(api, seed):
    chat, _ = numbered_chat(seed, 4)
    cursor = listing(api, chat, limit=1).json()["page_info"]["next_cursor"]
    resp = listing(api, chat, limit=1, cursor=cursor, _orderby="created_at desc")
    assert_odata_problem(resp, "cursor", "ORDER_WITH_CURSOR")


def test_cursor_of_another_filter_is_400_filter_mismatch(api, seed):
    chat, _ = numbered_chat(seed, 6)
    first = listing(api, chat, limit=1, _filter="role eq 'user'").json()
    cursor = first["page_info"]["next_cursor"]
    resp = listing(api, chat, limit=1, cursor=cursor, _filter="role eq 'assistant'")
    assert_odata_problem(resp, "cursor", "FILTER_MISMATCH")


# ---- $orderby ---------------------------------------------------------------


def test_orderby_created_at_descending(api, seed):
    chat, msgs = numbered_chat(seed, 4)
    resp = listing(api, chat, _orderby="created_at desc")
    assert resp.status_code == 200, resp.text
    assert ids(resp.json()["items"]) == list(reversed(msgs))


def test_orderby_role_groups_messages_by_role(api, seed):
    chat, msgs = numbered_chat(seed, 4)  # user, assistant, user, assistant
    asc = listing(api, chat, _orderby="role asc,created_at asc").json()["items"]
    assert ids(asc) == [msgs[1], msgs[3], msgs[0], msgs[2]]
    desc = listing(api, chat, _orderby="role desc,created_at asc").json()["items"]
    assert ids(desc) == [msgs[0], msgs[2], msgs[1], msgs[3]]


@pytest.mark.parametrize("orderby", ["content asc", "request_id asc", "model desc",
                                     "my_reaction asc", "nope", "created_at sideways"])
def test_orderby_unknown_field_is_400_invalid_orderby_field(api, seed, orderby):
    chat, _ = numbered_chat(seed, 2)
    assert_odata_problem(listing(api, chat, _orderby=orderby), "$orderby", "INVALID_ORDERBY_FIELD")


# ---- $filter ----------------------------------------------------------------


def filtered(api, chat, expr):
    resp = listing(api, chat, _filter=expr)
    assert resp.status_code == 200, f"{expr}: {resp.text}"
    return ids(resp.json()["items"])


def test_filter_by_role(api, seed):
    chat, m = numbered_chat(seed, 4)
    assert filtered(api, chat, "role eq 'assistant'") == [m[1], m[3]]
    assert filtered(api, chat, "role ne 'assistant'") == [m[0], m[2]]
    assert filtered(api, chat, "contains(role,'assist')") == [m[1], m[3]]
    assert filtered(api, chat, "startswith(role,'us')") == [m[0], m[2]]
    assert filtered(api, chat, "endswith(role,'ant')") == [m[1], m[3]]
    assert filtered(api, chat, "role in ('user','system')") == [m[0], m[2]]
    assert filtered(api, chat, "role eq 'system'") == []


def test_filter_by_created_at(api, seed):
    chat = new_chat(seed)
    m = [message(seed, chat, "user", created=n * DAY) for n in range(1, 5)]  # 2025-01-02 .. 01-05
    assert filtered(api, chat, "created_at gt 2025-01-03T12:00:00Z") == m[2:]
    assert filtered(api, chat, "created_at lt 2025-01-03T12:00:00Z") == m[:2]
    assert filtered(api, chat, "created_at ge 2025-01-02T12:00:00Z and "
                               "created_at le 2025-01-04T12:00:00Z") == m[1:3]


def test_filter_by_id(api, seed):
    chat, m = numbered_chat(seed, 3)
    assert filtered(api, chat, f"id eq {m[1]}") == [m[1]]
    assert filtered(api, chat, f"id ne {m[1]}") == [m[0], m[2]]
    assert filtered(api, chat, f"id in ({m[0]},{m[2]})") == [m[0], m[2]]


def test_filter_is_limited_to_the_chat_and_to_live_messages(api, seed):
    chat, m = numbered_chat(seed, 2)
    other = new_chat(seed)
    foreign = message(seed, other, "user", created=5)
    message(seed, chat, "user", created=6, deleted=True)
    assert filtered(api, chat, "role eq 'user'") == [m[0]]
    assert filtered(api, chat, f"id eq {foreign}") == []


@pytest.mark.parametrize("expr", [
    "content eq 'm1'", "request_id eq 5", "chat_id eq 5", "model eq 'x'", "role eq",
    "role eqq 'user'", "role eq 'unterminated", "created_at gt notadate", "((role eq 'user')"])
def test_filter_with_an_unknown_field_or_bad_syntax_is_400_invalid_filter(api, seed, expr):
    chat, _ = numbered_chat(seed, 2)
    assert_odata_problem(listing(api, chat, _filter=expr), "$filter", "INVALID_FILTER")


def test_select_is_accepted_and_ignored_and_a_bad_select_is_400(api, seed):
    chat, *_ = history(seed)
    full = listing(api, chat).json()
    resp = listing(api, chat, _select="id,content")
    assert resp.status_code == 200, resp.text
    assert resp.json() == full
    assert_odata_problem(listing(api, chat, _select=""), "$select", "INVALID_SELECT")


@pytest.mark.parametrize("option", ["$skip", "$count", "$bogus"])
def test_unsupported_odata_options_are_400(api, seed, option):
    chat, _ = numbered_chat(seed, 2)
    resp = api.get(f"/v1/chats/{chat}/messages?{option}=1")
    assert_odata_problem(resp, option, "UNSUPPORTED_QUERY_PARAM")


def test_non_uuid_chat_id_is_400_invalid_path_params(api):
    problem = assert_problem(api.get("/v1/chats/not-a-uuid/messages"), 400, "invalid_argument")
    assert any(v.get("reason") == "invalid_path_params"
               for v in problem["context"]["field_violations"]), problem


# ---- timestamps: exact $filter and chronological order ----------------------


def mixed_width_messages(seed):
    """Messages of one chat whose ``created_at`` share one second with different fraction
    widths; message ``n`` is the n-th instant ascending. Seeded out of order."""
    chat = new_chat(seed)
    created = {}
    for index in [3, 0, 4, 2, 1]:
        secs, micros = MIXED_WIDTH_INSTANTS[index]
        created[index] = message(seed, chat, "user", created=secs, created_micros=micros)
    return chat, [created[i] for i in range(5)]


def test_created_at_filters_are_exact_on_the_timestamps_the_api_returns(api, seed):
    chat, m = mixed_width_messages(seed)
    items = {i["id"]: i for i in listing(api, chat, limit=100).json()["items"]}
    stamps = [items[x]["created_at"] for x in m]
    for stamp, (secs, micros) in zip(stamps, MIXED_WIDTH_INSTANTS):
        assert parse_ts(stamp) == parse_ts(wire_ts(secs, micros))

    everyone = range(5)

    def oldest_first(indexes):
        return [m[i] for i in sorted(indexes)]

    for i, stamp in enumerate(stamps):
        assert filtered(api, chat, f"created_at eq {stamp}") == oldest_first([i]), f"eq {stamp}"
        assert filtered(api, chat, f"created_at ne {stamp}") == oldest_first(
            [j for j in everyone if j != i]), f"ne {stamp}"
        assert filtered(api, chat, f"created_at gt {stamp}") == oldest_first(
            [j for j in everyone if j > i]), f"gt {stamp}"
        assert filtered(api, chat, f"created_at ge {stamp}") == oldest_first(
            [j for j in everyone if j >= i]), f"ge {stamp}"
        assert filtered(api, chat, f"created_at lt {stamp}") == oldest_first(
            [j for j in everyone if j < i]), f"lt {stamp}"
        assert filtered(api, chat, f"created_at le {stamp}") == oldest_first(
            [j for j in everyone if j <= i]), f"le {stamp}"
    assert filtered(api, chat, f"created_at in ({stamps[0]},{stamps[2]},{stamps[4]})") == (
        oldest_first([0, 2, 4]))


def test_same_second_timestamps_of_different_fraction_widths_are_ordered_chronologically(
        api, seed):
    chat, m = mixed_width_messages(seed)
    assert ids(listing(api, chat).json()["items"]) == m
    for direction, expected in (("asc", m), ("desc", list(reversed(m)))):
        for page_size in (100, 1):
            walked = list_all(api, f"/v1/chats/{chat}/messages", page_size=page_size,
                              _orderby=f"created_at {direction}")
            assert ids(walked) == expected, f"created_at {direction}, pages of {page_size}"
