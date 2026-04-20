# ARC-7 Subagent Execution Failure: `ProviderModelNotFoundError`

## Issue Summary
When attempting to convene the ARC-7 architectural review panel via the `Task` tool, the system fails to spawn the specialized subagents (e.g., `arc7-architect`) and returns a `ProviderModelNotFoundError`.

## Steps to Reproduce
1. Load the ARC-7 skill.
2. Attempt to invoke the `Task` tool to spawn a panel member:
   ```json
   {
     "command": "/ARC-7",
     "description": "ARC-7 Architect Review",
     "prompt": "Read your persona file...",
     "subagent_type": "arc7-architect"
   }
   ```
3. The tool execution fails immediately with the output: `ProviderModelNotFoundError`.

## Root Cause Analysis
The ARC-7 skill definition mandates specific models for specific personas (e.g., *The Architect* requires Claude Sonnet 4.6 or its fallbacks). The current orchestration environment/backend running this agent does not have the requested model provider configured, mapped, or authenticated. Because the system cannot resolve the required model for the `arc7-architect` subagent type, it throws the routing error.

## Recommended Fix for the ARC-7 Maintainer
1. **Model Routing Configuration:** Verify the backend provider configuration (e.g., LiteLLM, OpenAI/Anthropic API keys, or custom routing layer) to ensure that the model IDs requested by the `arc7-*` subagent profiles exist and are properly mapped.
2. **Fallback Mechanism:** Ensure that if the primary model (e.g., Claude Sonnet 4.6) is unavailable, the fallback logic within the agent initialization pipeline gracefully degrades to an available model (like Gemini 3.1 Pro or Claude 3.5 Sonnet) rather than throwing a hard `ProviderModelNotFoundError`.
3. **Subagent Type Registration:** Confirm that `arc7-architect` (and the other 5 panel roles) are properly registered in the environment's available agent types roster with valid model bindings.