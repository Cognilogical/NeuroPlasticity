# RFC: Dynamic Sandbox Building via Dockerfile

## Context & Problem Statement
Currently, NeuroPlasticity relies on pre-built container images defined via the `base_image` key in `plasticity.json` (e.g., `localhost/neuro-agent-testbed:latest`). 

This creates a brittle testing environment:
1. Projects (like Praxis) that require specific runtimes (Node.js) or specific LLM CLI tools (opencode, claude, aider) are forced to mount binaries from the host machine (e.g., mounting `.tmp/opencode`).
2. These mounts frequently fail due to OS architecture mismatches or missing dynamic libraries inside the generic `testbed` container.
3. It makes NeuroPlasticity difficult to share, as users must manually pre-build specific host images before running tests.

## Proposed Solution
Make NeuroPlasticity framework/agent-agnostic by allowing `plasticity.json` to define a custom `Dockerfile` to build dynamically *before* executing the agent evaluation loop.

## Required Changes

### 1. Update the JSON Schema (`schemas/v1/plasticity.schema.json`)
Update the `sandbox` object definition to support an optional `build` configuration.

**Old:**
```json
"sandbox": {
  "type": "object",
  "properties": {
    "engine": { "type": "string" },
    "base_image": { "type": "string" }
  },
  "required": ["engine", "base_image"]
}
```

**New:**
```json
"sandbox": {
  "type": "object",
  "properties": {
    "engine": { "type": "string" },
    "base_image": { "type": "string" },
    "build": {
      "type": "object",
      "properties": {
        "dockerfile": { "type": "string", "description": "Path to Dockerfile relative to project root" },
        "context": { "type": "string", "description": "Build context path, usually '.'" }
      },
      "required": ["dockerfile", "context"]
    }
  },
  "required": ["engine"]
  // Note: Either base_image or build should be provided.
}
```

### 2. Update Rust Models (`src/models.rs` or equivalent)
Modify the `SandboxConfig` struct to deserialize the new `build` field.

```rust
#[derive(Debug, Deserialize, Serialize)]
pub struct SandboxBuild {
    pub dockerfile: String,
    pub context: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SandboxConfig {
    pub engine: String, // "podman" or "docker"
    pub base_image: Option<String>,
    pub build: Option<SandboxBuild>,
}
```

### 3. Update the Execution Engine (`src/executor.rs` or equivalent)
Before spawning the agent container, the executor must check if `build` is defined.

**Build Phase Logic:**
1. If `sandbox.build` is present, execute a synchronous shell command using `std::process::Command`:
   ```rust
   // Pseudo-code
   let tag = format!("neuroplasticity-test-{}", project_name);
   let status = Command::new(&sandbox.engine)
       .args(["build", "-t", &tag, "-f", &sandbox.build.dockerfile, &sandbox.build.context])
       .status()?;
   
   if !status.success() {
       return Err("Failed to build dynamic sandbox image");
   }
   ```

2. **Run Phase Logic:**
   When starting the test container, use the dynamically generated image tag (e.g., `neuroplasticity-test-<project_name>`) instead of `base_image`.

### 4. Cleanup (Optional but Recommended)
After the optimization loop (all epochs) finishes, consider running `{engine} rmi {tag}` to clean up the dynamically built image and save disk space, or leave it cached for faster subsequent runs (which `podman/docker build` handles naturally via caching).

## Success Criteria
An agent should be able to run `cargo run` in NeuroPlasticity against a `plasticity.json` that looks like this:

```json
{
  "name": "praxis-generator-validation",
  "agent_command": ["opencode", "run", "/praxis", "https://linkedin.com/job"],
  "sandbox": {
    "engine": "podman",
    "build": {
      "dockerfile": "tests/Dockerfile.agent",
      "context": "."
    }
  }
}
```
And NeuroPlasticity should automatically build the image using `tests/Dockerfile.agent` and execute the test inside it.
