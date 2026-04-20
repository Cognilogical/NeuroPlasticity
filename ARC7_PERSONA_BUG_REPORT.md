# ARC-7 Subagent Execution Failure: Persona File Missing

## Issue Summary
When spawning the ARC-7 subagents (e.g., `arc7-architect`, `arc7-security-sentinel`) via the `Task` tool, the agents successfully initialize but immediately fail their task and return an `AGENT ERROR`.

## Steps to Reproduce
1. Load the ARC-7 skill.
2. Attempt to invoke the `Task` tool to spawn a panel member:
   ```json
   {
     "command": "/ARC-7",
     "description": "ARC-7 Architect Review",
     "prompt": "Read your persona file at /home/kenton/.config/opencode/skills/ARC-7/agents/arc7-architect.md. Follow all instructions in it for your role, focus areas, output format, and rules. If you cannot read the file, STOP and report: \"AGENT ERROR: Could not load persona file at /home/kenton/.config/opencode/skills/ARC-7/agents/arc7-architect.md.\"\n\nReview the following architectural proposal/context...",
     "subagent_type": "arc7-architect"
   }
   ```

## Exact Error Output from Agent
The subagents return the following string as their `task_result`:

**From arc7-architect:**
```
AGENT ERROR: Could not load persona file at /home/kenton/.config/opencode/skills/ARC-7/agents/arc7-architect.md.
```

**From arc7-security-sentinel:**
```
AGENT ERROR: Could not load persona file at /home/kenton/.config/opencode/skills/ARC-7/agents/arc7-security-sentinel.md. Please ensure the file exists or provide alternative instructions.
```

## Root Cause Analysis
The prompt injected by the orchestrator commands the subagent to read its persona definition file (e.g., `/home/kenton/.config/opencode/skills/ARC-7/agents/arc7-architect.md`). Because the file does not exist at that specific absolute path on the host filesystem, the subagent hits the guardrail instruction and immediately aborts the review.

## Recommended Fix for the ARC-7 Maintainer
1. **Verify Persona File Paths:** The orchestrator agent relies on the paths listed in the **Panel Roster** section of the `SKILL.md` file (e.g., `agents/ARC-7/arc7-context-master.md`). Ensure these paths are correct relative to the skill's base directory.
2. **Update the Skill Prompt Generation:** If the persona files have been moved, renamed, or bundled differently, the `Task` prompt template in `SKILL.md` (Step 4) needs to be updated to reflect the true absolute paths of the persona files on the user's filesystem.