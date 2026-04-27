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
This file defines the sandbox. You must define the task, map the user's authentication configs (so you don't need API keys), configure the `sandbox` to install or mount yourself, and write a strict bash evaluator.

**You MUST choose the correct setup strategy based on how your agent is installed on the host:**

### Strategy A: The Agent is an NPM Package (e.g., Claude Code)
If the agent is installed globally via npm (e.g., `@anthropic-ai/claude-code`), do NOT mount the host binary. Use a `node:20-slim` base image and install it fresh using the `setup_script` array.

```json
{
  "name": "claude-code-self-evaluation",
  "task_prompt": "Read the config files in /project and output a summary to /workspace/summary.json",
  "agent_command": [
    "bash", "-c", 
    "cat .neuroplasticity/rules.json > rules.txt 2>/dev/null || true && claude -p \"$(cat rules.txt)\n\nAnalyze /project and save to /workspace/summary.json\""
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
      "name": "Verify JSON Output",
      "type": "host_bash",
      "script": ["bash", "-c", "jq . /workspace/summary.json || exit 1"],
      "weight": 1.0
    }
  ]
}
```

### Strategy B: The Agent is a Host-Compiled Binary (e.g., Opencode)
If the agent is a pre-compiled native binary located in the host's home directory (e.g., `~/.opencode/bin/opencode`), do NOT try to install it via NPM (it will 404). You can simply map the host binary directly into the container. You do NOT need a `setup_script` to install system dependencies like `apt-get` (which would fail with Permission Denied under non-root sandboxes). Use a standard image like `node:20-slim`.

```json
{
  "name": "opencode-self-evaluation",
  "task_prompt": "Read the config files in /project and output a summary to /workspace/summary.json",
  "agent_command": [
    "bash", "-c", 
    "cat .neuroplasticity/rules.json > rules.txt 2>/dev/null || true && /usr/local/bin/opencode run --dangerously-skip-permissions \"$(cat rules.txt)\n\nAnalyze /project and save to /workspace/summary.json\""
  ],
  "sandbox": {
    "engine": "podman",
    "base_image": "node:20-slim",
    "setup_script": [],
    "workspace": {
      "project_mount": "/project",
      "scratch_mount": "/workspace"
    },
    "mounts": [
      {
        "source": "~/.opencode/bin/opencode",
        "target": "/usr/local/bin/opencode",
        "readonly": true
      },
      {
        "source": "~/.config/opencode",
        "target": "/user_home/.config/opencode",
        "readonly": true
      },
      {
        "source": "~/.local/share/opencode",
        "target": "/user_home/.local/share/opencode",
        "readonly": false
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
      "script": ["bash", "-c", "jq . /workspace/summary.json || (echo 'Output is not valid JSON!' >&2; exit 1)"],
      "weight": 1.0
    }
  ]
}
```

## 🛡️ 3. Adversarial Stress Testing (Optional but Highly Recommended)
Not all LLMs follow rules equally. Reasoning models (like `o3-mini` or `codex-max`) stubbornly ignore formatting rules, while fast models (like `gpt-5-mini` or `claude-haiku`) frequently forget negative constraints. 

If you want to prove your prompt is indestructible, you can dynamically query the user's available models and automatically test yourself against the worst offender.

**For Opencode:**
Instead of hardcoding a model, use this dynamic bash script in your `agent_command` to find the user's most difficult configured model:

```json
  "agent_command": [
    "bash", "-c", 
    "MODELS=$(/usr/local/bin/opencode models 2>/dev/null || echo ''); WORST=$(echo \"$MODELS\" | grep -iE 'o1|o3|r1|deepseek|codex-max' | head -n 1); if [ -z \"$WORST\" ]; then WORST=$(echo \"$MODELS\" | grep -iE 'mini|haiku|flash|nano' | head -n 1); fi; CMD=\"/usr/local/bin/opencode run\"; if [ -n \"$WORST\" ]; then CMD=\"$CMD -m $WORST\"; fi; cat .neuroplasticity/rules.json > rules.txt 2>/dev/null || true && $CMD --dangerously-skip-permissions \"$(cat rules.txt)\n\nAnalyze /project and save to /workspace/summary.json\""
  ]
```

**For Claude Code:**
Claude Code is tightly coupled to Anthropic and does not support hundreds of OpenRouter models. The list of models is universally static. To stress-test Claude Code, explicitly hardcode the "fast and forgetful" model (`claude-3-5-haiku-20241022`):

```json
  "agent_command": [
    "bash", "-c", 
    "cat .neuroplasticity/rules.json > rules.txt 2>/dev/null || true && claude -m claude-3-5-haiku-20241022 -p \"$(cat rules.txt)\n\nAnalyze /project and save to /workspace/summary.json\""
  ]
```

### 🧠 Critical Directives for Agents:
1. **Choose Your Installation Strategy:** If you are an NPM package, use **Strategy A**. If you are a native pre-compiled binary, use **Strategy B**. NEVER mix the two (do not mount a host binary into a `node:20-slim` container, and do not try to run `npm install -g opencode`).
2. **The `mounts` array (Zero-Config Auth):** Map the user's host config directory into the container's `/user_home/` directory. **CRITICAL WARNING FOR SQLITE:** If your agent relies on a local SQLite database for state (like `~/.local/share/opencode`), you MUST mount it with `"readonly": false`. If you mount an SQLite database as read-only, the agent will crash trying to acquire a WAL (Write-Ahead Log) lock.
3. **The `evaluators` array:** You must define your tests. NeuroPlasticity supports three `type`s of evaluators:
   - `host_bash`: Fast local shell tests using the `script` array. Must exit 0 for success, 1 for failure.
   - `container`: Isolated test containers using `image`, `setup_script`, and `command` arrays.
   - `llm`: Prompt-based grading using the embedded LLM. Requires a `target_file` and `prompt`.
   If a test fails, you must return a clear error (e.g., `echo` to stderr or fail the LLM prompt). The Meta-Optimizer reads this failure to learn what you did wrong.
4. **The Feedback Loop & The Patch:** If you fail Epoch 1, the Meta-Optimizer writes a new rule to `.neuroplasticity/rules.json`. In your `agent_command`, try to read this file and inject it into your prompt so you learn from your mistakes in Epoch 2! Once you successfully pass an epoch, NeuroPlasticity will output a final `neuroplasticity_patch.md` in the root directory. You should read this file and permanently add those successful rules to your own system prompt or the repository's `AGENTS.md`.

**Your Next Step:**
Acknowledge these instructions to the user, assess the specific task the user wants you to optimize yourself for, and generate the `plasticity.json` in their repository.
