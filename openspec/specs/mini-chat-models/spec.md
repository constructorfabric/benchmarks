# mini-chat-models Specification

## Purpose
Model catalog exposure and model selection of the mini-chat gear.

## Requirements

### Requirement: Read-only models API
`GET /v1/models` SHALL return `{items:[{model_id, display_name, tier (standard|premium), multiplier_display, description?, multimodal_capabilities, context_window}]}` for enabled catalog entries only, read from the policy plugin on each request. `GET /v1/models/{id}` SHALL return the same projection or 404 `not_found` with `resource_type = gts.cf.core.mini_chat.model.v1~` for disabled or unknown ids. Provider ids, provider model names, multipliers, policy version, max output and `is_default` SHALL NOT be exposed.

#### Scenario: Disabled model hidden
- **WHEN** the catalog contains a disabled model `m-off`
- **THEN** it is absent from `GET /v1/models` and `GET /v1/models/m-off` returns 404

#### Scenario: No internal fields
- **WHEN** a model is fetched
- **THEN** the body has no `provider_id`, `provider_model_id` or credit multiplier fields
