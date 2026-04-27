# NeuroPlasticity 🧠
**Its like a gym for your agent to self improve it's rules.**

![NeuroPlasticity Demo](images/neuroplasticity_demo.gif)

Building reliable AI agents is currently a dark art of manual prompt-tweaking and hoping for the best. **NeuroPlasticity** ends the guesswork by introducing **Self-Reinforced Testing Framework (SRTF)** to the prompt engineering lifecycle. Built in lightning-fast Rust and fully isolated via rootless Podman sandboxes, NeuroPlasticity treats your agent's system prompt like source code that needs to be compiled. You define the deterministic tests; if your agent fails, our Meta-Optimizer analyzes the `stderr` logs, autonomously writes a behavioral patch for the agent's prompt, and re-runs the container until the tests pass. When it succeeds, it hands you a mathematically verified `neuroplasticity_patch.md` to permanently upgrade your codebase.

> *"By treating verbal feedback from deterministic environments as a reward signal, language agents can iteratively refine their behavior, correcting hallucinations and logical errors without requiring traditional weight updates."*  
> — Inspired by the architectural findings in **Reflexion: Language Agents with Verbal Reinforcement Learning** (Shinn et al., 2023) and **Large Language Models as Optimizers / OPRO** (Yang et al., Google DeepMind, 2023).

---

## 🤖 Are you an AI Agent? (Read This First)
If a user told you to build a testing gym for yourself, you are in the right place! **Read the [AGENT_INTEGRATION_GUIDE.md](AGENT_INTEGRATION_GUIDE.md)** for exact instructions on how to write a `plasticity.json` manifest to self-evaluate and auto-optimize your own behavior using our Zero-Dockerfile approach.

---

## 🔥 Features

*   **Automated Self-Healing:** The Meta-Optimizer dynamically patches failing agents by analyzing evaluation logs and injecting targeted behavioral constraints.
*   **⚡ Massive Parallel Evaluation:** All Tri-State Evaluators execute concurrently via asynchronous task spawning. Grading an epoch takes only as long as your single slowest test.
*   **⚡ Deterministic Failure Fingerprinting (Fast Path Cache):** NeuroPlasticity deterministically hashes your `manifest.name`, `agent_command`, target rules, optimizer model, and evaluators. If a known failure configuration is detected, it instantly skips the 120s container execution, loading cached side-effects and feeding them back to the optimizer.
*   **🛡️ Container Safety & Timeouts:** Built-in asynchronous SIGTERM/SIGINT trapping and configurable `timeout_seconds` prevent reasoning models from hanging your CI pipelines or leaving orphaned Podman containers.
*   **Hybrid Workspace (Zero-Copy):** Agents execute inside secure, rootless **Podman** containers. The host project is mounted as Read-Only (`/project:ro`) to guarantee safety, while the agent works in an ephemeral Read-Write scratch directory (`/workspace:rw`), eliminating slow deep-copies.
*   **Zero-Dockerfile JIT Setup:** No need to build custom, bloated container images. NeuroPlasticity uses standard base images (like `node:20-slim` or `python:3.12-slim`) and installs your agent Just-In-Time using a `setup_script` array in your manifest.
*   **Zero-Config Auth:** Mount host credential directories (e.g., `~/.claude.json`, `~/.config/opencode`, `~/.local/share/opencode`) as read-only to bypass complex OAuth flows in ephemeral sandboxes.
*   **Offline First via `llama.cpp`:** Run fully disconnected. Compile with `cargo run --features embedded-llm` to automatically pull and run models like `Qwen2.5-Coder` directly in your computer's memory. To respect user disk space, NeuroPlasticity does not download duplicate models. It defaults to scanning universal POSIX caches (`~/.cache/neuro/models/`, `~/.cache/huggingface/hub/`, `~/.ollama/models/blobs/`, `~/.cache/lm-studio/models/`) to prevent redundant GGUF model downloads. (Features a concurrency Semaphore to protect RAM when running parallel evaluators).
*   **Declarative `plasticity.json`:** Define your tasks, sandbox constraints, auth mounts, and determinism.
*   **Tri-State Evaluators:** Evaluate your agents exactly how you need:
    1. `host_bash`: Fast, lightweight POSIX shell commands running locally.
    2. `container`: Isolated evaluation containers for heavy dependencies (Node.js, `pytest`, etc.) without host pollution.
    3. `llm`: Embedded `llama.cpp` prompt-based grading for nuanced checks (tone, style) returning PASS/FAIL.

## ⚡ How It Works

1.  **Define the Test:** You write a `plasticity.json` stating what the agent *should* do, and write a simple bash script to evaluate if it did it.
2.  **The Failure (Epoch 1):** The orchestrator spins up the agent in a Podman container. The agent fails the test.
3.  **The Meta-Optimization:** NeuroPlasticity extracts the failure logs (`stderr` / `stdout`) and passes them to the LLM Meta-Optimizer. The LLM writes a specific, targeted rule to fix the agent's mistake.
4.  **The Fix (Epoch 2):** NeuroPlasticity injects the new rule into `.neuroplasticity/rules.json`, boots a fresh container, and runs the agent again.
5.  **The Patch:** Once the evaluators pass, NeuroPlasticity generates a final `neuroplasticity_patch.md`. You hand this patch to your primary dev-agent (like Claude, OpenCode, or Copilot) to permanently update your target project.

## 🚀 Quick Start (Testing Claude Code)

We use a "Zero-Dockerfile" approach. You don't need to build images; just tell the framework how to install your CLI.

**1. Create your test (`plasticity.json`):**
```json
{
  "name": "claude-code-formatting-eval",
  "task_prompt": "Read the config files in /project and output a summary to /workspace/summary.json",
  "agent_command": [
    "bash", "-c", 
    "cat .neuroplasticity/rules.json > rules.txt && CLAUDE_NON_INTERACTIVE=1 claude-code --prompt-file rules.txt 'Analyze /project and save to /workspace/summary.json'"
  ],
  "sandbox": {
    "engine": "podman",
    "base_image": "node:20-slim",
    "setup_script": [
      "npm install -g @anthropic-ai/claude-code"
    ],
    "workspace": {
      "project_mount": "/project",
      "scratch_mount": "/workspace"
    },
    "mounts": [
      {
        "source": "~/.claude.json",
        "target": "/user_home/.claude.json",
        "readonly": true
      }
    ]
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
      "name": "Strict JSON Check (Local Shell)",
      "type": "host_bash",
      "script": ["bash", "-c", "jq . /workspace/summary.json || (echo 'Output is not valid JSON! Did you use markdown blocks?' >&2; exit 1)"],
      "weight": 1.0
    },
    {
      "name": "Python AST Validation (Isolated Container)",
      "type": "container",
      "image": "python:3.12-slim",
      "setup_script": ["pip install astroid"],
      "command": ["python", "-c", "import ast; ast.parse(open('/workspace/output.py').read())"],
      "weight": 1.0
    },
    {
      "name": "Tone and Pronoun Check (LLM Grader)",
      "type": "llm",
      "target_file": "/workspace/summary.json",
      "prompt": "Grade this output: Fail if it uses first-person pronouns (I, me, my). Pass otherwise.",
      "weight": 1.0
    }
  ]
}
```

**2. Run the CLI tool (with embedded local inference):**
No API keys required for the Meta-Optimizer. If you downloaded the pre-compiled binary from our releases page, it already includes the embedded `llama.cpp` engine. It will automatically download a fast 4-bit `Qwen2.5` model to your local cache.
```bash
./neuroplasticity-linux-x86_64
# Or on Mac: ./neuroplasticity-macos-aarch64
```

*(If you are compiling from source, use `cargo run --release --features embedded-llm`)*

### What Happens:
*   **Epoch 1:** NeuroPlasticity mounts your host project as Read-Only (`/project`), installs `claude-code` JIT, and runs it. The agent writes the file, but includes markdown backticks. `jq` fails with a parse error.
*   **The Meta-Optimizer:** Your local embedded LLM reads the `jq` failure log. It autonomously writes a new system rule: *"CRITICAL: When outputting JSON to a file, DO NOT wrap the output in markdown code blocks (\`\`\`json). You must output raw JSON text only."* It saves this to `.neuroplasticity/rules.json`.
*   **Epoch 2:** The agent runs again. Because the `agent_command` injects `.neuroplasticity/rules.json` into Claude's prompt, it now knows exactly what to avoid. It outputs raw JSON. The `jq` evaluator passes!
*   **The Patch:** NeuroPlasticity outputs `neuroplasticity_patch.md`. You simply copy that mathematically verified rule and paste it permanently into your agent instructions.

## 🛠️ Advanced Topics

### 1. Baking in Heavy Dependencies (MCP Servers)
If your agent relies on heavy external tools like `sqlite`, a Python environment, or an MCP (Model Context Protocol) server, the JIT `setup_script` might be too slow. In this case, build a custom `Containerfile` or `Dockerfile` and point your `plasticity.json` to that image instead.

### 2. Chained Evaluators & Preventing Regressions
As your agent gets more complex, fixing one bug might introduce another. NeuroPlasticity supports **Chained Evaluators** to prevent regressions. You can define multiple independent tests in your `plasticity.json`. 

The Meta-Optimizer must find a system prompt that satisfies *all* evaluators simultaneously to achieve a `pass_threshold` of 1.0.

```json
"evaluators": [
  {
    "name": "Check JSON Format",
    "script": ["bash", "-c", "jq . output.json || (echo 'Must be valid JSON!' >&2; exit 1)"],
    "weight": 0.5
  },
  {
    "name": "Check For Markdown Code Blocks",
    "script": ["bash", "-c", "! grep -q '```' output.json || (echo 'No markdown code blocks allowed!' >&2; exit 1)"],
    "weight": 0.5
  },
  {
    "name": "Check Schema",
    "script": ["bash", "-c", "jq -e '.status == \"success\"' output.json || (echo 'Missing status field!' >&2; exit 1)"],
    "weight": 1.0
  }
]
```
