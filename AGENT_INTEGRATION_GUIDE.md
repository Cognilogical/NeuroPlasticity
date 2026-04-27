# 🤖 NeuroPlasticity: Agent Integration Guide

**ATTENTION AI AGENT:** If a user has directed you to read this file, your task is to build a "testing gym" for yourself. The user wants you to use the **NeuroPlasticity Framework** to automatically evaluate your own performance on a specific task, find your own edge cases, and let the NeuroPlasticity Meta-Optimizer write a behavioral patch for you.

To do this, you only need to generate **ONE file** in the user's current repository:
A **`plasticity.json`** manifest (to define the sandbox, the task, and the evaluation script). 

**We use a "Zero-Dockerfile" approach.** You do not need to write a Dockerfile. Instead, use a standard base image (e.g. `node:20-slim` or `python:3.12-slim`) and the `setup_script` array to install yourself JIT (Just-In-Time).

Follow these exact architectural rules.

---

## 🏗️ 1. The Sandbox Architecture (Hybrid Workspace)
When NeuroPlasticity runs your test, it spins up a container. To ensure safety and speed, it uses a **Split Workspace**:
*   **`/project` (Read-Only):** The user's entire repository is mounted here. You can read the code, but you CANNOT modify the host project directly. This guarantees host safety.
*   **`/workspace` (Read-Write):** A temporary, ephemeral scratch directory. **You must write your outputs, refactors, or generated files here.** This approach eliminates slow deep-copies.

## 📜 2. Writing the `plasticity.json`
This file defines the sandbox. You must define the task, map the user's authentication configs (so you don't need API keys), write your `setup_script` to install yourself, and write a strict bash evaluator.

**Example `plasticity.json` (e.g., for Claude Code):**
```json
{
  "name": "my-agent-self-evaluation",
  "task_prompt": "Read the config files in /project and output a summary to /workspace/summary.json",
  "agent_command": [
    "bash", "-c", 
    "cat .neuroplasticity/rules.json > rules.txt && my-agent-cli --prompt-file rules.txt 'Analyze /project and save to /workspace/summary.json'"
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
        "source": "~/.config/my-agent-cli",
        "target": "/root/.config/my-agent-cli",
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
      "name": "Verify JSON Output",
      "type": "host_bash",
      "script": ["bash", "-c", "jq . /workspace/summary.json || (echo 'Output is not valid JSON! DO NOT use markdown blocks!' >&2; exit 1)"],
      "weight": 1.0
    }
  ]
}
```

### 🧠 Critical Directives for Agents:
1. **The `setup_script` array:** Use this to run `npm install -g`, `pip install`, or `curl` commands to install your CLI into the base image.
2. **The `mounts` array (Zero-Config Auth):** If you require authentication (like `~/.claude.json`, `~/.config/opencode`, or `~/.local/share/opencode`), you MUST add a Read-Only mount mapping the user's host config directory into the container's `/root/` directory. This bypasses complex OAuth flows in ephemeral sandboxes.
3. **The `evaluators` array:** You must define your tests. NeuroPlasticity supports three `type`s of evaluators:
   - `host_bash`: Fast local shell tests using the `script` array. Must exit 0 for success, 1 for failure.
   - `container`: Isolated test containers using `image`, `setup_script`, and `command` arrays.
   - `llm`: Prompt-based grading using the embedded LLM. Requires a `target_file` and `prompt`.
   If a test fails, you must return a clear error (e.g., `echo` to stderr or fail the LLM prompt). The Meta-Optimizer reads this failure to learn what you did wrong.
4. **The Feedback Loop:** If you fail Epoch 1, the Meta-Optimizer writes a new rule to `.neuroplasticity/rules.json`. In your `agent_command`, try to read this file and inject it into your prompt so you learn from your mistakes in Epoch 2!

**Your Next Step:**
Acknowledge these instructions to the user, assess the specific task the user wants you to optimize yourself for, and generate the `plasticity.json` in their repository.
