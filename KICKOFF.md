# NeuroPlasticity

## Self-Reinforced Testing & Prompt Optimization Framework

### 1. Vision & Abstract
**NeuroPlasticity** is a language-agnostic, configuration-driven orchestration engine for AI agents—think of it as "DSPy for Agent Workflows." 

As AI agents become more autonomous, they increasingly fail not because of intelligence, but because of rigid, brittle, or ambiguous system prompts that do not survive complex, multi-step environments. 

NeuroPlasticity solves this by treating the agent's instructions (prompts) as **hyperparameters that can be optimized by an LLM**. It drops an agent into an ephemeral, isolated sandbox, asks it to perform a task, and then runs pluggable evaluation scripts to grade its side-effects (e.g., Did it write the file? Did the tests pass? Did it update the database?). If the agent fails, an LLM reviews the logs and rewrites the agent's instructions to ensure it doesn't make the same mistake twice.

---

### 2. Core Concepts
NeuroPlasticity is built on three foundational pillars:

#### A. The Ephemeral Sandbox (The "Crucible")
Agents cannot be safely evaluated on the host machine. NeuroPlasticity uses **Podman/Docker** to spin up an isolated, disposable container for every evaluation epoch. The agent executes its task within this sandbox, preventing accidental system modifications while allowing us to precisely measure its side-effects.

#### B. Pluggable Evaluators (The Reward Function)
Instead of relying on LLMs to grade other LLMs (which is prone to hallucination), NeuroPlasticity grades the **physical artifacts** the agent leaves behind. Projects define arbitrary scripts (Bash, Python, Node) that return exit codes.
* `npm run test` (Did the code compile and pass?)
* `check_db.sh` (Did the agent successfully insert a record into LanceDB?)
* `verify_synaptic_graph.sh` (Did the agent generate the required visual graph via SynapticGraph?)

#### C. The Meta-Optimizer (LLM-as-an-Optimizer)
If an agent scores 1.5/3.0, a "Meta-LLM" (e.g., GPT-4o or Claude 3.5) is fed:
1. The agent's previous instructions.
2. The agent's stdout/stderr execution logs.
3. The specific evaluators that failed.

The Meta-LLM is instructed: *"The agent failed the 'Memory Extraction' test because it forgot to invoke the `neurostrata_add_memory` tool. Rewrite its system prompt to explicitly enforce this behavior before it closes a task."* The framework then overwrites the prompt and loops again.

---

### 3. Origin Story & Lessons Learned (The SRTF Prototype)
NeuroPlasticity was born out of the necessity to stabilize the **NeuroStrata** architecture. Our initial Self-Reinforced Testing Framework (SRTF) was a hardcoded Python script (`train_srtf.py`) that successfully stabilized an agent interacting with a Rust-based MCP server (`neurostrata-mcp`). 

During that prototype, we learned critical lessons that inform NeuroPlasticity's design:

1. **Sandbox Permission Boundaries are Fragile:** 
   * *Problem:* When an agent inside a container tries to write files to a host-mounted directory, it causes `EACCES` errors if the UIDs do not match.
   * *Solution:* NeuroPlasticity must natively support strict UID mapping (e.g., `--userns=keep-id` or explicit `useradd -u 1000`) when orchestrating containers, ensuring seamless host-container file sharing.
2. **Native Dependencies Matter for MCPs:** 
   * *Problem:* The agent failed to use a tool because the underlying MCP server silently crashed inside the minimal Ubuntu container due to missing `ca-certificates` (required to download ONNX embedding models over TLS).
   * *Solution:* The sandbox base image must be robust, or NeuroPlasticity must allow projects to define a custom `Containerfile` to guarantee the environment matches production.
3. **Headless Execution & Auto-Approval:** 
   * *Problem:* Agents built for interactive CLI usage (like `opencode`) will hang indefinitely in a CI/CD sandbox when asking for user confirmation to run a tool or edit a file.
   * *Solution:* NeuroPlasticity must explicitly inject flags like `--dangerously-skip-permissions` or configure agents to run in fully headless, non-interactive modes during evaluation.
4. **Prompt Truncation vs. Injection:** 
   * *Problem:* Completely replacing an agent's `SKILL.md` or system prompt with a newly generated one often destroys its foundational tool context (e.g., forgetting how an MCP server works).
   * *Solution:* The Meta-Optimizer should use **Targeted Prompt Mutation**. NeuroPlasticity will support an `injection_marker` (e.g., `## 🚨 DYNAMIC RULES`), allowing the LLM to append its optimized constraints safely at the bottom of the file without destroying the core definitions above it.

### 4. Naming Conventions & Organization
The suite follows a strict naming convention to distinguish between top-level orchestration systems and the internal utility engines that power them.

*   **`Neuro-` (Top-Level Projects):** Reserved for flagship architectures and orchestrators. 
    *   *NeuroStrata*: The 3-Tier Memory Architecture.
    *   *NeuroPlasticity*: The prompt optimization and testing framework.
*   **`Synaptic-` (Internal Engines/Tools):** Reserved for the underlying utilities and processing tools that the Neuro systems execute.
    *   *SynapticGraph*: The visual AST parser and Markdown-to-Obsidian Canvas mapper (formerly Graphify/ast-parser).

---

### 5. Proposed Architecture & Implementation

#### Language Choice: Rust (or Go)
To distribute NeuroPlasticity as a universal CLI tool, it should be compiled to a single, static binary. Rust is highly recommended because it avoids the friction of Python virtual environments and aligns with the existing Rust footprint of the Neuro ecosystem (e.g., `neurostrata-mcp`).

#### Configuration-Driven Design (`plasticity.yaml`)
Projects simply drop a manifest file into their repository to begin training their agents:

```yaml
name: "neurostrata-memory-eval"
agent_command: "opencode run --dangerously-skip-permissions"
task_prompt: "Refactor routing and document the architecture. Follow memory constraints."

sandbox:
  engine: "podman"
  base_image: "ubuntu:24.04"
  setup_script: "./tests/setup_env.sh"
  testbed_repo: "https://github.com/expressjs/express.git"
  mounts:
    - source: "./target/release/neurostrata-mcp"
      target: "/usr/local/bin/neurostrata-mcp"

optimization:
  target_file: ".agents/skills/neurostrata/SKILL.md"
  injection_marker: "## 🚨 DYNAMIC RULES"
  epochs: 5
  meta_llm: "claude-3-5-sonnet"

evaluators:
  - name: "Recall (Database Created)"
    script: "./tests/eval/check_db.sh"
    weight: 1.0
  - name: "Synthesis (Canvas Generated)"
    script: "./tests/eval/check_canvas.sh"
    weight: 1.0
```

#### The Execution Loop
1. **Parse:** Read `plasticity.yaml`.
2. **Build:** Construct the ephemeral container image.
3. **Execute:** Run the agent command inside the container and capture `stdout`/`stderr`.
4. **Evaluate:** Run the evaluator scripts on the mutated workspace and calculate the Composite Score.
5. **Optimize:** If the score is not perfect, send the logs, previous rules, and failed evaluators to the Meta-LLM.
6. **Mutate:** Inject the newly generated rules into the `target_file`.
7. **Repeat:** Loop until the max epochs are reached or a perfect score is achieved.

---

### 6. Roadmap / Next Steps
1. **Initialize the Repository:** Create a new Rust project (`cargo new neuroplasticity --bin`).
2. **Build the Parser:** Implement `serde` to parse the `plasticity.yaml` manifest.
3. **Container Orchestrator:** Implement a robust wrapper around `std::process::Command` to manage Podman/Docker lifecycles, volume mounts, and automated cleanup.
4. **Evaluation Engine:** Build the logic to sequentially execute evaluation scripts and aggregate their exit codes into a composite reward float.
5. **LLM Integration:** Integrate `reqwest` to communicate with standard OpenAI/Anthropic APIs for the Meta-Optimizer prompt generation.
6. **CLI Polish:** Add rich terminal output (using tools like `indicatif` or `ratatui` in Rust) to visualize the epochs, scores, and prompt mutations in real-time.