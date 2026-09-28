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
*   **Fail Loud, Never Silent:** API errors are surfaced with their status and body instead of being coerced into plausible-looking model output. An LLM evaluator that cannot be reached aborts the run rather than recording a failure the agent never caused.
*   **Honest Artifacts:** The patch header states what actually happened, derived from the run rather than hardcoded. A failed or regressed run never claims its rules were verified, and never instructs you to inject them.
*   **Protected Rules & Data Egress (opt-in):** Mark safety/compliance rules immutable to the optimizer — they are hidden from it and any attempted change is quarantined for human sign-off. Declare a data class to control which providers may receive your material, with `--print-egress-plan` to inspect the outbound path before a run.
*   **Noisy Graders Can't Write Rules:** A grader may answer `INDETERMINATE` when a document is undecidable. That verdict is excluded from scoring and halts the run rather than becoming a rule, and every verdict is recorded with its model, endpoint, and prompt hash so a patch decision stays re-verifiable.
*   **Attributable Failures & Cross-Cutting Invariants:** Agents can emit a JSON-lines run transcript, which lets a failure be pinned to one step (so the rule written for it is scoped, not global) and lets you assert properties that span a whole run — "never books after a failed lookup", "at most one booking" — which unit-style tests cannot reach.
*   **Re-verifiable Patches:** Patches record digests of the manifest, evaluators, and target rules they came from. `verify-patch` refuses to re-verify a target whose prompt has drifted, rather than reporting a result for a different artifact.
*   **Grader Quorums:** An `llm` evaluator can declare several graders with `primary` / `veto` / `audit` roles. Disagreement yields `INDETERMINATE` rather than `FAIL`, and agreement is reported as raw % and Cohen's κ — because graders that always agree can still be useless.
*   **Offline First via `llama.cpp`:** Run fully disconnected. Compile with `cargo run --features embedded-llm` to automatically pull and run a 4-bit `Qwen3` model directly in your computer's memory. To respect user disk space, NeuroPlasticity does not download duplicate models. It defaults to scanning universal POSIX caches (`~/.cache/neuro/models/`, `~/.cache/huggingface/hub/`, `~/.ollama/models/blobs/`, `~/.cache/lm-studio/models/`) to prevent redundant GGUF model downloads; the candidate list is editable at `~/.config/NeuroPlasticity/models.json`. Prompts are assembled with the chat template embedded in each GGUF, so non-Qwen models are not silently mis-prompted. (Features a concurrency Semaphore to protect RAM when running parallel evaluators).
*   **Release binaries include the offline engine:** The published artifacts are built with `--features embedded-llm`, and CI asserts the engine is actually present before uploading. Builds without that flag compile successfully but abort at runtime on `provider: "embedded"`, so the check is enforced rather than documented.
*   **Declarative `plasticity.json`:** Define your tasks, sandbox constraints, auth mounts, and determinism.
*   **Tri-State Evaluators:** Evaluate your agents exactly how you need:
    1. `host_bash`: Fast, lightweight POSIX shell commands running locally.
    2. `container`: Isolated evaluation containers for heavy dependencies (Node.js, `pytest`, etc.) without host pollution.
    3. `llm`: Schema-constrained prompt grading for nuanced checks (tone, style), returning a structured `{"verdict": "PASS"|"FAIL"|"INDETERMINATE", "reason": "..."}` object. Uses whichever model `optimization.meta_llm` points at, embedded or hosted, optionally across a quorum of graders. An `INDETERMINATE` is excluded from scoring and halts the run rather than becoming a rule.

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
    "cat .neuroplasticity/rules.json > rules.txt && claude -p --append-system-prompt-file rules.txt --dangerously-skip-permissions 'Analyze /project and save to /workspace/summary.json'"
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
      "script": ["jq", ".", "/workspace/summary.json"],
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
No API keys required for the Meta-Optimizer. If you downloaded the pre-compiled binary from our releases page, it already includes the embedded `llama.cpp` engine. It will automatically download a fast 4-bit `Qwen3` model to your local cache.
```bash
./neuroplasticity-linux-x86_64
# Or on Mac: ./neuroplasticity-macos-aarch64
```

*(If you are compiling from source, use `cargo run --release --features embedded-llm`)*

### Using a hosted model instead (e.g. OpenCode Zen)

`optimization.meta_llm` is the only place the model API is configured. Set `provider` to `custom` and point `base_url` at any OpenAI-compatible `/chat/completions` endpoint:

```json
"meta_llm": {
  "provider": "custom",
  "model": "deepseek-v4-flash",
  "base_url": "https://opencode.ai/zen/v1/chat/completions",
  "api_key_env": "OPENCODE_API_KEY"
}
```

Then export the key in the shell that runs the CLI:

```bash
export OPENCODE_API_KEY=your-key
```

Notes:
- This client speaks both `chat/completions` and `responses`. On OpenCode Zen that covers `deepseek-*`, `glm-*`, `kimi-*`, `minimax-*`, `qwen3.8-*` and the `*-free` models on `chat/completions`, plus the `gpt-*` family on `responses`. To use a `gpt-*` model, point `base_url` at the responses endpoint and it is detected automatically:

```json
"meta_llm": {
  "provider": "custom",
  "model": "gpt-5.5",
  "base_url": "https://opencode.ai/zen/v1/responses",
  "api_key_env": "OPENCODE_API_KEY"
}
```

  Set `api_style` to `"chat_completions"` or `"responses"` explicitly to override the inference. The `claude-*` (`/v1/messages`) and `jev-*` (`/v1/systemone`) families use different protocols and are not supported.
- Grader verdicts and optimizer rules are requested as JSON Schema. Providers that reject `response_format` (or `text.format`) are detected from the 400 response and the constraint is dropped automatically, falling back to a prompt-only contract.
- An LLM evaluator that cannot be reached (bad key, dead endpoint) aborts the run instead of counting as an agent failure. A framework that penalizes the agent for its own infrastructure problems produces meaningless rules.
- `temperature` defaults to `0.0` for reproducible grading. Providers that reject it are detected from the 400 response and the field is dropped automatically, so reasoning models work too.
- Changing `provider`, `model`, or `base_url` invalidates the failure-fingerprint cache, so a new backend never replays another backend's cached verdicts.
- To test an endpoint without a real key, use the ignored round-trip test:
  `NP_TEST_BASE_URL=https://opencode.ai/zen/v1/chat/completions NP_TEST_API_KEY_ENV=OPENCODE_API_KEY cargo test --release --features embedded-llm -- --ignored custom_endpoint`


### What Happens:
*   **Epoch 1:** NeuroPlasticity mounts your host project as Read-Only (`/project`), installs the agent JIT, and runs it. The agent writes the file, but includes markdown backticks. `jq` fails with a parse error.
*   **The Meta-Optimizer:** Your local embedded LLM reads the `jq` failure log. It autonomously writes a new system rule: *"CRITICAL: When outputting JSON to a file, DO NOT wrap the output in markdown code blocks (\`\`\`json). You must output raw JSON text only."* It saves this to `.neuroplasticity/rules.json`.
*   **Epoch 2:** The agent runs again. Because the `agent_command` injects `.neuroplasticity/rules.json` into Claude's prompt, it now knows exactly what to avoid. It outputs raw JSON. The `jq` evaluator passes!
*   **The Patch:** NeuroPlasticity outputs `neuroplasticity_patch.md`. You simply copy that mathematically verified rule and paste it permanently into your agent instructions.

## 🛠️ Advanced Topics

### 1. Baking in Heavy Dependencies (MCP Servers)
If your agent relies on heavy external tools like `sqlite`, a Python environment, or an MCP (Model Context Protocol) server, the JIT `setup_script` might be too slow. In this case, build a custom `Containerfile` or `Dockerfile` and point your `plasticity.json` to that image instead.

### 3. Patch Honesty, Regression Guard, Protected Rules & Egress

`pass_threshold` is a **run-level** scalar: the score is `passing_weight / total_weight` across all evaluators, so a passing run can still hide an individually failing evaluator. NeuroPlasticity captures a per-evaluator baseline on the first evaluated epoch, before any rule is mutated, and diffs against it afterwards.

Any evaluator that passed at baseline and fails now is a **regression** — a rule that fixed something by breaking something else. Regressions are announced on stdout, listed in the patch, and surfaced in a delta table:

```
| evaluator             | baseline | final | delta        |
|-----------------------|----------|-------|--------------|
| `jq: schema valid`    | PASS     | PASS  | —            |
| `llm: tone is warm`   | FAIL     | PASS  | improved     |
| `host_bash: no secrets` | PASS   | FAIL  | **REGRESSION** |
```

The patch header is derived from the actual outcome, never assumed:

| Outcome | Status line | "permanently inject" advice | Exit code |
|---|---|---|---|
| All evaluators passed | `✅ Verified…` | yes | 0 |
| A manifest exhausted its epochs | `⚠️ PARTIAL — … UNVERIFIED` | **no** | 2 |
| An evaluator regressed | `⚠️ REGRESSED — …` | **no** | 2 |

A non-clean run no longer prints "You should permanently inject these into your system prompt" — advice the run has not earned. The warning lives in the artifact, not just stdout, since the artifact is what gets handed to another agent. Each patch also carries a machine-readable block so a consumer can filter without parsing prose:

```markdown
<!-- neuroplasticity:status
outcome: regressed
rules_verified: false
regressed_evaluator: host_bash: no secrets
-->
```

To refuse the patch entirely when a regression appears, set the guard to `block`:

```json
"optimization": {
  "regression_guard": { "policy": "block" }
}
```

Omitting `regression_guard` keeps the default `annotate` behavior: report the regression, mark the patch, still emit it.

### 4. Protected Rules & Data Egress

Two opt-in blocks keep the automated loop away from the parts of your prompt it shouldn't touch. **Both are absent by default, so existing manifests are unaffected.**

**Protected rules (F2).** A safety or compliance constraint and a sandboxed behavioral tweak are not the same kind of data. Mark the ones the optimizer may never modify:

```json
"optimization": {
  "rules": { "policy": {
    "protected": ["Escalate emergencies to 911 immediately"],
    "protected_match": "prefix"
  }}
}
```

Protected rules are **hidden from the optimizer's prompt** rather than merely protected afterward, because a model shown a constraint tends to reword it. If a change is attempted anyway, it is reverted in the rules file, recorded in `neuroplasticity_quarantine.md`, reported in the patch under "Quarantined constraint changes (NOT applied)", and never described as an improvement. `protected_match` accepts `exact` (default) and `prefix`, since rule text drifts.

The rules file itself accepts both shapes, so existing files keep working:

```json
["Behavior rule the optimizer may edit"]
```

```json
[{"class": "constraint", "text": "Never disclose credentials"}]
```

**Data egress (F3).** Declare how sensitive the material is, and which providers may receive it:

```json
"optimization": {
  "data": {
    "data_class": "regulated",
    "egress": { "allow": [
      { "provider": "embedded",  "max_class": "restricted" },
      { "provider": "hosted",    "max_class": "internal" }
    ] }
  }
}
```

Classes are ordered `public` < `internal` < `regulated` < `restricted`. With no `data_class`, nothing is enforced. With one, **unlisted hosted providers are denied** and a denial aborts the run before any container starts, naming the provider, the class, and the fix. `embedded` is always permitted regardless — local inference never leaves the machine, which is what lets you run restricted data with a local model and an empty allow list.

Check what a manifest would do before running it:

```bash
./neuroplasticity plasticity.json --print-egress-plan
```

```
Data class: regulated
meta_llm provider: custom

  meta-optimizer    hosted → DENIED  (regulated data to a hosted provider: not listed…)
```

### 5. Grader Verdicts: `INDETERMINATE` and Provenance

An LLM grader can return a third verdict, `INDETERMINATE`, meaning the document could not be judged from what was shown. It is deliberately **not** a `FAIL`:

- An `INDETERMINATE` is **excluded from scoring** rather than counted as a failure, so a flaky grader cannot drag a run below threshold.
- It is **never fed to the optimizer**. A run that receives one is halted with a diagnostic instead of optimized against a guess — passing an undecidable artifact to the optimizer as a failing log is exactly how noise becomes a rule.
- The prompt tells the grader to reserve it for genuinely undecidable cases, never as a substitute for `FAIL`.

Every verdict is recorded with the model, endpoint, temperature, and a hash of the grading prompt that produced it, and surfaced in the patch:

```
### Grader provenance

- `custom/gpt-5.5 (temp=0, prompt=abcdef012345)` — verdict `PASS` · endpoint `https://opencode.ai/zen/v1/responses`
```

A patch decision should be re-verifiable months later, which requires knowing which model and prompt produced it.

### 6. Budgets

`optimization` takes an optional cap on what a run may consume. Omit it and nothing is capped.

```json
"optimization": {
  "budget": {
    "max_wall_clock_seconds": 900,
    "max_usd": 5.00,
    "on_exceed": "halt"
  }
}
```

The budget is checked **before each epoch**, so a halted run stops promptly rather than after another 120-second container spin-up. `on_exceed` defaults to `halt`; set it to `warn` to log and continue.

A run that halts on budget has reached no conclusion either way, so it is reported as a distinct outcome — `🛑 HALTED ON BUDGET` — not as a pass and not as a partial run:

```
**Status:** 🛑 HALTED ON BUDGET — wall clock 900s exceeded the 300s budget. The run stopped
before it reached a conclusion, so nothing here was verified either way.
```

### 7. Transcripts, Failure Localization & Invariants

**F6a — the ordered transcript.** An agent can emit a run transcript as JSON Lines in its scratch workspace (`transcript.jsonl`), one step per line:

```json
{"id": "s1", "kind": "lookup", "status": "failed", "detail": "no availability"}
{"id": "s2", "kind": "book",   "status": "ok",     "detail": "booked flight-123"}
```

Fields: `id` (stable, unique), `kind`, `status` (`ok` / `failed` / `skipped`), and optional `input_ref`, `output_ref`, `detail`. This is **additive** — stdout/stderr capture is untouched, and a missing or unusable transcript never fails a run. A truncated final line is tolerated, so an agent killed mid-write still yields the steps it completed.

**F5 — failure localization.** The optimizer previously received one whole-run blob, so a failure local to step 7 could only produce a *global* rule — and global rules are how an optimizer fixes one behavior and quietly changes three others. An evaluator can now name the step it judges:

```json
{ "name": "no availability message", "unit": "s1", "type": "host_bash", "script": [...], "weight": 1.0 }
```

When that step actually failed, the optimizer is shown *that step* with one step of surrounding context, and the patch names it:

```
#### Rule 2 (step `s1`)
> When a lookup returns no availability, state that plainly instead of booking.
```

**F6 — cross-cutting invariants.** Unit-style tests cannot express "never does X after Y" or "at most one of these is ever true" — precisely the properties that matter in long-horizon agents. An evaluator with `kind: "invariant"` is checked against the whole ordered transcript:

```json
{
  "name": "never-books-after-failed-lookup",
  "kind": "invariant",
  "assert": "no_action_after_failure",
  "weight": 1.0
}
```

| `assert` | Meaning |
|---|---|
| `no_action_after_failure` | No successful action of any kind after a step failed |
| `at_most_once` | Steps of `unit` occur at most once across the run |
| `no_failed_steps` | No step in the run failed |

A violation names the **transition** that broke it (`s1 → s2`), not just the run. The set is deliberately restricted to properties decidable from the transcript alone, so invariants stay deterministic rather than asking a model to reason about a sequence. An unknown `assert` fails loudly instead of passing silently, and an agent that emits no transcript causes invariants to be **skipped**, not failed.

Note that `no_action_after_failure` compares ordering, not kinds — a `book` after a `lookup` failure violates it even though the kinds differ, which is the case a naive kind check would miss.

### 8. Grader Quorum & Agreement

One grader is one noisy oracle. An `llm` evaluator can declare several, each with a role:

```json
{
  "name": "tone is warm",
  "type": "llm",
  "target_file": "out.txt",
  "prompt": "Fail if it uses first-person pronouns.",
  "graders": [
    { "name": "gpt-5.5", "role": "primary",
      "meta_llm": { "provider": "custom", "model": "gpt-5.5",
                    "base_url": "https://opencode.ai/zen/v1/responses",
                    "api_key_env": "OPENCODE_API_KEY" } },
    { "name": "local",  "role": "veto",
      "meta_llm": { "provider": "embedded", "model": "qwen-local" } }
  ]
}
```

| Role | Authority |
|---|---|
| `primary` | Decides the verdict |
| `veto` | Can withhold agreement from a `PASS`, turning it into `INDETERMINATE`. **Cannot** assert a `PASS` |
| `audit` | Recorded for agreement statistics only; never changes the outcome |

Omitting `graders` gives the single grader using `optimization.meta_llm` — unchanged behavior. Each grader may override the model, so a quorum can pair a strong hosted model with a cheap local one.

**Disagreement is `INDETERMINATE`, never `FAIL`.** Two graders disagreeing is not evidence the artifact is wrong, and treating it as a failure is how noise becomes a rule.

Agreement is reported as raw percentage *and* Cohen's κ, because raw agreement alone is misleading — two graders that always say `PASS` agree 100% and are both useless. A comparison is only treated as trustworthy when κ > 0.4 **and** both labels actually occurred:

```
### Grader agreement

- 1 comparison(s): 100% raw agreement, κ = 1.00 — **at or near chance, or measured on a single label**

⚠️ At least one comparison is not trustworthy. The verdicts behind this patch were not
produced by graders that demonstrably agree, so treat the rules as weaker evidence than usual.
```

### 9. Patch Provenance & Re-verification

A patch is prose rules with no way to tell whether they still apply. If the target's prompt has drifted since the run, re-applying it can reintroduce a fix that is now wrong. Every patch therefore records what it was derived from:

```
**Provenance**

- **Manifest hash:** `sha256:5b753fe4…`
- **Evaluator set hash:** `sha256:938fa742…`
- **Baseline target rules digest:** `sha256:9c7ca576…`
- **Resulting target rules digest:** `sha256:9c7ca576…`
- **Transcript digest:** `sha256:…`          (when the agent emitted one)
- **Run at:** `2026-09-28T02:40:57Z` · **Finished at:** `2026-09-28T02:40:57Z`
```

Check it before trusting it again:

```bash
./neuroplasticity verify-patch neuroplasticity_patch.md plasticity.json
```

```
✅ Rules digest matches. This patch still applies to the current target.
```

```
🛑 DRIFT DETECTED — refusing to verify.
The target's rules have changed since this patch was generated, so a re-run would
describe a different prompt than the one these rules were derived from.
```

Refusing is the point: re-running the evaluators against drifted rules would report a result for a *different* artifact than the patch describes, which is worse than refusing. Drift can be overridden, but only explicitly:

```bash
./neuroplasticity verify-patch neuroplasticity_patch.md plasticity.json --allow-drift
```

A patch predating provenance has nothing to compare against and says so rather than passing silently. Exit code `3` means drift.

### 10. Chained Evaluators & Preventing Regressions
As your agent gets more complex, fixing one bug might introduce another. NeuroPlasticity supports **Chained Evaluators** to prevent regressions. You can define multiple independent tests in your `plasticity.json`. 

The Meta-Optimizer must find a system prompt that satisfies *all* evaluators simultaneously to achieve a `pass_threshold` of 1.0.

```json
"evaluators": [
  {
    "name": "Check JSON Format",
    "script": ["jq", ".", "output.json"],
    "weight": 0.5
  },
  {
    "name": "Check Schema",
    "script": ["jq", "-e", ".status == \"success\"", "output.json"],
    "weight": 1.0
  },
  {
    "name": "Check For Markdown Code Blocks",
    "type": "container",
    "image": "alpine:latest",
    "setup_script": ["apk add --no-cache grep"],
    "command": ["sh", "-c", "grep -q '```' /workspace/output.json && echo 'No markdown code blocks allowed!' >&2 && exit 1 || exit 0"],
    "weight": 0.5
  }
]
```

**Important — `host_bash` runs on your host machine.** Its first argument must be one of `git`, `jq`, `cat`, `ls`, `grep`, or `echo`; anything else is rejected as a sandbox-escape attempt. That includes `bash -c`, which would let a manifest run arbitrary commands on your machine, so shell logic cannot be used there even though it looks more convenient. Write the command as a direct argument list and let the exit code be the signal — the tool's own stderr is captured and handed to the Meta-Optimizer, so failure messages stay informative (e.g. `jq: error: Could not open file ...`). Note that `grep -q pattern file` exits **zero when it finds a match**, so for a "must not contain" check you need the negation that only a shell can express: use a `container` evaluator, which is properly sandboxed.
