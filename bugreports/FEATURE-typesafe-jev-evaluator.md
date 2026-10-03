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
   conversational models. Making the *evaluation layer* cheap is what turns
   NeuroPlasticity from a "deploy gate" into an iteration loop you can run
   weekly. **Cost caveat (stated honestly):** TypeSafe's published docs carry NO
   pricing page — the only quantified public datapoint is the batching cookbook
   (https://docs.typesafe.ai/cookbooks/parallel_questions.md): 13 batched
   questions in ONE call measured **12.2× cheaper and 10.0× faster than
   separate calls, with no change in answers**. The owner reports Jev is
   available on a free tier (unverified in docs). **Gate before scheduling:**
   confirm actual per-judgment cost from the account dashboard or a billed
   pilot; the headline benefit is aspirational until that number exists.
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
    "pass_above": 0.75,
    "fail_below": 0.35
  },
  "indeterminate_below": 0.5
}
```

Semantics:

- Each `questions` entry runs as one Jev judgment (Noul / Choice / Score) over
  the evaluator's `document` state. Independent questions run in parallel
  (TypeSafe composes them in one request where possible — their API batches
  independent judgments).
- For a Noul: probability ≥ `pass_above` → `Verdict::Pass`; ≤ `fail_below` →
  `Verdict::Fail`; anything in the dead-band between the two thresholds →
  `Verdict::Indeterminate`. **Noul answers carry NO separate confidence field**
  (verified from https://docs.typesafe.ai/confidence: "Noul answers don't carry
  one" — the probability itself is the uncertainty; p near 0.5 IS unsure). So
  for Noul, `indeterminate_below` does not apply: the dead-band IS the
  uncertainty signal. (If a single confidence-scale gate across all primitives
  is wanted anyway, the docs' own suggestion is the derived
  `|2p − 1|` — same scale as Choice confidence.)
- For Choice and Score answers, which DO carry a `confidence` property
  (a 0–1 collapse of the probability-distribution shape, exact formulas in the
  docs): Confidence under `indeterminate_below` forces `Indeterminate` even if
  the probability band alone would pass — distribution shape catches cases the
  point-probability misses (e.g. a Score torn between adjacent levels).
  Defaults if omitted: `pass_above` 0.75, `fail_below` 0.35,
  `indeterminate_below` 0.5.
- A Choice question's selected value can also route: `verdict_by_answer` with a
  per-answer verdict map, for graders that judge "which failure class is this?"
- Validation: `pass_above > fail_below` is required (overlapping or inverted
  bands must fail manifest parse, not grade epochs).

## Implementation shape (explicit): a new `EvaluatorType`, NOT a provider branch

**This must not be implemented as a `provider == "typesafe"` branch inside
`llm_client.rs::complete_tracked`.** The `llm` evaluator's plumbing is
chat-shaped (`CompletionSpec`, prose completion, verdict parsed from generated
text) — forcing judgment questions through it would reconstruct the exact
parse-failure class this feature exists to eliminate, and would lose batched
typed judgments. The correct shape is a **new evaluator type** with its own
request builder (TypeSafe's System One request payload) and its own response
decoding (typed answers, not parsed prose). Shared concerns — fingerprinting,
semaphore, quorum roles, egress plan, fail-loud aborts — are reused; the
completion path is not.

## Integration points (all existing)

- **Fingerprinting:** add the evaluator's provider (`typesafe`), the **full
  serialized question text** (including interpolated rule text and criteria —
  rewording a question changes the grader as much as changing thresholds, so it
  must invalidate the failed-config cache), and the **mapping thresholds** to the
  failure fingerprint exactly as `meta_llm.provider/model/base_url` are today —
  same failed-config cache, no new mechanics.
- **Semaphore:** TypeSafe is a cloud HTTP API; treat it like cloud providers
  (concurrency up to the existing 10), not the embedded-1 slot. A `typesafe`
  grader inside a quorum slots into `primary`/`veto`/`audit` unchanged.
- **Fail loud:** HTTP errors / auth failures from the TypeSafe API abort the run
  with a diagnostic, same as an unreachable llm endpoint — an evaluator
  infrastructure failure must never become an agent FAIL.
- **Record/replay for tests (REQUIRED, not optional).** The framework is
  paranoid about determinism and cost; acceptance tests that call the live
  TypeSafe API would be neither. Tests 1-4 MUST run against **recorded-response
  fixtures** (cassettes): first contact with the live API records the full
  request/response (typed answers, probabilities, confidence) to a fixture
  file; subsequent runs replay it. A fixture/dry-run mode keeps `cargo test`
  free and reproducible, and a changed question/mapping that invalidates the
  fingerprint also invalidates the cassette (same rule as the failure
  fingerprint cache). Live-API runs are the deliberate-spend exception, exactly
  like lane runs.
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

**Prerequisite (likely the largest work item — larger than the code): build the
boundary corpus.** The repo has NO recorded transcripts with known-marginal
labels today. Test 3's evidence value depends entirely on this corpus:
transcripts that sit NEAR the boundary — marginal passes, marginal fails,
truncations, half-satisfied rules — each with a human-confirmed label
(PASS/FAIL/INDETERMINATE). Budget it as a first-class task: curate from existing
`.neuroplasticity/runs/` artifacts, hand-label, and version the labels. A
boundary corpus without confirmed labels would make the κ number decorative.

**All tests run against recorded-response fixtures (cassettes), never the live
API** — free and reproducible per the record/replay requirement above.

1. **Smoke, clean data:** a manifest with one `typesafe` Noul grader over a
   recorded transcript produces a well-formed verdict end-to-end (proves wiring
   only — agreement on clean data is nearly free for any grader and is NOT the
   quality claim; quality lives in test 3).
2. **Boundary/ambiguity is the real test:** boundary-corpus items that are
   deliberately ambiguous (transcript truncated mid-sentence, or a rule the
   transcript only half satisfies) yield `INDETERMINATE` (not FAIL): for Noul,
   via the probability dead-band; for Choice/Score, also via
   Confidence < `indeterminate_below`. The run halts per existing INDETERMINATE
   policy.
3. **Grader quorum + κ is the informative agreement test:** `typesafe` primary +
   `llm` veto graded over the **boundary corpus** (marginal passes and marginal
   fails — NOT obvious cases, which both graders pass for free and which prove
   nothing), with raw agreement % and Cohen's κ reported per epoch. A
   disagreement yields INDETERMINATE with both verdicts in the artifact. The κ
   on the boundary band is the evidence that Jev grades like the incumbent
   grader where it matters.
4. **Failure fingerprint:** same manifest + typesafe grader caches a known
   failure; changing ONLY the question wording invalidates the fingerprint
   (question text is fingerprint material); changing ONLY the mapping
   thresholds also invalidates it (new grader configuration).
5. `--print-egress-plan` lists the TypeSafe endpoint and the document data class.
6. Endpoint unreachable → run aborts with diagnostic; no verdict recorded, no
   rule written.
