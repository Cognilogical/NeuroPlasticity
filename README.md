# NeuroPlasticity 🧠
**Its like a gym for you Agent to improve it's rules.**

Building reliable AI agents is currently a dark art of manual prompt-tweaking and hoping for the best. **NeuroPlasticity** ends the guesswork by introducing **Self-Reinforced Testing (SRT)** to the prompt engineering lifecycle. Built in lightning-fast Rust and fully isolated via rootless Podman sandboxes, NeuroPlasticity treats your agent's system prompt like source code that needs to be compiled. You define the deterministic tests; if your agent fails, our Meta-Optimizer analyzes the `stderr` logs, autonomously writes a behavioral patch for the agent's prompt, and re-runs the container until the tests pass. When it succeeds, it hands you a mathematically verified `neuroplasticity_patch.md` to permanently upgrade your codebase.

> *"By treating verbal feedback from deterministic environments as a reward signal, language agents can iteratively refine their behavior, correcting hallucinations and logical errors without requiring traditional weight updates."*  
> — Inspired by the architectural findings in **Reflexion: Language Agents with Verbal Reinforcement Learning** (Shinn et al., 2023) and **Large Language Models as Optimizers / OPRO** (Yang et al., Google DeepMind, 2023).

---

## 🔥 Features

*   **Automated Self-Healing:** The Meta-Optimizer dynamically patches failing agents by analyzing evaluation logs and injecting targeted behavioral constraints.
*   **100% Ephemeral Sandboxing:** Agents execute inside secure, rootless **Podman** containers with copy-on-write scratch directories. No host bleed. No broken state.
*   **Offline First via `llama.cpp`:** Run fully disconnected. Compile with `cargo run --features embedded-llm` to automatically pull and run models like `Qwen2.5-Coder` directly in your laptop's memory.
*   **Declarative `plasticity.json`:** Define your tasks, sandbox constraints, and deterministic `bash` evaluators in a strict, schema-backed JSON manifest.
*   **Verified Improvement Patches:** If the framework successfully optimizes an agent, it generates a `neuroplasticity_patch.md` detailing the exact prompt overrides needed to fix the agent permanently.

## ⚡ How It Works

1.  **Define the Test:** You write a `plasticity.json` stating what the agent *should* do, and write a simple bash script to evaluate if it did it.
2.  **The Failure (Epoch 1):** The orchestrator spins up the agent in a Podman container. The agent fails the test.
3.  **The Meta-Optimization:** NeuroPlasticity extracts the failure logs (`stderr` / `stdout`) and passes them to the LLM Meta-Optimizer. The LLM writes a specific, targeted rule to fix the agent's mistake.
4.  **The Fix (Epoch 2):** NeuroPlasticity injects the new rule into `.neuroplasticity/rules.json`, boots a fresh container, and runs the agent again.
5.  **The Patch:** Once the evaluators pass, NeuroPlasticity generates a final `neuroplasticity_patch.md`. You hand this patch to your primary dev-agent (like Claude or Aider) to permanently update your target project.

## 🚀 Quick Start

Ensure you have [Podman](https://podman.io/) installed.

```bash
# 1. Clone the repository
git clone https://github.com/neuro-org/neuroplasticity.git
cd neuroplasticity

# 2. Compile the CLI tool (with embedded local inference)
cargo build --release --features embedded-llm

# 3. Add to your PATH
export PATH="$PWD/target/release:$PATH"
```

## 🎯 Example: Optimizing the ARC-7 Architectural Review Skill

Let's say you have an elite architectural review agent called **ARC-7** (located at `../ARC-7`). You noticed that ARC-7 sometimes fails to flag when developers use YAML instead of JSON for configs. 

We can use NeuroPlasticity to run a deterministic test and force ARC-7 to improve.

**1. Create a dummy test file in your project:**
```bash
cd ../ARC-7
echo "database: postgres" > dummy_config.yaml
```

**2. Write your `plasticity.json` in the `../ARC-7` directory:**
```json
{
  "$schema": "https://raw.githubusercontent.com/neuro-org/neuroplasticity/main/schemas/v1/plasticity.schema.json",
  "name": "arc7-yaml-eval",
  "task_prompt": "Run an architectural review on dummy_config.yaml",
  "agent_command": ["./bin/arc7", "review", "dummy_config.yaml"],
  "sandbox": {
    "engine": "podman",
    "base_image": "localhost/arc7-testbed:latest"
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
      "name": "Flag YAML usage",
      "script": ["bash", "-c", "grep -qi 'JSON' .arc7_report.md || (echo 'ARC-7 failed to recommend JSON instead of YAML' >&2; exit 1)"],
      "weight": 1.0
    }
  ]
}
```

**3. Run the Test!**
```bash
neuroplasticity
```

### What Happens:
*   **Epoch 1:** NeuroPlasticity copies `ARC-7` into a sandbox. ARC-7 reviews `dummy_config.yaml`, but says nothing about JSON. The `grep` evaluator fails.
*   **The Meta-Optimizer:** Your local embedded LLM sees `ARC-7 failed to recommend JSON instead of YAML`. It autonomously writes a new system rule: *"When reviewing configuration files, if you see YAML, you MUST explicitly flag it and mandate strict JSON usage instead."* and saves it to `.neuroplasticity/rules.json`.
*   **Epoch 2:** ARC-7 boots up again, reading the newly generated rules file. This time, it forcefully flags the YAML file. The `grep` evaluator passes!
*   **The Fix:** NeuroPlasticity outputs `neuroplasticity_patch.md`. You take that mathematically verified rule and permanently paste it into ARC-7's actual prompt/codebase.

## 🛠️ Configuration (`plasticity.json`)

```json
{
  "$schema": "https://raw.githubusercontent.com/neuro-org/neuroplasticity/main/schemas/v1/plasticity.schema.json",
  "name": "my-agent-eval",
  "task_prompt": "Update the database schema.",
  "agent_command": ["./run_my_agent.sh"],
  "sandbox": {
    "engine": "podman",
    "base_image": "localhost/my-agent-testbed:latest"
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
      "name": "Check Schema Creation",
      "script": ["bash", "-c", "test -f schema.sql"],
      "weight": 1.0
    }
  ]
}
```
