# Mini Chat - Quickstart

Mini Chat is a multi-tenant AI chat gear: users create chats, send messages
and receive streamed (SSE) responses, upload documents and images, and get
answers grounded in their content. The gear enforces per-user credit quotas
with a premium→standard downgrade cascade, and emits usage and audit events
through the transactional outbox.

**Features:**

- Chat CRUD with cursor-paginated history (`GET /mini-chat/v1/chats`)
- Real-time streamed responses (`POST /mini-chat/v1/chats/{id}/messages:stream`)
- Document upload with provider vector-store indexing and image attachments
  (`POST /mini-chat/v1/chats/{id}/attachments`)
- Turn retry/edit/delete and status recovery
  (`/mini-chat/v1/chats/{id}/turns/{request_id}`)
- Message reactions, model catalog and quota status APIs
- Quota enforcement with reserve/settle, kill switches and downgrade cascade

Full API documentation: <http://127.0.0.1:8087/cf/docs>

## Examples

### Create a chat and send a message

```bash
curl -s -X POST http://127.0.0.1:8087/mini-chat/v1/chats \
  -H 'content-type: application/json' -H 'authorization: Bearer dev' \
  -d '{"title": "Demo"}'

curl -N -X POST http://127.0.0.1:8087/mini-chat/v1/chats/<chat-id>/messages:stream \
  -H 'content-type: application/json' -H 'authorization: Bearer dev' \
  -d '{"content": "Hello!"}'
```

### Upload a document

```bash
curl -s -X POST http://127.0.0.1:8087/mini-chat/v1/chats/<chat-id>/attachments \
  -H 'authorization: Bearer dev' -F 'file=@report.pdf;application/pdf'
```

For additional endpoints, see <http://127.0.0.1:8087/cf/docs>.
