# UPSTREAM_REQS — {Target Gear  Name}

<!--
=============================================================================
UPSTREAM REQUIREMENTS DOCUMENT
=============================================================================
PURPOSE: Optional document to capture technical requirements IMPOSED
ON this gear BY upstream gears (dependencies, consumers, surrounding systems).

CONTEXT: When a gear is being designed or its PRD is still emerging,
upstream gears may need specific capabilities from it. This document
captures those requirements from the perspective of upstream gears.

SCOPE:
  ✓ Requirements FROM upstream gears TO this gear
  ✓ Public interfaces this gear must expose
  ✓ Functional requirements imposed by upstream consumers
  ✓ Non-functional requirements from upstream dependencies
  ✓ Integration contracts this gear must fulfill

STRUCTURE:
  - Each upstream gear has its own section
  - Requirements are grouped by upstream gear source
  - Clear ownership: each requirement states which gear needs it

RELATIONSHIP TO OTHER DOCS:
  - PRD.md: Defines what THIS gear does (internal perspective)
  - UPSTREAM_REQS.md: Defines what OTHERS need from THIS gear (external perspective)
  - DESIGN.md: Technical implementation of both sets of requirements

USE CASES:
  - Early-stage development: upstream needs known before full PRD exists
  - API-first design: consumers define interface requirements
  - Integration planning: capture cross-gear dependencies
  - Incremental development: build features driven by actual consumer needs

STANDARDS ALIGNMENT:
  - IEEE 830 / ISO/IEC/IEEE 29148:2018 (requirements specification)
  - ISO/IEC 15288 / 12207 (interface requirements)

REQUIREMENT LANGUAGE:
  - Use "MUST" or "SHALL" for mandatory requirements (implicit default)
  - Do not use "SHOULD" or "MAY" — use priority p2/p3 instead
  - Be specific and clear; no fluff, bloat, duplication, or emoji
=============================================================================
-->

# UPSTREAM: {name of the upstream gear from which requirements are imposed}

## System Actor Definition

**ID**: `cpt-{target-gear}-upstream-actor-{slug}`

**Role**: {Description of how the upstream gear interacts with target gear}
**Integration Pattern**: {e.g., REST API client, SDK consumer, Event subscriber, Direct library import}

## Functional Requirements

Requirements that the upstream gear needs the target gear to fulfill.

### {Requirement Name}

- [ ] `p1` - **ID**: `cpt-{target-gear}-upstream-fr-{slug}`

The target gear **MUST** {specific capability or behavior needed by upstream}.

**Rationale**: {Why the upstream gear needs this capability}
**Use Case**: {How the upstream gear will use this capability}
**Integration Point**: {e.g., REST endpoint, SDK method, Event topic}

### {Another Requirement}

- [ ] `p2` - **ID**: `cpt-{target-gear}-upstream-fr-{slug}`

The target gear **MUST** {another specific requirement}.

**Rationale**: {Why this is needed}
**Use Case**: {Usage scenario}

## Non-Functional Requirements

NFRs that the upstream gear requires from the target gear.

### {NFR Name}

- [ ] `p1` - **ID**: `cpt-{target-gear}-upstream-nfr-{slug}`

The target gear **MUST** {measurable NFR with specific thresholds}.

**Threshold**: {Quantitative target with units, e.g., "respond within 100ms at p95"}
**Rationale**: {Why this NFR is required by upstream}
**Impact on Upstream**: {What happens if this NFR is not met}

## Public Interface Requirements

Interfaces that must be exposed to the upstream gear.

### {Interface Name}

- [ ] `p1` - **ID**: `cpt-{target-gear}-upstream-interface-{slug}`

**Type**: {REST API | gRPC | SDK method | Event | Data format}
**Stability Required**: {stable | unstable acceptable}
**Contract**: {Detailed interface contract or link to OpenAPI spec}

**Description**: {What this interface must provide}
**Input**: {Expected input parameters/format}
**Output**: {Expected output/response}
**Error Handling**: {Required error scenarios and responses}

---

# UPSTREAM: {name of the upstream gear from which requirements are imposed}

## System Actor Definition
...

## Functional Requirements
...

### {Requirement Name}
...

### {Another Requirement}
...

## Non-Functional Requirements
...

### {NFR Name}
...

## Public Interface Requirements
...

### {Interface Name}
...
