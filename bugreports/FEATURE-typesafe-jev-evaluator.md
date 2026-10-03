# FEATURE REQUEST: TypeSafe (System One / Jev) as a native evaluator provider

**Requested:** 2026-10-02
**Requested by:** keywest.health agent (owner-directed)
**Status:** proposal — awaiting implementation scheduling

## Summary

Add **TypeSafe's System One models ("Jev")** as a first-class evaluator provider,
alongside the existing `llm` evaluator (OpenAI-compatible endpoints) and the
embedded llama.cpp grader.

TypeSafe (https://docs.typesafe.ai) is a **judgment model**, not a chat model: it
receives state (a document, transcript, candidate list) plus a typed question and
returns **schema-constrained typed answers with probabilities** — Choice (one of a
defined set), Noul (probability a condition holds), Score (graded position on
ordered levels). It never generates prose. This is exactly the shape of the work
our `llm` evaluators already do, minus everything we don't need (generation,
retry-on-ramble, prompt-format drift).

## Why this fits NeuroPlasticity specifically

1. **The verdict contract is the same object.** Jev returns typed, schema-shaped
   answers — the mapping to `Verdict::Pass / Fail / Indeterminate` is mechanical,
   not a parse-and-pray. The "replied `**PASS** — valid JSON`" failure class
   cannot exist: the answer is typed at the API boundary, never a string to parse.
2. **INDETERMINATE becomes principled.** Jev returns calibrated probabilities and
   a confidence per answer. A `Confidence` below a manifest-configured threshold
   maps to `Verdict::Indeterminate` — which is exactly the doctrine this project
   already encodes ("noisy graders can't write rules", grader disagreement yields
   INDETERMINATE, not FAIL). Today that threshold is a vibe in the grader prompt;
   with Jev it is a measured probability in the response.
3. **It attacks the biggest real cost of running epochs.** keywest.health's
   NeuroPlasticity spend policy (2026-10-02) records ~$42 for one night of
   epochs (~$3-4 per 5-scenario epoch) because grading rides on live
   conversational models. Jev on a free/cheap tier turns the *evaluation layer*
   of an epoch into a near-$0 operation, leaving real spend only on the
   conversations themselves. That is the difference between NeuroPlasticity as a
   "deploy gate" and NeuroPlasticity as an iteration loop you can actually run
   weekly.
4. **Grader quorums get a cheap default.** Jev as `primary` grader with a heavier
   model as `veto` is the natural configuration — and the existing κ (agreement)
   reporting between graders means adopting Jev is *measured by the framework's
   own machinery* from the first run, rather than trusted on faith.

## Proposed manifest shape

Keep it minimal and consistent with `llm` evaluators:

```json
{
  "type": "typesafe",
  "document": "{{run.transcript}}",
  "questions": {
    "passes_rule": {
      "primitive": "noul",
      "question": "Does the transcript above comply with the rule stated below? \"{{rule.text}}\"",
      "criteria": "The full conversation is visible; judge only what is shown."
    }
  },
  "mapping": {
    "passes_rule": { "above": 0.75, "verdict": "PASS", "below": 0.35, "verdict": "FAIL" }
  }
}
```

Semantics:

- Each `questions` entry runs as one Jev judgment (Noul / Choice / Score) over
  the evaluator's `document` state. Independent questions run in parallel
  (TypeSafe composes them in one request where possible — their API batches
  independent judgments).
- `mapping` converts a probability/answer to a verdict. Anything inside the
  dead-band between `above` and `below` (or Confidence under a floor, config
  `indeterminate_below`) yields `Verdict::Indeterminate`. Default dead-band:
  0.35–0.75.
- A Choice question's selected value can also route: `verdict_by_answer` with a
  per-answer verdict map, for graders that judge "which failure class is this?"

## Integration points (all existing)

- **Fingerprinting:** add the evaluator's provider (`typesafe`) + question IDs +
  mapping to the failure fingerprint exactly as `meta_llm.provider/model/base_url`
  are today — same failed-config cache, no new mechanics.
- **Semaphore:** TypeSafe is a cloud HTTP API; treat it like cloud providers
  (concurrency up to the existing 10), not the embedded-1 slot. A `typesafe`
  grader inside a quorum slots into `primary`/`veto`/`audit` unchanged.
- **Fail loud:** HTTP errors / auth failures from the TypeSafe API abort the run
  with a diagnostic, same as an unreachable llm endpoint — an evaluator
  infrastructure failure must never become an agent FAIL.
- **Egress plan:** a `typesafe` evaluator participates in the declared data-class
  egress plan like any other cloud provider (`--print-egress-plan` must show the
  outbound path: document text → TypeSafe endpoint).
- **API keys:** `TYPESAFE_API_KEY` env or keyring; never in the manifest file.
  (keywest.health's vault pattern: `secret-tool lookup service typesafe`.)

## API reference (verified from live docs, 2026-10-02)

- Programming model: https://docs.typesafe.ai/concepts/system-one
- Building guide: https://docs.typesafe.ai/concepts/how-to-build-with-system-one
- Primitives: Choice https://docs.typesafe.ai/primitives/choice · Noul
  https://docs.typesafe.ai/primitives/noul · Score https://docs.typesafe.ai/primitives/score
- Confidence: https://docs.typesafe.ai/confidence
- HTTP API: https://docs.typesafe.ai/api
- Rerank cookbook (batched judgments over one document):
  https://docs.typesafe.ai/cookbooks/rerank_typesafe.md

## Non-goals

- Jev does NOT replace the conversational model inside the container. It cannot
  run the agent — it has no chat surface by design. The container/agent evaluator
  path is untouched.
- No prompt-engineering needed for graders: question meaning lives in the
  question itself (complete, self-contained), not in few-shot prompt craft.

## Suggested acceptance tests

1. A manifest with one `typesafe` Noul grader over a recorded transcript
   reproduces the same PASS/FAIL as the equivalent `llm` grader on clean data.
2. A deliberately ambiguous document (transcript truncated mid-sentence) yields
   `INDETERMINATE` (not FAIL) at the default dead-band, and the run halts per
   existing INDETERMINATE policy.
3. Grader quorum: `typesafe` primary + `llm` veto disagree → run records
   INDETERMINATE with κ reported; agree → PASS with both verdicts in the
   artifact.
4. Failure fingerprint: same manifest + typesafe grader caches a known failure;
   changing only the mapping thresholds invalidates the fingerprint (new config).
5. `--print-egress-plan` lists the TypeSafe endpoint and the document data class.
6. Endpoint unreachable → run aborts with diagnostic; no verdict recorded, no
   rule written.
