# 🤖 NeuroPlasticity: Agent Integration Guide

**ATTENTION AI AGENT:** If a user has directed you to read this file, your task is to build a "testing gym" for yourself. The user wants you to use the **NeuroPlasticity Framework** to automatically evaluate your own performance on a specific task, find your own edge cases, and let the NeuroPlasticity Meta-Optimizer write a behavioral patch for you.

To do this, you only need to generate **ONE file** in the user's current repository:
A **`plasticity.json`** manifest (to define the sandbox, the task, and the evaluation script). 

**We use a "Zero-Dockerfile" approach.** You do not need to write a Dockerfile. Instead, use a standard base image (e.g. `node:20-slim` or `python:3.12-slim`) and the `setup_script` array to install yourself JIT (Just-In-Time).

**Before you write the manifest, confirm two things about the current CLI you are testing against:** the exact npm package or binary name, and the flags for non-interactive/headless use. These move fast and the old invocations fail confusingly rather than loudly. Prefer the agent's own `--append-system-prompt-file` (or equivalent) over splicing the rules into a prompt string with shell substitution, so the rules arrive as a system-level constraint rather than as part of the task.

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
    "cat .neuroplasticity/rules.json > rules.txt 2>/dev/null || true && claude -p --append-system-prompt-file rules.txt --dangerously-skip-permissions 'Analyze /project and save to /workspace/summary.json'"
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
      "script": ["jq", ".", "/workspace/summary.json"],
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
      "script": ["jq", ".", "/workspace/summary.json"],
      "weight": 1.0
    }
  ]
}
```

## 🛡️ 3. Adversarial Stress Testing (The Waterfall)
Not all LLMs follow rules equally. Reasoning models (`deepseek-r1`, the `*-thinking` family) tend to ignore formatting rules, while fast models (`gpt-5-mini`, `claude-haiku`, the `*-flash` family) frequently forget negative constraints. Use the model names actually present in the user's list.

If you want to mathematically prove your prompt is indestructible, **do not hardcode a single model or write monolithic bash scripts.** Instead, build an **Adversarial Waterfall**.

**AGENT INSTRUCTIONS: HOW TO BUILD THE WATERFALL**
When the user asks you to set up tests, you must discover what models are available on their specific machine, categorize them, and generate a sequence of JSON test files.

**Step 1: Discovery**
Use your own CLI tool via your host terminal to pull the user's available model list. Check whichever agents are actually installed (e.g. `opencode models`, or a provider's own `/models` command). Do not assume a model list from a vendor that is no longer installed or whose service has been retired — a stale list produces tests that can never pass.

**Step 2: Categorization**
Search the list you just pulled and identify the worst offenders in these three Archetypes:
1. **The Reasoning Rebel:** Look for models containing `o1`, `o3`, `r1`, `deepseek`, `codex`, or `thinking`. (These ignore formatting).
2. **The Fast & Forgetful:** Look for models containing `mini`, `haiku`, `flash`, `nano`, or `fast`. (These drop rules).
3. **The Wildcard:** Look for models containing `grok`, `experimental`, or random names like `pickle`. (These add chatty conversational filler).

**Step 3: Generate the Test Suite**
Create a directory (e.g., `.neuroplasticity/tests/`) and generate a separate `plasticity.json` for each archetype you found. The only difference between these files should be the model flag (e.g., `-m`) in the `agent_command`.
- `test-01-reasoning.json`
- `test-02-forgetful.json`
- `test-03-wildcard.json`

**Step 4: The Regression Loop (Tell the User)**
Instruct the user to run them sequentially (Test 1, then Test 2, then Test 3). 
Because NeuroPlasticity accumulates its lessons in `.neuroplasticity/rules.json`, the prompt will get stronger with each test. **CRITICAL:** If Test 3 fails and requires the Meta-Optimizer to generate a new rule, tell the user they *must* restart the waterfall from Test 1 to ensure the new rule didn't cause a regression in the Reasoning model.

When all tests in the waterfall pass on Epoch 1, the accumulated `neuroplasticity_patch.md` is universally bulletproof.

### 🧠 Critical Directives for Agents:
1. **Choose Your Installation Strategy:** If you are an NPM package, use **Strategy A**. If you are a native pre-compiled binary, use **Strategy B**. NEVER mix the two (do not mount a host binary into a `node:20-slim` container, and do not try to run `npm install -g opencode`).
2. **Pick a base image that matches the agent's Node requirement.** `node:20-slim` is right for most CLIs, but some (e.g. `@github/copilot`) require Node 22+. An `npm install -g` that cannot resolve its engine constraint fails at setup time with a confusing error.
3. **The `mounts` array (Zero-Config Auth):** Map the user's host config directory into the container's `/user_home/` directory. **CRITICAL WARNING FOR SQLITE:** If your agent relies on a local SQLite database for state (like `~/.local/share/opencode`), you MUST mount it with `"readonly": false`. If you mount an SQLite database as read-only, the agent will crash trying to acquire a WAL (Write-Ahead Log) lock.
4. **Do not weaken an evaluator to make it pass.** A rule that satisfies one evaluator by breaking another is detected and marked a `REGRESSION` in the patch, and the run exits non-zero. `pass_threshold` is run-level (`passing_weight / total_weight`), so a run can pass overall while an individual evaluator regresses — which is exactly why the per-evaluator delta table exists. If your evaluator is wrong, fix the evaluator deliberately, not by widening a rule until the test is satisfied.
5. **If your rules encode a safety or compliance constraint, mark it protected.** In any project where a constraint is load-bearing, an automated loop that rewrites rules until tests pass will eventually trade the constraint for the test. Mark it and the optimizer never sees it:

```json
"optimization": {
  "rules": { "policy": {
    "protected": ["Escalate emergencies to 911 immediately", "Never disclose credentials"],
    "protected_match": "exact"
  }}
}
```

An attempted change to a protected rule is reverted and reported for human sign-off — it is never presented as an improvement. If your material is sensitive, also declare a `data_class` so the harness refuses to send it to an unlisted provider; run `--print-egress-plan` to see the outbound path first.

6. **Make failures attributable, and assert cross-cutting invariants.** If your agent works in steps, have it emit a run transcript as JSON Lines in `/workspace/transcript.jsonl`, one object per step: `{"id": "s1", "kind": "lookup", "status": "failed", "detail": "no availability"}`. Then name the step an evaluator judges with `"unit": "s1"` — the optimizer is shown that step instead of the whole run, so the rule it writes is scoped rather than global. For properties that span the run, add an invariant: `{"name": "never-books-after-failed-lookup", "kind": "invariant", "assert": "no_action_after_failure"}`, which supports `no_action_after_failure`, `at_most_once` (needs `unit`), and `no_failed_steps`. Transcripts are optional; without one, localization and invariants are skipped rather than failing.

7. **The `evaluators` array:** You must define your tests. NeuroPlasticity supports three `type`s of evaluators:
   - `host_bash`: Fast local tests that run directly on the **host** machine. Must exit 0 for success, non-zero for failure. Because it is not sandboxed, the first argument must be one of `git`, `jq`, `cat`, `ls`, `grep`, or `echo`; `bash -c` and anything else are rejected as sandbox-escape attempts. Use a direct argument list and rely on the exit code — the tool's stderr is captured for the optimizer. Remember `grep -q PATTERN FILE` exits **zero when it finds a match**, so "must not contain" checks need a `container` evaluator.
   - `container`: Isolated test containers using `image`, `setup_script`, and `command` arrays.
   - `llm`: Prompt-based grading using the configured Meta-Optimizer LLM (`optimization.meta_llm`). Requires a `target_file` and `prompt`. Returns a structured `{"verdict": "PASS"|"FAIL"|"INDETERMINATE", "reason": "..."}` object, so write the `prompt` as a grading criterion and let the framework handle the output contract. The grader may answer `INDETERMINATE` when the artifact is undecidable; that is not counted as a failure and will halt the run rather than become a rule, so make your prompt specific enough to decide. If the model endpoint is unreachable, the run also **aborts** rather than recording a failure — that is deliberate, so infrastructure problems are never mistaken for your mistakes. Add a `graders` array for a quorum; graders that disagree yield `INDETERMINATE`, not `FAIL`.
   If a test fails, you must return a clear error (e.g., `echo` to stderr or fail the LLM prompt). The Meta-Optimizer reads this failure to learn what you did wrong. Evaluators are run entirely **asynchronously in parallel**, so execution is extremely fast regardless of how many tests you write.
8. **Choosing the Meta-Optimizer model** (`optimization.meta_llm`): `"provider": "embedded"` is the default and needs no API key (a 4-bit Qwen3 model is downloaded once). To use a hosted model instead, set `"provider": "custom"` with `base_url` and `api_key_env` — the key must be exported in the shell that runs the CLI. Both `chat/completions` and `responses` endpoints are supported; set `api_style` if the URL does not make the protocol obvious. Grading quality depends heavily on this choice: a weak grader produces confidently wrong verdicts, which is worse than no LLM evaluator at all.
9. **Timeouts & Safety:** By default, NeuroPlasticity kills the sandbox container if your agent takes longer than 120 seconds to execute. If you are testing a slow reasoning model, you can override this by adding `"timeout_seconds": 300` to the `sandbox` block.
10. **The Fast-Path Cache:** NeuroPlasticity hashes your test configuration (command, model, provider, endpoint, active rules). If you run a test that previously failed with the exact same rules, the orchestrator will instantly skip the 120s execution and reload the cached failure logs. Don't be surprised if your test returns in 0.1 seconds! Because this caches *failures*, changing the model or endpoint also invalidates the cache — which is correct, since a different backend is a different grader.
11. **The Feedback Loop & The Patch:** If you fail Epoch 1, the Meta-Optimizer writes a new rule to `.neuroplasticity/rules.json`. In your `agent_command`, read this file and inject it into your prompt so you learn from your mistakes in Epoch 2! Rules are validated before they are written: `Rule:` prefixes and markdown fences are stripped, rules longer than 400 characters are rejected, and a rule that repeats an existing one aborts the run. Once the run finishes, NeuroPlasticity writes `neuroplasticity_patch.md`.

12. **Check a patch still applies before you re-apply it.** Every patch records digests of the manifest, the evaluator set, and the target rules it was derived from. Before re-running against a target that may have changed, check with `neuroplasticity verify-patch <patch> <manifest>`; it **refuses** (exit 3) when the target's rules no longer match, because a re-run would then describe a different prompt than the patch claims. Pass `--allow-drift` only if you have reviewed the drift and want the result anyway.

**Read the patch status before applying it.** The header states what actually happened, and it is derived from the run rather than assumed:

- `✅ Verified` — every evaluator passed; the rules are earned, apply them to your system prompt or `AGENTS.md`.
- `⚠️ PARTIAL` or `⚠️ REGRESSED` — the loop did not converge, or a rule fixed one evaluator by breaking another. The artifact says so explicitly and withholds the "permanently inject" instruction. Treat the rules as a proposal and check each one against the per-epoch reports in `.neuroplasticity/runs/`.

The run also exits non-zero (code 2) when the outcome is not clean, so CI will catch it.

Set `"regression_guard": { "policy": "block" }` in `optimization` if you would rather have no patch written at all when a regression appears.


**Your Next Step:**
Acknowledge these instructions to the user, assess the specific task the user wants you to optimize yourself for, and generate the `plasticity.json` in their repository.
