# NeuroPlasticity Architecture

NeuroPlasticity is a highly isolated, self-healing testing environment for AI CLI agents. Its architecture is designed to securely sandbox agent execution, evaluate the results concurrently, and automatically mutate the agent's prompt to fix failures in subsequent epochs.

---

## 1. The Meta-Optimization Loop & Fast-Path Cache

At the core of NeuroPlasticity is the evaluation and optimization loop. If an agent fails to accomplish its task, the orchestrator feeds the `stderr` and evaluation results to an embedded Meta-Optimizer LLM. 

To save immense amounts of compute time (and LLM tokens), NeuroPlasticity v1.0.1 introduces the **Deterministic Fingerprint Cache**. Before spinning up a 120-second container, it hashes the entire test configuration. If that exact configuration previously failed, it skips execution and instantly feeds the cached logs back to the optimizer to force a new breakthrough rule.

```mermaid
graph TD
    A[Start Epoch] --> B{Check Fingerprint Cache}
    B -- Known Failure --> C[Load Cached stdout/stderr/score]
    B -- Cache Miss --> D[Provision Ephemeral Workspace]
    D --> E[Execute Target Agent inside Podman]
    E --> F[Run Tri-State Evaluators Concurrently]
    
    F --> G{Score >= Threshold?}
    C --> G
    
    G -- Yes --> H[Success! Generate Improvement Patch]
    
    G -- No --> I[Save Fingerprint to Cache]
    I --> J[Meta-Optimizer LLM analyzes logs + existing rules]
    J --> K[Generate new rules in .neuroplasticity/rules.json]
    K --> |Next Epoch| B

    style H fill:#2e8b57,stroke:#fff,stroke-width:2px,color:#fff
    style J fill:#b22222,stroke:#fff,stroke-width:2px,color:#fff
    style B fill:#d4af37,stroke:#333,stroke-width:2px,color:#000
```

---

## 2. Hybrid Workspace (Zero-Copy Architecture)

Historically, testing agents required deep-copying the entire host repository to prevent accidental corruption. This was prohibitively slow for large codebases. NeuroPlasticity introduces the **Hybrid Workspace**.

Instead of copying files, the host repository is mounted into the container as **Read-Only**. The agent is provided a separate, ephemeral scratch directory mounted as **Read-Write**.

```mermaid
flowchart LR
    subgraph Host Machine
        Repo[Host Project Repo]
        Scratch[Ephemeral /tmp/workspace]
    end

    subgraph Podman Sandbox
        P_mnt["/project (Read-Only)"]
        W_mnt["/workspace (Read-Write)"]
        Agent((AI Agent))
    end

    Repo -- "Safe read access" --> P_mnt
    Scratch -- "Write artifacts here" --> W_mnt
    P_mnt -.-> Agent
    Agent -.-> W_mnt

    classDef host fill:#1e1e1e,stroke:#4a4a4a,color:#fff;
    classDef sandbox fill:#0d47a1,stroke:#4a4a4a,color:#fff;
    
    class Repo,Scratch host;
    class P_mnt,W_mnt,Agent sandbox;
```

**Benefits:**
*   **Absolute Host Safety:** The agent physically cannot delete or corrupt the user's host codebase.
*   **Instant Boot:** Zero file copying means the sandbox boots in milliseconds.

---

## 3. Zero-Config Auth & UID Sandboxing

Agents like `opencode` and `claude-code` require OAuth tokens to communicate with their LLM providers. Instead of forcing users to implement complex headless OAuth flows inside the sandbox, NeuroPlasticity uses **Zero-Config Auth**.

Host configuration directories (like `~/.config/opencode`) are mounted as Read-Only into the container. However, mapping UIDs via Podman's `--userns=keep-id` can cause `EACCES` permission errors when agents try to write to their home directory (e.g., `npm install -g`). 

To solve this, NeuroPlasticity dynamically injects an ephemeral `/user_home` directory.

```mermaid
flowchart TD
    subgraph Host
        Config["~/.config/opencode"]
        EphHome["/tmp/neuro-home-..."]
    end

    subgraph Container
        CHome["/user_home/.config/opencode (RO)"]
        CState["/user_home (RW)"]
        Env["HOME=/user_home<br>NPM_CONFIG_PREFIX=/user_home/.npm-global"]
    end

    Config -- "Pre-created parent dirs" --> CHome
    EphHome -- "Mapped to container user" --> CState
    CState --- Env
```

---

## 4. Zero-Dockerfile JIT Setup & Timeouts

To avoid maintaining dozens of custom Dockerfiles for different agents, NeuroPlasticity uses standard, minimalistic base images (e.g., `node:20-slim` or `python:3.12-slim`). The agent is installed Just-In-Time (JIT) using the `setup_script` array.

To protect host CI resources from hanging, all container executions are wrapped in asynchronous `tokio` timeouts (default 120s) with active `SIGTERM` and `SIGINT` trapping. If an agent (or reasoning LLM) hangs, the orchestrator forcefully kills the orphaned Podman container before exiting.

---

## 5. Tri-State Evaluators (Massively Parallel)

Evaluating the output of an AI agent is notoriously difficult. NeuroPlasticity solves this with **Tri-State Evaluators**.

Every evaluator defined in `plasticity.json` is spawned as an asynchronous `tokio` task, meaning **all tests run concurrently**. Grading an epoch takes only as long as your single slowest test.

```mermaid
flowchart TD
    Eval[Tri-State Evaluators run concurrently via futures::join_all]
    
    Eval -->|type: host_bash| HB[Host Bash]
    HB -.-> HBD[Fast, lightweight POSIX shell commands.<br>Runs directly on the host machine.<br><i>e.g., checking if a file exists.</i>]
    
    Eval -->|type: container| Cont[Isolated Container]
    Cont -.-> ContD[Spins up a dynamic ephemeral container.<br>Mounts workspace as Read-Only.<br><i>e.g., Heavy AST parsers, PyTest, Node.js scripts.</i>]
    
    Eval -->|type: llm| L[Embedded LLM]
    L -.-> LD[Feeds the document to local llama.cpp.<br>Prompt-based qualitative grading (PASS/FAIL).<br><i>e.g., Checking tone, pronouns, structural intent.</i>]
```

### The LLM Semaphore
Because executing 5 LLM evaluators simultaneously using local `llama.cpp` would instantly OOM crash a machine, NeuroPlasticity uses an `Arc<Semaphore>`. If `provider == "embedded"`, LLM concurrency is strictly limited to 1 (queueing safely). Cloud providers (GitHub, OpenAI) scale up to 10 concurrent requests to maximize speed.

---

## 6. Global Model Caching (Offline-First)

When the `embedded-llm` feature is active, NeuroPlasticity runs entirely offline using `llama.cpp`. To respect the user's disk space, it does not blindly download 5GB GGUF models.

Instead, the Rust engine scans universally accepted POSIX model caches across the system before attempting a download (`~/.cache/huggingface/hub/`, `~/.ollama/models/blobs/`, etc.). If a compatible model is found, it is mapped directly into memory.
