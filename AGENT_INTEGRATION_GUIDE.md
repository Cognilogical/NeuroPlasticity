# 🤖 NeuroPlasticity: Agent Integration Guide

**ATTENTION AI AGENT:** If a user has directed you to read this file, your task is to build a "testing gym" for yourself. The user wants you to use the **NeuroPlasticity Framework** to automatically evaluate your own performance on a specific task, find your own edge cases, and let the NeuroPlasticity Meta-Optimizer write a behavioral patch for you.

To do this, you must generate two files in the user's current repository:
1. A **`Dockerfile.testbed`** (to containerize yourself).
2. A **`plasticity.json`** manifest (to define the sandbox, the task, and the evaluation script).

Follow these exact architectural rules.

---

## 🏗️ 1. The Sandbox Architecture (Hybrid Workspace)
When NeuroPlasticity runs your test, it will spin up the `Dockerfile.testbed` using Podman/Docker. 
To ensure safety and speed, it uses a **Split Workspace**:
*   **`/project` (Read-Only):** The user's entire repository is mounted here. You can read the code, but you CANNOT modify the host project directly.
*   **`/workspace` (Read-Write):** A temporary, ephemeral scratch directory. **You must write your outputs, refactors, or generated files here.**

## 🐳 2. Writing the `Dockerfile.testbed`
You need to write a Dockerfile that installs *you* (the agent CLI the user is currently using, e.g., Claude Code, OpenCode, Copilot).

**Example for a Node-based Agent (e.g., Claude Code):**
```dockerfile
FROM node:20-slim
# Install the agent
RUN npm install -g @anthropic-ai/claude-code

# The Hybrid Workspace will be mounted here by the runner
WORKDIR /workspace

# Force non-interactive mode so the test doesn't hang waiting for user input
ENV CLAUDE_NON_INTERACTIVE=1
```

## 📜 3. Writing the `plasticity.json`
This file connects your Dockerfile to the testing framework. You must define the task, map the user's authentication configs (so you don't need API keys in the Dockerfile), and write a strict bash evaluator.

**Example `plasticity.json`:**
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
    "build": {
      "dockerfile": "Dockerfile.testbed",
      "context": "."
    },
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
      "script": ["bash", "-c", "jq . /workspace/summary.json || (echo 'Output is not valid JSON! DO NOT use markdown blocks!' >&2; exit 1)"],
      "weight": 1.0
    }
  ]
}
```

### 🧠 Critical Directives for Agents:
1. **The `mounts` array:** If you (the agent) require authentication (like `~/.claude.json` or `~/.config/gh`), you MUST add a Read-Only mount mapping the user's host config directory into the container's `/root/` directory. This allows you to run without hardcoding API keys.
2. **The `evaluators` script:** You must write a bash script that returns `exit 0` on success and `exit 1` on failure. If the test fails, **you must `echo` a highly descriptive error message to `>&2` (stderr)**. The Meta-Optimizer reads this `stderr` message to learn what you did wrong.
3. **The Feedback Loop:** If you fail Epoch 1, the Meta-Optimizer will write a new rule to `.neuroplasticity/rules.json`. In your `agent_command`, you should try to `cat` or read this file and prepend it to your prompt so you learn from your mistakes in Epoch 2!

**Your Next Step:**
Acknowledge these instructions to the user, assess the specific task the user wants you to optimize yourself for, and generate the `Dockerfile.testbed` and `plasticity.json` in their repository.
