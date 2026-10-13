# Mini-chat OpenAPI reference (extracted from docs/api/api.json)

All paths are under the gear url_prefix `/mini-chat`. Error responses are `application/problem+json` (canonical Problem). 503 responses declare a `Retry-After` header.

## Operations

### GET /mini-chat/v1/chats

- operationId: `mini_chat.list_chats`; tags: ['Mini Chat Chats']
- params: [('limit', 'query', False), ('cursor', 'query', False), ('$filter', 'query', False), ('$orderby', 'query', False)]
- responses: {'200': ({'application/json': 'Page_ChatDetailDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}
- extensions: `{"x-odata-filter": {"allowedFields": {"id": ["eq", "ne", "in"], "title": ["eq", "ne", "contains", "startswith", "endswith", "in"], "updated_at": ["eq", "ne", "gt", "ge", "lt", "le", "in"]}}, "x-odata-orderby": {"allowedFields": ["updated_at asc", "updated_at desc", "id asc", "id desc", "title asc", "title desc"]}}`

### POST /mini-chat/v1/chats

- operationId: `mini_chat.create_chat`; tags: ['Mini Chat Chats']
- body: {"application/json": "#/components/schemas/CreateChatReq"}
- responses: {'201': ({'application/json': 'ChatDetailDto'}, ['Location']), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '422': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}
- extensions: `{"x-in-flight-limit": 100, "x-rate-limit-burst": 10, "x-rate-limit-rps": 10, "x-throttling-in-flight-limit-zone": "ifl_mini_chat_chat", "x-throttling-rate-limit-zone": "rl_mini_chat_chat"}`

### DELETE /mini-chat/v1/chats/{id}

- operationId: `mini_chat.delete_chat`; tags: ['Mini Chat Chats']
- params: [('id', 'path', True)]
- responses: {'204': ({}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/chats/{id}

- operationId: `mini_chat.get_chat`; tags: ['Mini Chat Chats']
- params: [('id', 'path', True)]
- responses: {'200': ({'application/json': 'ChatDetailDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### PATCH /mini-chat/v1/chats/{id}

- operationId: `mini_chat.update_chat`; tags: ['Mini Chat Chats']
- params: [('id', 'path', True)]
- body: {"application/json": "#/components/schemas/UpdateChatReq"}
- responses: {'200': ({'application/json': 'ChatDetailDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '422': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### POST /mini-chat/v1/chats/{id}/attachments

- operationId: `mini_chat.upload_attachment`; tags: ['Mini Chat Attachments']
- params: [('id', 'path', True)]
- body: {"multipart/form-data": {"properties": {"file": {"format": "binary", "type": "string"}}, "required": ["file"], "type": "object"}}
- responses: {'201': ({'application/json': 'AttachmentDetailDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '409': ({'application/problem+json': 'Problem'}, []), '429': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### DELETE /mini-chat/v1/chats/{id}/attachments/{attachment_id}

- operationId: `mini_chat.delete_attachment`; tags: ['Mini Chat Attachments']
- params: [('id', 'path', True), ('attachment_id', 'path', True)]
- responses: {'204': ({}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '409': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/chats/{id}/attachments/{attachment_id}

- operationId: `mini_chat.get_attachment`; tags: ['Mini Chat Attachments']
- params: [('id', 'path', True), ('attachment_id', 'path', True)]
- responses: {'200': ({'application/json': 'AttachmentDetailDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/chats/{id}/messages

- operationId: `mini_chat.list_messages`; tags: ['Mini Chat Messages']
- params: [('id', 'path', True), ('limit', 'query', False), ('cursor', 'query', False), ('$filter', 'query', False), ('$orderby', 'query', False)]
- responses: {'200': ({'application/json': 'Page_MiniChatMessageDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}
- extensions: `{"x-odata-filter": {"allowedFields": {"created_at": ["eq", "ne", "gt", "ge", "lt", "le", "in"], "id": ["eq", "ne", "in"], "role": ["eq", "ne", "contains", "startswith", "endswith", "in"]}}, "x-odata-orderby": {"allowedFields": ["created_at asc", "created_at desc", "id asc", "id desc", "role asc", "role desc"]}}`

### DELETE /mini-chat/v1/chats/{id}/messages/{msg_id}/reaction

- operationId: `mini_chat.delete_reaction`; tags: ['Mini Chat Reactions']
- params: [('id', 'path', True), ('msg_id', 'path', True)]
- responses: {'204': ({}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### PUT /mini-chat/v1/chats/{id}/messages/{msg_id}/reaction

- operationId: `mini_chat.put_reaction`; tags: ['Mini Chat Reactions']
- params: [('id', 'path', True), ('msg_id', 'path', True)]
- body: {"application/json": "#/components/schemas/SetReactionReq"}
- responses: {'200': ({'application/json': 'MiniChatReactionDto'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '422': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### POST /mini-chat/v1/chats/{id}/messages:stream

- operationId: `mini_chat.stream_message`; tags: ['Mini Chat Messages']
- params: [('id', 'path', True)]
- body: {"application/json": "#/components/schemas/StreamMessageRequest"}
- responses: {'200': ({'text/event-stream': 'MiniChatSseEvent'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '409': ({'application/problem+json': 'Problem'}, []), '422': ({'application/problem+json': 'Problem'}, []), '429': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### DELETE /mini-chat/v1/chats/{id}/turns/{request_id}

- operationId: `mini_chat.delete_turn`; tags: ['Mini Chat Turns']
- params: [('id', 'path', True), ('request_id', 'path', True)]
- responses: {'204': ({}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '409': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/chats/{id}/turns/{request_id}

- operationId: `mini_chat.get_turn`; tags: ['Mini Chat Turns']
- params: [('id', 'path', True), ('request_id', 'path', True)]
- responses: {'200': ({'application/json': 'TurnStatusResponse'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### PATCH /mini-chat/v1/chats/{id}/turns/{request_id}

- operationId: `mini_chat.edit_turn`; tags: ['Mini Chat Turns']
- params: [('id', 'path', True), ('request_id', 'path', True)]
- body: {"application/json": "#/components/schemas/EditTurnRequest"}
- responses: {'200': ({'text/event-stream': 'MiniChatSseEvent'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '409': ({'application/problem+json': 'Problem'}, []), '422': ({'application/problem+json': 'Problem'}, []), '429': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### POST /mini-chat/v1/chats/{id}/turns/{request_id}/retry

- operationId: `mini_chat.retry_turn`; tags: ['Mini Chat Turns']
- params: [('id', 'path', True), ('request_id', 'path', True)]
- responses: {'200': ({'text/event-stream': 'MiniChatSseEvent'}, []), '400': ({'application/problem+json': 'Problem'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '409': ({'application/problem+json': 'Problem'}, []), '429': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/models

- operationId: `mini_chat.list_models`; tags: ['Mini Chat Models']
- responses: {'200': ({'application/json': 'ModelListDto'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/models/{id}

- operationId: `mini_chat.get_model`; tags: ['Mini Chat Models']
- params: [('id', 'path', True)]
- responses: {'200': ({'application/json': 'ModelDto'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '404': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

### GET /mini-chat/v1/quota/status

- operationId: `mini_chat.get_quota_status`; tags: ['Mini Chat Quotas']
- responses: {'200': ({'application/json': 'QuotaStatusResponse'}, []), '401': ({'application/problem+json': 'Problem'}, []), '403': ({'application/problem+json': 'Problem'}, []), '500': ({'application/problem+json': 'Problem'}, []), '503': ({'application/problem+json': 'Problem'}, ['Retry-After'])}

## Schemas

### AttachmentDetailDto

```json
{
 "description": "Full attachment details returned by the GET attachment endpoint.",
 "properties": {
  "content_type": {
   "type": "string"
  },
  "created_at": {
   "format": "date-time",
   "type": "string"
  },
  "doc_summary": {
   "type": [
    "string",
    "null"
   ]
  },
  "error_code": {
   "type": [
    "string",
    "null"
   ]
  },
  "filename": {
   "type": "string"
  },
  "id": {
   "format": "uuid",
   "type": "string"
  },
  "img_thumbnail": {
   "oneOf": [
    {
     "type": "null"
    },
    {
     "$ref": "#/components/schemas/ImgThumbnailDto"
    }
   ]
  },
  "kind": {
   "$ref": "#/components/schemas/AttachmentKindDto"
  },
  "size_bytes": {
   "format": "int64",
   "type": "integer"
  },
  "status": {
   "$ref": "#/components/schemas/AttachmentStatusDto"
  },
  "summary_updated_at": {
   "format": "date-time",
   "type": [
    "string",
    "null"
   ]
  }
 },
 "required": [
  "id",
  "filename",
  "content_type",
  "size_bytes",
  "status",
  "kind",
  "created_at"
 ],
 "type": "object"
}
```

### AttachmentKindDto

```json
{
 "description": "Attachment kind.",
 "enum": [
  "document",
  "image"
 ],
 "type": "string"
}
```

### AttachmentStatusDto

```json
{
 "description": "Attachment lifecycle status.",
 "enum": [
  "pending",
  "uploaded",
  "ready",
  "failed"
 ],
 "type": "string"
}
```

### AttachmentSummaryDto

```json
{
 "description": "Lightweight attachment metadata embedded in Message responses.",
 "properties": {
  "attachment_id": {
   "format": "uuid",
   "type": "string"
  },
  "filename": {
   "type": "string"
  },
  "img_thumbnail": {
   "oneOf": [
    {
     "type": "null"
    },
    {
     "$ref": "#/components/schemas/ImgThumbnailDto"
    }
   ]
  },
  "kind": {
   "$ref": "#/components/schemas/AttachmentKindDto"
  },
  "status": {
   "$ref": "#/components/schemas/AttachmentStatusDto"
  }
 },
 "required": [
  "attachment_id",
  "kind",
  "filename",
  "status"
 ],
 "type": "object"
}
```

### ChatDetailDto

```json
{
 "description": "Response DTO for chat details.",
 "properties": {
  "created_at": {
   "format": "date-time",
   "type": "string"
  },
  "id": {
   "format": "uuid",
   "type": "string"
  },
  "is_temporary": {
   "type": "boolean"
  },
  "message_count": {
   "format": "int64",
   "type": "integer"
  },
  "model": {
   "type": "string"
  },
  "title": {
   "type": [
    "string",
    "null"
   ]
  },
  "updated_at": {
   "format": "date-time",
   "type": "string"
  }
 },
 "required": [
  "id",
  "model",
  "is_temporary",
  "message_count",
  "created_at",
  "updated_at"
 ],
 "type": "object"
}
```

### Citation

```json
{
 "description": "A citation extracted from provider annotations.",
 "properties": {
  "attachment_id": {
   "description": "The attachment UUID (the provider file id before mapping).",
   "format": "uuid",
   "type": [
    "string",
    "null"
   ]
  },
  "score": {
   "format": "double",
   "type": [
    "number",
    "null"
   ]
  },
  "snippet": {
   "type": "string"
  },
  "source": {
   "$ref": "#/components/schemas/CitationSource"
  },
  "span": {
   "oneOf": [
    {
     "type": "null"
    },
    {
     "$ref": "#/components/schemas/TextSpan"
    }
   ]
  },
  "title": {
   "type": "string"
  },
  "url": {
   "type": [
    "string",
    "null"
   ]
  }
 },
 "required": [
  "source",
  "title",
  "snippet"
 ],
 "type": "object"
}
```

### CitationSource

```json
{
 "description": "Whether a citation came from a file or web search.",
 "enum": [
  "file",
  "web"
 ],
 "type": "string"
}
```

### CitationsData

```json
{
 "description": "Citations from provider annotations.",
 "properties": {
  "items": {
   "items": {
    "$ref": "#/components/schemas/Citation"
   },
   "type": "array"
  }
 },
 "required": [
  "items"
 ],
 "type": "object"
}
```

### CreateChatReq

```json
{
 "description": "Request DTO for creating a new chat.",
 "properties": {
  "model": {
   "type": [
    "string",
    "null"
   ]
  },
  "title": {
   "type": [
    "string",
    "null"
   ]
  }
 },
 "type": "object"
}
```

### DeltaData

```json
{
 "description": "Delta text chunk.",
 "properties": {
  "content": {
   "type": "string"
  },
  "type": {
   "$ref": "#/components/schemas/DeltaKind"
  }
 },
 "required": [
  "type",
  "content"
 ],
 "type": "object"
}
```

### DeltaKind

```json
{
 "description": "Kind of a `delta` chunk.",
 "enum": [
  "text",
  "reasoning"
 ],
 "type": "string"
}
```

### DoneData

```json
{
 "description": "Successful stream completion.",
 "properties": {
  "downgrade_from": {
   "type": [
    "string",
    "null"
   ]
  },
  "downgrade_reason": {
   "type": [
    "string",
    "null"
   ]
  },
  "effective_model": {
   "type": "string"
  },
  "quota_decision": {
   "$ref": "#/components/schemas/QuotaDecisionKind"
  },
  "quota_warnings": {
   "items": {
    "$ref": "#/components/schemas/QuotaWarning"
   },
   "type": [
    "array",
    "null"
   ]
  },
  "selected_model": {
   "type": "string"
  },
  "usage": {
   "$ref": "#/components/schemas/Usage"
  }
 },
 "required": [
  "usage",
  "effective_model",
  "selected_model",
  "quota_decision"
 ],
 "type": "object"
}
```

### EditTurnRequest

```json
{
 "description": "Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).",
 "properties": {
  "content": {
   "type": "string"
  }
 },
 "required": [
  "content"
 ],
 "type": "object"
}
```

### ErrorData

```json
{
 "description": "Stream error (terminal).",
 "properties": {
  "code": {
   "type": "string"
  },
  "message": {
   "type": "string"
  }
 },
 "required": [
  "code",
  "message"
 ],
 "type": "object"
}
```

### ImgThumbnailDto

```json
{
 "description": "Server-generated preview thumbnail for an image attachment.",
 "properties": {
  "content_type": {
   "type": "string"
  },
  "data_base64": {
   "type": "string"
  },
  "height": {
   "format": "int32",
   "type": "integer"
  },
  "width": {
   "format": "int32",
   "type": "integer"
  }
 },
 "required": [
  "content_type",
  "width",
  "height",
  "data_base64"
 ],
 "type": "object"
}
```

### MessageRoleDto

```json
{
 "description": "Message author role.",
 "enum": [
  "user",
  "assistant",
  "system"
 ],
 "type": "string"
}
```

### MiniChatMessageDto

```json
{
 "description": "Response DTO for a message in the list endpoint.\n\nAliased to `MiniChatMessageDto` in the `OpenAPI`` schema: `chat-engine` also\nexposes a `MessageDto`, and both gears register into the same api-gateway\n`OpenAPI`` registry, so the bare ident would collide in `components.schemas`.",
 "properties": {
  "attachments": {
   "items": {
    "$ref": "#/components/schemas/AttachmentSummaryDto"
   },
   "type": "array"
  },
  "content": {
   "type": "string"
  },
  "created_at": {
   "format": "date-time",
   "type": "string"
  },
  "id": {
   "format": "uuid",
   "type": "string"
  },
  "input_tokens": {
   "format": "int64",
   "type": [
    "integer",
    "null"
   ]
  },
  "model": {
   "type": [
    "string",
    "null"
   ]
  },
  "my_reaction": {
   "oneOf": [
    {
     "type": "null"
    },
    {
     "$ref": "#/components/schemas/ReactionKindDto",
     "description": "The caller's reaction to this message; `null` when there is none."
    }
   ]
  },
  "output_tokens": {
   "format": "int64",
   "type": [
    "integer",
    "null"
   ]
  },
  "request_id": {
   "format": "uuid",
   "type": "string"
  },
  "role": {
   "$ref": "#/components/schemas/MessageRoleDto"
  }
 },
 "required": [
  "id",
  "request_id",
  "role",
  "content",
  "attachments",
  "my_reaction",
  "created_at"
 ],
 "type": "object"
}
```

### MiniChatReactionDto

```json
{
 "description": "Response DTO for a reaction.",
 "properties": {
  "created_at": {
   "format": "date-time",
   "type": "string"
  },
  "message_id": {
   "format": "uuid",
   "type": "string"
  },
  "reaction": {
   "$ref": "#/components/schemas/ReactionKindDto"
  }
 },
 "required": [
  "message_id",
  "reaction",
  "created_at"
 ],
 "type": "object"
}
```

### MiniChatSseEvent

```json
{
 "description": "`OpenAPI` description of one SSE event of the `messages:stream`, retry and\nedit responses. Each event is sent as `event: <name>` and `data: <payload>`\n(JSON); `event` below is the SSE event name and `data` its payload. This\ntype only documents the wire format; `StreamEvent::into_sse_event` writes it.",
 "oneOf": [
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/StreamStartedData"
    },
    "event": {
     "enum": [
      "stream_started"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  },
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/PingData"
    },
    "event": {
     "enum": [
      "ping"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  },
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/DeltaData"
    },
    "event": {
     "enum": [
      "delta"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  },
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/ToolData"
    },
    "event": {
     "enum": [
      "tool"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  },
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/CitationsData"
    },
    "event": {
     "enum": [
      "citations"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  },
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/DoneData"
    },
    "event": {
     "enum": [
      "done"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  },
  {
   "properties": {
    "data": {
     "$ref": "#/components/schemas/ErrorData"
    },
    "event": {
     "enum": [
      "error"
     ],
     "type": "string"
    }
   },
   "required": [
    "data",
    "event"
   ],
   "type": "object"
  }
 ]
}
```

### ModelDto

```json
{
 "description": "Response DTO for a single model.",
 "properties": {
  "context_window": {
   "format": "int32",
   "minimum": 0,
   "type": "integer"
  },
  "description": {
   "type": [
    "string",
    "null"
   ]
  },
  "display_name": {
   "type": "string"
  },
  "model_id": {
   "type": "string"
  },
  "multimodal_capabilities": {
   "items": {
    "type": "string"
   },
   "type": "array"
  },
  "multiplier_display": {
   "type": "string"
  },
  "tier": {
   "$ref": "#/components/schemas/ModelTierDto"
  }
 },
 "required": [
  "model_id",
  "display_name",
  "tier",
  "multiplier_display",
  "multimodal_capabilities",
  "context_window"
 ],
 "type": "object"
}
```

### ModelListDto

```json
{
 "description": "Response DTO for the model list endpoint.",
 "properties": {
  "items": {
   "items": {
    "$ref": "#/components/schemas/ModelDto"
   },
   "type": "array"
  }
 },
 "required": [
  "items"
 ],
 "type": "object"
}
```

### ModelTierDto

```json
{
 "description": "Model tier.",
 "enum": [
  "standard",
  "premium"
 ],
 "type": "string"
}
```

### PageInfo

```json
{
 "properties": {
  "limit": {
   "format": "int64",
   "minimum": 0,
   "type": "integer"
  },
  "next_cursor": {
   "type": [
    "string",
    "null"
   ]
  },
  "prev_cursor": {
   "type": [
    "string",
    "null"
   ]
  }
 },
 "required": [
  "limit"
 ],
 "type": "object"
}
```

### Page_ChatDetailDto

```json
{
 "properties": {
  "items": {
   "items": {
    "$ref": "#/components/schemas/ChatDetailDto"
   },
   "type": "array"
  },
  "page_info": {
   "$ref": "#/components/schemas/PageInfo"
  }
 },
 "required": [
  "items",
  "page_info"
 ],
 "type": "object"
}
```

### Page_MiniChatMessageDto

```json
{
 "properties": {
  "items": {
   "items": {
    "$ref": "#/components/schemas/MiniChatMessageDto"
   },
   "type": "array"
  },
  "page_info": {
   "$ref": "#/components/schemas/PageInfo"
  }
 },
 "required": [
  "items",
  "page_info"
 ],
 "type": "object"
}
```

### PingData

```json
{
 "description": "Payload of the `ping` event: an empty JSON object (a unit struct would\nbe documented as `null`).",
 "type": "object"
}
```

### QuotaDecisionKind

```json
{
 "description": "Quota decision reported in `done`.",
 "enum": [
  "allow",
  "downgrade"
 ],
 "type": "string"
}
```

### QuotaPeriod

```json
{
 "description": "Quota period classification.",
 "enum": [
  "daily",
  "monthly"
 ],
 "type": "string"
}
```

### QuotaPeriodStatus

```json
{
 "properties": {
  "exhausted": {
   "type": "boolean"
  },
  "limit_credits_micro": {
   "format": "int64",
   "type": "integer"
  },
  "next_reset": {
   "format": "date-time",
   "type": "string"
  },
  "period": {
   "$ref": "#/components/schemas/QuotaPeriod"
  },
  "remaining_credits_micro": {
   "format": "int64",
   "type": "integer"
  },
  "remaining_percentage": {
   "format": "int32",
   "minimum": 0,
   "type": "integer"
  },
  "used_credits_micro": {
   "format": "int64",
   "type": "integer"
  },
  "warning": {
   "type": "boolean"
  }
 },
 "required": [
  "period",
  "limit_credits_micro",
  "used_credits_micro",
  "remaining_credits_micro",
  "remaining_percentage",
  "next_reset",
  "warning",
  "exhausted"
 ],
 "type": "object"
}
```

### QuotaStatusResponse

```json
{
 "properties": {
  "tiers": {
   "items": {
    "$ref": "#/components/schemas/QuotaTierStatus"
   },
   "type": "array"
  },
  "warning_threshold_pct": {
   "format": "int32",
   "minimum": 0,
   "type": "integer"
  }
 },
 "required": [
  "tiers",
  "warning_threshold_pct"
 ],
 "type": "object"
}
```

### QuotaTier

```json
{
 "description": "Quota tier classification.",
 "enum": [
  "premium",
  "total"
 ],
 "type": "string"
}
```

### QuotaTierStatus

```json
{
 "properties": {
  "periods": {
   "items": {
    "$ref": "#/components/schemas/QuotaPeriodStatus"
   },
   "type": "array"
  },
  "tier": {
   "$ref": "#/components/schemas/QuotaTier"
  }
 },
 "required": [
  "tier",
  "periods"
 ],
 "type": "object"
}
```

### QuotaWarning

```json
{
 "description": "Per-tier, per-period quota warning entry in the SSE `done` event.",
 "properties": {
  "exhausted": {
   "type": "boolean"
  },
  "next_reset": {
   "description": "RFC 3339 timestamp of the next quota-period reset.\nPresent when `warning` or `exhausted` is true; absent otherwise.",
   "format": "date-time",
   "type": [
    "string",
    "null"
   ]
  },
  "period": {
   "$ref": "#/components/schemas/QuotaPeriod"
  },
  "remaining_percentage": {
   "format": "int32",
   "minimum": 0,
   "type": "integer"
  },
  "tier": {
   "$ref": "#/components/schemas/QuotaTier"
  },
  "warning": {
   "type": "boolean"
  }
 },
 "required": [
  "tier",
  "period",
  "remaining_percentage",
  "warning",
  "exhausted"
 ],
 "type": "object"
}
```

### ReactionKindDto

```json
{
 "description": "Reaction value.",
 "enum": [
  "like",
  "dislike"
 ],
 "type": "string"
}
```

### SetReactionReq

```json
{
 "description": "Request DTO for setting a reaction.",
 "properties": {
  "reaction": {
   "type": "string"
  }
 },
 "required": [
  "reaction"
 ],
 "type": "object"
}
```

### StreamMessageRequest

```json
{
 "description": "Request body for `POST /v1/chats/{id}/messages:stream`.",
 "properties": {
  "attachment_ids": {
   "description": "Attachment IDs to include.",
   "items": {
    "format": "uuid",
    "type": "string"
   },
   "type": "array"
  },
  "content": {
   "description": "Message content (must be non-empty).",
   "type": "string"
  },
  "request_id": {
   "description": "Idempotency key: any UUID; generated by the server when omitted.",
   "format": "uuid",
   "type": [
    "string",
    "null"
   ]
  },
  "web_search": {
   "oneOf": [
    {
     "type": "null"
    },
    {
     "$ref": "#/components/schemas/WebSearchConfig",
     "description": "Web search configuration."
    }
   ]
  }
 },
 "required": [
  "content"
 ],
 "type": "object"
}
```

### StreamStartedData

```json
{
 "description": "Stream header event carrying the stream request ID and server-generated\nassistant message ID.\n\nEmitted as the first event in every SSE stream (both new generations and\nreplays). `is_new_turn` distinguishes replayed completed turns from live\ngenerations.",
 "properties": {
  "is_new_turn": {
   "description": "`true` for a live generation (new turn); `false` when the stream\nreplays an already-completed turn (idempotent replay).",
   "type": "boolean"
  },
  "message_id": {
   "format": "uuid",
   "type": "string"
  },
  "request_id": {
   "format": "uuid",
   "type": "string"
  },
  "thread_summary_applied": {
   "oneOf": [
    {
     "type": "null"
    },
    {
     "$ref": "#/components/schemas/ThreadSummaryInfo",
     "description": "Present when a thread summary is included in the context window.\nUI can display an informational indicator."
    }
   ]
  }
 },
 "required": [
  "request_id",
  "message_id",
  "is_new_turn"
 ],
 "type": "object"
}
```

### TextSpan

```json
{
 "description": "A character span within response text.",
 "properties": {
  "end": {
   "minimum": 0,
   "type": "integer"
  },
  "start": {
   "minimum": 0,
   "type": "integer"
  }
 },
 "required": [
  "start",
  "end"
 ],
 "type": "object"
}
```

### ThreadSummaryInfo

```json
{
 "description": "Metadata about a thread summary applied to the current turn's context.\n\nSent in the `stream_started` event when the conversation has an active\nthread summary. The UI can use this to show an informational banner.",
 "properties": {
  "token_estimate": {
   "description": "Estimated token cost of the summary in the context window.",
   "format": "int32",
   "minimum": 0,
   "type": "integer"
  }
 },
 "required": [
  "token_estimate"
 ],
 "type": "object"
}
```

### ToolData

```json
{
 "description": "Tool lifecycle event.",
 "properties": {
  "details": {
   "type": "object"
  },
  "name": {
   "type": "string"
  },
  "phase": {
   "$ref": "#/components/schemas/ToolPhase"
  }
 },
 "required": [
  "phase",
  "name",
  "details"
 ],
 "type": "object"
}
```

### ToolPhase

```json
{
 "description": "Lifecycle phase of a tool invocation within a stream.",
 "enum": [
  "start",
  "done"
 ],
 "type": "string"
}
```

### TurnStatusResponse

```json
{
 "description": "Response DTO for `GET /chats/{id}/turns/{request_id}`.",
 "properties": {
  "assistant_message_id": {
   "format": "uuid",
   "type": [
    "string",
    "null"
   ]
  },
  "error_code": {
   "type": [
    "string",
    "null"
   ]
  },
  "request_id": {
   "format": "uuid",
   "type": "string"
  },
  "state": {
   "$ref": "#/components/schemas/TurnStatusState"
  },
  "updated_at": {
   "format": "date-time",
   "type": "string"
  }
 },
 "required": [
  "request_id",
  "state",
  "updated_at"
 ],
 "type": "object"
}
```

### TurnStatusState

```json
{
 "description": "Turn state as reported by the turn status endpoint.",
 "enum": [
  "running",
  "done",
  "error",
  "cancelled"
 ],
 "type": "string"
}
```

### UpdateChatReq

```json
{
 "description": "Request DTO for updating a chat title.",
 "properties": {
  "title": {
   "type": "string"
  }
 },
 "required": [
  "title"
 ],
 "type": "object"
}
```

### Usage

```json
{
 "description": "Token usage counters.",
 "properties": {
  "input_tokens": {
   "format": "int64",
   "type": "integer"
  },
  "output_tokens": {
   "format": "int64",
   "type": "integer"
  }
 },
 "required": [
  "input_tokens",
  "output_tokens"
 ],
 "type": "object"
}
```

### WebSearchConfig

```json
{
 "description": "Web search toggle.",
 "properties": {
  "enabled": {
   "type": "boolean"
  }
 },
 "required": [
  "enabled"
 ],
 "type": "object"
}
```

### Problem

```json
{
 "description": "RFC 9457 problem+json. `context` varies by error category.",
 "properties": {
  "context": {
   "type": "object"
  },
  "detail": {
   "type": "string"
  },
  "instance": {
   "type": "string"
  },
  "status": {
   "format": "int32",
   "type": "integer"
  },
  "title": {
   "type": "string"
  },
  "trace_id": {
   "type": "string"
  },
  "type": {
   "type": "string"
  }
 },
 "required": [
  "type",
  "title",
  "status",
  "detail",
  "context"
 ],
 "type": "object"
}
```

