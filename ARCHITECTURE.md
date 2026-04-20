# NeuroPlasticity Architecture
## Self-Reinforced Testing & Prompt Optimization Framework

### 1. The Universal `plasticity.json` Specification

Every project within the Neuro ecosystem will include a `plasticity.json` file at its root. This JSON manifest configures the ephemeral sandbox, the task prompt, and the evaluation scripts required to assess the agent's side effects.

```json
{
  "$schema": "https://neuroplasticity.local/schemas/v1/plasticity.schema.json",
  "name": "project-module-eval",
  "agent_command": "opencode run --dangerously-skip-permissions",
  "task_prompt": "Refactor routing and document the architecture. Follow memory constraints.",
  "sandbox": {
    "engine": "podman",
    "base_image": "neuro-rust-testbed:latest",
    "setup_script": "./tests/setup_env.sh",
    "mounts": [
      {
        "source": ".",
        "target": "/workspace"
      }
    ]
  },
  "optimization": {
    "target_file": "AGENTS.md",
    "injection_marker": "## 🚨 DYNAMIC SRT RULES",
    "epochs": 3,
    "meta_llm": "claude-3-5-sonnet"
  },
  "evaluators": [
    {
      "name": "Compilation & Linting",
      "script": "./scripts/eval/01_compile.sh",
      "weight": 1.0
    },
    {
      "name": "Side Effect Verification",
      "script": "./scripts/eval/02_side_effects.sh",
      "weight": 1.5
    }
  ]
}
```

### 2. Standardized Podman Sandbox Archetypes

To enforce strict, ephemeral isolation and solve `EACCES` issues common in container-host interactions, NeuroPlasticity exclusively uses Podman archetypes. These base images are optimized for rootless execution.

*   **`neuro-rust-testbed` (For NeuroStrata, NeuroFabric):** Rust slim image containing essential C libraries like `pkg-config` and `libssl-dev` to support ONNX/FastEmbed models often used by embedded MCPs.
*   **`neuro-node-testbed` (For NeuroCortex, neuro-ui):** Node Alpine image for fast frontend testing.
*   **`neuro-agent-testbed` (For NeuroGenesis, NeuroPhonetic):** Ubuntu image with `git`, `curl`, and `jq` for general-purpose filesystem and CLI manipulations.

**Execution Command Template:**
```bash
podman run --userns=keep-id -v .:/workspace --workdir /workspace <base_image> <agent_command>
```

### 3. The Execution & Evaluation Loop

NeuroPlasticity orchestrates the following evaluation epochs:
1.  **Parse:** The Rust CLI reads `plasticity.json` and validates it against its JSON Schema.
2.  **Execute:** The agent is executed completely headlessly inside the Podman sandbox via the `agent_command`.
3.  **Evaluate:** The scripts listed under `evaluators` run sequentially. Any exit code != 0 signifies a failure and strips the test of its assigned weight.
4.  **Optimize & Mutate:** If the test fails, NeuroPlasticity feeds the `task_prompt`, previous rules, and failing `stderr` output to the Meta-Optimizer LLM. The LLM generates a targeted fix which is appended directly beneath the `injection_marker` in the `target_file`.
5.  **Loop:** The system loops until a perfect evaluator score is achieved or the max `epochs` limit is hit.