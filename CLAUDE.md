# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->


## Build & Test

```bash
# Build with the embedded offline LLM engine (required for provider "embedded")
cargo build --release --features embedded-llm

# Unit tests — no network, no model download
cargo test --features embedded-llm

# Ignored integration tests: real GGUF load + live endpoint round-trips
cargo test --release --features embedded-llm -- --ignored
#   NP_TEST_BASE_URL=<openai-compatible endpoint> NP_TEST_API_KEY_ENV=<KEY_ENV_VAR> \
#     cargo test --release --features embedded-llm custom_endpoint_round_trip -- --ignored
```

Without `--features embedded-llm`, builds are faster but a manifest using `provider: "embedded"` fails at runtime with a clear message.

## Architecture Overview

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full walkthrough. Key points:

- `src/main.rs` — CLI entry, the epoch/waterfall loop, and the final patch writer.
- `src/runner.rs` — Podman/Docker sandbox execution, the hybrid `/project` (ro) + `/workspace` (rw) workspace, and the ephemeral `/user_home`.
- `src/evaluator.rs` — Tri-State Evaluators, run concurrently under a semaphore. LLM verdicts are JSON Schema output; infrastructure errors abort the run.
- `src/optimizer.rs` — Meta-Optimizer. Returns a structured `{"rule": ...}` that is sanitized, length-capped, and de-duplicated before being persisted.
- `src/llm_client.rs` — the single request path for `custom` providers: deadline, retries, provider-error surfacing, and both `chat/completions` and `responses` wire formats.
- `src/rules.rs` — the rules-file model (`behavioral` / `constraint`) and the protected-rule policy. A protected rule is hidden from the optimizer, and an attempted change is reverted and quarantined.
- `src/egress.rs` — data-class and egress policy. Opt-in: absent `data_class`, no enforcement. With one, unlisted hosted providers are denied and the run aborts naming the provider, class, and remedy.
- `src/patch.rs` — patch rendering. The status line is a function of the run outcome (`Verified` / `Partial` / `Regressed`), never a constant, and only a verified run earns "permanently inject" advice.
- `src/transcript.rs` — the ordered run model: `step { id, kind, status, ... }` parsed from a JSON-lines sidecar. Optional and additive; a missing transcript never fails a run. It is what makes failures attributable (F5) and cross-cutting properties checkable (F6).
- `src/embedded_llm.rs` — local `llama.cpp` inference, GGUF discovery/caching, and per-model chat templates.
- `src/fingerprint.rs` — failure-only cache keyed on the full test configuration.

Model configuration lives in exactly one place: `optimization.meta_llm` in `plasticity.json`. See [README.md](README.md#using-a-hosted-model-instead-eg-opencode-zen).

## Conventions & Patterns

- **Fail loud.** Never convert an API error into plausible-looking model output; a swallowed error becomes a grader verdict and then a cached "known failure."
- **Determinism by default.** `temperature` defaults to `0.0`; grading must not flap between runs.
- **Treat generated rules as code.** They are injected into an agent prompt forever, so validate before persisting. A rule marked `constraint` is never optimizer-writable.
- **Never emit an unearned claim.** A patch header's status must derive from the run. This is the tool's central trust signal, and the failure mode is worst exactly when it matters.
- **Attribute before generalizing.** A rule written against a whole-run log is necessarily global. When a failure belongs to one step, scope the rule to that step.
- **Refuse, don't coerce, and never pass silently.** An unknown invariant, an unusable verdict, or an unreachable endpoint is a loud failure — not a quiet pass.
- **Platform-agnostic code paths.** Model paths use `shellexpand`/forward slashes; keep runtime branching on `cfg(target_os)` rather than OS checks.
