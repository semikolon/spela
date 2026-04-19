# Architectural Decisions - spela

## 2026-04-18: Symptoms Are Signals — Nine Principles of System Intervention
**Decision ID**: d_2026_04_18_36D949B2
**Category**: governance
**Decision**: Symptoms Are Signals — Nine Principles of System Intervention
**Rationale**: Automated BUFFERING detection in spela cast_health_monitor killed a self-recovering HLS stream (Apr 18, 2026). Root cause investigation revealed the fix violated nine named SWE principles (dogfooding cost curve, symptoms≠problems, normalization of deviance, Chesterton's Fence, observability before remediation, auto-remediation anti-patterns, Five Whys, primum non nocere, Goodhart's Law). Framework now governs all future system intervention decisions.
**Context**: Triggered by recent significance: architecture: Latest actions are explicit cross-project architectural decisions (MCP Tool raw_cypher_query integration; Graphiti fork divergence) plus governance artifacts (Narration Deduplication; Episode-to-Decision Graduation; Self-Learning Skills). They have broad, cross-team impact and warrant ADR-style documentation and alignment.; integration: Cross-project integration decisions (MCP Tool raw_cypher_query integration; Graphiti fork divergence) plus governance/learning artifacts (Narration Deduplication; Episode-to-Decision Graduation; Self-Learning Skills) and an integration-related change (Path B SpeexDSP AEC) indicate multi-project impact requiring ADR-style documentation and cross-team alignment.; architecture: Recent interactions show cross-project architectural decisions (MCP Tool raw_cypher_query integration; Graphiti fork divergence) and governance/self-learning artifacts (Narration Deduplication; Episode-to-Decision Graduation; Self-Learning Skills), warranting ADR-style documentation and cross-team alignment.
**Status**: Implemented

---

