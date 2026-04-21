# NeuroPlasticity Architecture (v3)
## Self-Reinforced Testing & Prompt Optimization Framework

*Updated to incorporate embedded LLM inference and provider-agnostic Meta-Optimization.*

### 1. The Universal `plasticity.json` Specification
Every project within the Neuro ecosystem includes a `plasticity.json` file at its root. This JSON manifest configures the ephemeral sandbox, the task prompt, and the evaluation scripts.

**Security & Reliability Hardening:**
*   **Command Injection Prevented:** `agent_command` and `setup_script` must be strictly typed as arrays of strings (`argv`), completely bypassing shell parsing.
*   **Deterministic Environments:** `base_image` securely isolates the agent using Podman.
*   **Routable Schema:** `$schema` points to a public, routable URI for strict IDE/CLI validation.
*   **Explicit Scoring:** Defines a `pass_threshold` to govern the optimization loop.

```json
{
  "$schema": "https://raw.githubusercontent.com/neuro-org/neuroplasticity/main/schemas/v1/plasticity.schema.json",
  "name": "project-module-eval",
  "task_prompt": "Refactor routing and document the architecture. Follow memory constraints.",
  "agent_command": ["opencode", "run", "--dangerously-skip-permissions"],
  "sandbox": {
    "engine": "podman",
    "base_image": "neuro-rust-testbed:latest"
  },
  "optimization": {
    "target_rules_file": ".neuroplasticity/rules.json",
    "epochs": 3,
    "pass_threshold": 1.0,
    "meta_llm": {
      "provider": "embedded",
      "model": "qwen-local"
    }
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
To solve `EACCES` issues while guaranteeing **true ephemerality** and preventing host repository corruption:

1.  **Copy-on-Write Workspace:** Before each epoch, the orchestrator copies the host repository (excluding `.git` and `.neuroplasticity/runs`) to a temporary scratch directory.
2.  **Podman Execution:** This isolated scratch directory is mounted read-write into the container. The host repository is never mounted.
3.  **Disposal:** If the epoch fails, the scratch directory is automatically destroyed, leaving the host repository pristine.

### 3. The Execution, Evaluation, & Observability Loop
NeuroPlasticity orchestrates the following evaluation epochs:

1.  **Parse & Validate:** The Rust CLI reads `plasticity.json` and strictly enforces the JSON schema.
2.  **Isolate:** A new scratch workspace is created for the epoch.
3.  **Execute:** The agent executes headlessly inside the Podman sandbox.
4.  **Evaluate & Score:** Evaluator scripts run (in parallel if independent).
    *   **Scoring Math:** `Score = Sum(Passing Evaluator Weights) / Sum(All Evaluator Weights)`
    *   The loop terminates successfully if `Score >= pass_threshold`.
5.  **Observe:** An epoch artifact (`.neuroplasticity/runs/<run_id>/epoch-<n>.json`) is written to the host, containing the exact commands run, stdout/stderr, and the computed score.
6.  **Optimize & Mutate:** If `Score < pass_threshold`, the Meta-Optimizer LLM is invoked.
    *   **Embedded Inference:** The orchestrator natively boots `llama.cpp` inside the Rust process to locally evaluate `stderr` and generate prompt corrections without network dependency.
    *   **Safe Mutation:** The LLM generates structured rules. These are written to an overlay file (`.neuroplasticity/rules.json`) that the agent consumes at runtime.
7.  **Loop:** Repeat until success or `epochs` cap is reached. When successful, outputs a `neuroplasticity_patch.md` for permanent agent integration.