# NeuroPlasticity Architecture (v2)
## Self-Reinforced Testing & Prompt Optimization Framework

*Updated to incorporate findings from the ARC-7 Panel Architectural Review (2026-04-20).*

### 1. The Universal `plasticity.json` Specification
Every project within the Neuro ecosystem includes a `plasticity.json` file at its root. This JSON manifest configures the ephemeral sandbox, the task prompt, and the evaluation scripts.

**ARC-7 Security & Reliability Hardening:**
*   **Command Injection Prevented (M-001):** `agent_command` and `setup_script` must be strictly typed as arrays of strings (`argv`), completely bypassing shell parsing.
*   **Deterministic Environments (M-002):** `base_image` strictly requires pinned `@sha256` digests. Floating tags like `:latest` are rejected.
*   **Routable Schema (M-001):** `$schema` points to a public, routable URI for strict IDE/CLI validation.
*   **Explicit Scoring (C-003):** Defines a `pass_threshold` to govern the optimization loop.

```json
{
  "$schema": "https://raw.githubusercontent.com/neuro-org/neuroplasticity/main/schemas/v1/plasticity.schema.json",
  "name": "project-module-eval",
  "task_prompt": "Refactor routing and document the architecture. Follow memory constraints.",
  "agent_command": ["opencode", "run", "--dangerously-skip-permissions"],
  "sandbox": {
    "engine": "podman",
    "base_image": "neuro-rust-testbed@sha256:a1b2c3d4e5f6...",
    "setup_script": ["./tests/setup_env.sh"]
  },
  "optimization": {
    "target_rules_file": ".neuroplasticity/rules.json",
    "epochs": 3,
    "pass_threshold": 1.0,
    "meta_llm": "claude-3-5-sonnet"
  },
  "evaluators": [
    {
      "name": "Compilation & Linting",
      "script": ["./scripts/eval/01_compile.sh"],
      "weight": 1.0
    },
    {
      "name": "Side Effect Verification",
      "script": ["./scripts/eval/02_side_effects.sh"],
      "weight": 1.5
    }
  ]
}
```

### 2. Strict Ephemeral Sandbox Isolation (Podman)
To solve `EACCES` issues while guaranteeing **true ephemerality** and preventing host repository corruption (C-001):

1.  **Copy-on-Write Workspace:** Before each epoch, the orchestrator copies the host repository (excluding `.git` and `.neuroplasticity/runs`) to a temporary scratch directory (e.g., `/tmp/neuroplasticity-run-<uuid>`).
2.  **Podman Execution:** This isolated scratch directory is mounted read-write into the container. The host repository is never mounted.
3.  **Disposal:** If the epoch fails, the scratch directory is destroyed, leaving the host repository pristine.

**Execution Command Template:**
```bash
podman run --rm --userns=keep-id --security-opt no-new-privileges \
  -v /tmp/neuroplasticity-run-<uuid>:/workspace:Z \
  --workdir /workspace \
  <base_image@sha256:...> \
  <agent_command_argv>
```

### 3. The Execution, Evaluation, & Observability Loop
NeuroPlasticity orchestrates the following evaluation epochs:

1.  **Parse & Validate:** The Rust CLI reads `plasticity.json` and strictly enforces the JSON schema.
2.  **Isolate:** A new scratch workspace is created for the epoch.
3.  **Execute:** The agent executes headlessly inside the Podman sandbox.
4.  **Evaluate & Score (C-003, MIN-001):** Evaluator scripts run (in parallel if independent).
    *   **Scoring Math:** `Score = Sum(Passing Evaluator Weights) / Sum(All Evaluator Weights)`
    *   The loop terminates successfully if `Score >= pass_threshold`.
5.  **Observe (M-003):** An epoch artifact (`.neuroplasticity/runs/<run_id>/epoch-<n>.json`) is written to the host, containing the exact commands run, stdout/stderr, and the computed score.
6.  **Optimize & Mutate (C-002):** If `Score < pass_threshold`, the Meta-Optimizer LLM is invoked.
    *   **Safe Mutation:** Instead of blindly appending to `AGENTS.md`, the LLM generates structured rules. These are written to an overlay file (`.neuroplasticity/rules.json`) that the agent consumes at runtime.
7.  **Loop:** Repeat until success or `epochs` cap is reached.