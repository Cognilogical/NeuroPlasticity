# Architectural Review Report — NeuroPlasticity Architecture

**Subject:** NeuroPlasticity Architecture (JSON/Podman)
**Date:** 2026-04-20
**Mode:** Document Review
**Panel:** Context Master (Gemini 3 Pro) · The Architect (Claude Sonnet 4.6) · Security Sentinel (OpenAI o4) · Product Visionary (GPT-5.2) · Creative Strategist (GPT-5.3-Codex) · The Optimizer (GPT-5.3-Codex) · The Naysayer (Claude Sonnet 4.6)

---

## Final Recommendation: Request Changes

The panel universally praises the foundational choices of this architecture: using a declarative JSON manifest (`plasticity.json`) and standardizing on rootless Podman to solve `EACCES` issues provides a clean, language-agnostic contract. However, the architecture contains Critical flaws regarding sandbox isolation, prompt mutation safety, and scoring math. Because the sandbox mounts the host directory read-write, an agent can permanently corrupt the repository. Furthermore, appending LLM output directly to `AGENTS.md` without a rollback mechanism creates an unbounded, dangerous feedback loop. These structural integrity and security issues must be addressed before implementation.

---

## Findings Summary

| Severity | Count |
|----------|-------|
| Critical | 3     |
| Major    | 3     |
| Minor    | 1     |
| Info     | 0     |

---

## Critical Issues (Must Address)

### C-001: Live Workspace Mounted Read-Write defeats Ephemerality
- **Severity:** Critical
- **Source:** The Architect, Security Sentinel, The Naysayer, Creative Strategist
- **Description:** The proposed execution command (`podman run -v .:/workspace`) mounts the host's current working directory directly into the container. Combined with an agent running autonomously, any file deletions, git operations, or mutations escape the container and permanently alter the developer's checkout. This breaks the "ephemeral sandbox" guarantee and allows a failed epoch to corrupt the host repository (A01 access-control gap).
- **Recommendation:** Implement a copy-on-write workspace. The orchestrator must create an ephemeral working copy (e.g., via `git worktree add` or copying to a tmpfs), mount THAT into the container, and discard it after the epoch. The host repository must remain untouched by the sandbox.

### C-002: Direct Marker-Based Mutation is Fragile and Dangerous
- **Severity:** Critical
- **Source:** The Architect, Security Sentinel, Product Visionary, The Naysayer, Creative Strategist
- **Description:** Appending LLM-generated rules beneath an `injection_marker` in a tracked file (`AGENTS.md`) is highly susceptible to hallucinated formatting, unbounded file growth, and silent corruption of agent instructions. There is no human-in-the-loop gate, no validation of the LLM's output, and no rollback mechanism.
- **Recommendation:** Stop writing directly to `AGENTS.md`. Instead, emit generated rules to a structured JSON overlay file (e.g., `.neuroplasticity/rules.json`) that the agent reads at runtime, OR write mutations to a candidate file (`AGENTS.md.candidate`) requiring an explicit human promotion step.

### C-003: Evaluator Weight Normalization is Undefined
- **Severity:** Critical
- **Source:** The Architect, Product Visionary, Creative Strategist, The Optimizer, The Naysayer
- **Description:** The spec states that an exit code != 0 "strips the test of its assigned weight," but does not define how weights aggregate or what constitutes a "perfect score." Without a normalized scoring function, the optimization loop cannot determine convergence reliably, risking false positives where the loop exits despite failing critical evaluators.
- **Recommendation:** Explicitly define the scoring formula (e.g., `Score = Sum(Passing Weights) / Sum(All Weights)`). Define a `pass_threshold` (e.g., 1.0) in the schema to dictate when the optimization loop successfully terminates.

---

## Major Issues (Should Address)

### M-001: Command Injection and Weak Schema Validation
- **Severity:** Major
- **Source:** The Architect, Security Sentinel, The Naysayer
- **Description:** `agent_command` is defined as a raw shell string, creating an A03 injection surface if `plasticity.json` is modified maliciously. Furthermore, the `$schema` URL (`neuroplasticity.local`) is non-routable, preventing strict IDE and CLI validation.
- **Recommendation:** Define `agent_command` as an `argv` array of strings in the JSON schema. Publish the schema to a routable URL (e.g., GitHub raw content) and enforce strict schema validation in the Rust CLI before executing any container.

### M-002: Non-Deterministic Environments via `:latest` Tags
- **Severity:** Major
- **Source:** The Architect, The Naysayer, The Optimizer
- **Description:** Relying on `neuro-rust-testbed:latest` ensures that execution environments will drift over time. An evaluation failure could be caused by an upstream image update rather than the agent's actions, breaking the integrity of the SRT loop.
- **Recommendation:** Mandate pinned image digests (e.g., `@sha256:...`) in the schema. Reject floating tags like `:latest` to ensure reproducible epochs.

### M-003: Lack of Observability and Audit Artifacts
- **Severity:** Major
- **Source:** Product Visionary, Creative Strategist, The Optimizer, The Naysayer
- **Description:** The pipeline executes headlessly and mutates rules but produces no structured output artifacts (no `epoch_report.json` or NDJSON event stream). Without this, debugging a failed optimization loop or gaining developer trust is impossible.
- **Recommendation:** Emit a structured JSON report per epoch containing the score, the evaluator outputs, the exact sandbox command used, and the rule mutation applied. 

---

## Minor Suggestions (Nice to Have)

### MIN-001: Sequential Evaluators Create Latency Bottlenecks
- **Severity:** Minor
- **Source:** The Optimizer
- **Description:** Running evaluators sequentially extends the critical path of each epoch, creating slow feedback loops.
- **Recommendation:** Execute independent evaluator scripts in parallel where possible, aggregating their exit codes at the end.

---

## What Was Done Well

- **Podman Rootless Choice:** Excellent, pragmatic selection that directly addresses known `EACCES` friction points in CI and local dev.
- **JSON Manifest:** A single, standardized `plasticity.json` contract improves machine validation and unifies cross-language tooling.
- **Evaluator Abstraction:** Decoupling the evaluation logic into pluggable scripts cleanly supports polyglot repositories without forcing a single test runner.
- **Humility in Design:** Explicitly listing Open Questions in the architectural proposal demonstrated strong engineering maturity and perfectly framed the review.

---

## Blind Voting Results (If Applicable)

None needed. The panel was unanimous in identifying the critical isolation and mutation flaws, as well as praising the core containerization strategy.

---

## Panel Breakdown

### The Architect (Claude Sonnet 4.6)
- **Recommendation:** Approve with Conditions
- **Summary:** The design separates concerns well but is critically under-specified regarding sandbox isolation and prompt mutation. Bind-mounting `.` defeats ephemerality.
- **Findings:** 2 Critical, 2 Major, 0 Minor

### Security Sentinel (OpenAI o4)
- **Recommendation:** Request Changes
- **Summary:** Leaves major security gaps in Podman sandbox execution and injection logic that expose the host to malicious inputs.
- **Findings:** 1 Critical, 2 Major, 1 Minor

### Product Visionary (GPT-5.2)
- **Recommendation:** Approve with Conditions
- **Summary:** Pragmatic execution choice, but the self-modifying "black box" risks losing developer trust without governance, replayability, and safe metrics.
- **Findings:** 0 Critical, 3 Major, 2 Minor

### Creative Strategist (GPT-5.3-Codex)
- **Recommendation:** Request Changes
- **Summary:** Directionally strong but couples execution and mutation too tightly. Inverting the mutation target to an immutable overlay makes the system trustworthy.
- **Findings:** 1 Critical, 2 Major, 2 Minor

### The Optimizer (GPT-5.3-Codex)
- **Recommendation:** Approve with Conditions
- **Summary:** Good standardization, but performance risks include per-epoch container latency, sequential evaluators, and unbounded prompt context growth.
- **Findings:** 1 Critical, 3 Major, 2 Minor

### The Naysayer (Claude Sonnet 4.6)
- **Recommendation:** Request Changes
- **Summary:** Highlights three dangerous risks: mutating tracked source files without a gate, mounting the live workspace read-write, and undefined weight normalization that will cause false convergence.
- **Findings:** 3 Critical, 4 Major, 2 Minor

---

## Dissenting Opinions

Panel was unanimous.

---

## Action Items

- [ ] Refactor the execution loop to copy the workspace to an ephemeral `tmpfs` or `git worktree` before mounting it into the Podman container.
- [ ] Replace `injection_marker` string appending with a structured rules file (`rules.json`) or a strict human-review candidate generation.
- [ ] Define the exact math for evaluator score aggregation in the schema documentation.
- [ ] Update `plasticity.json` schema to require `argv` arrays for commands and pinned `@sha256` digests for images.

---

*Generated by ARC-7 Panel · 2026-04-20*