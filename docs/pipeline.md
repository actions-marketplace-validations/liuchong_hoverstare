# Review pipeline

The review pipeline runs the same diff in parallel review lanes, clusters the findings by location and title, and accepts only findings that receive at least two votes or pass a verifier check. This reduces false positives from the single-pass backend while keeping the same interface.

> Implementation: [`src/pipeline.rs`](../src/pipeline.rs)  
> Full specification: [`specs/05-review-pipeline.md`](../specs/05-review-pipeline.md)  
> Underlying single-pass backend: [`specs/04-agent-backend.md`](../specs/04-agent-backend.md)

The review pipeline is a false-positive reduction layer that sits on top of the single-pass agent backend. It runs the same diff through several parallel review lanes, each with a different focus, then clusters equivalent findings and votes on them.

```
diff + context
    │
    ├─► lane 1: correctness (temp 0.2)
    ├─► lane 2: concurrency & resources (temp 0.4)
    └─► lane 3: security & boundaries (temp 0.6)
         │
    join_all (parallel)
         │
         ▼
   cluster findings
         │
         ▼
       vote
    ├─ >=2 votes ──► accepted
    └─ 1 vote ──────► verifier ──► accepted / dropped
```

## Lanes

Each lane is a parallel call to the single-pass backend. They share the same system-prompt skeleton; only the appended `[FOCUS OF THIS PASS]` paragraph and the temperature differ.

| Pass | Focus | Temperature |
|---|---|---|
| 1 | Correctness: logic errors, null dereferences, off-by-one errors, missing error handling | 0.2 |
| 2 | Concurrency & resources: races, deadlocks, leaks, lifetimes | 0.4 |
| 3 | Security & boundaries: injection, privilege escalation, input validation, integer overflow | 0.6 |

Lanes run concurrently with `join_all`. A single lane failure does not stop the others, but if every lane fails the whole run fails.

## Degradation rules

Not every diff goes through the full pipeline.

- **Small diff**: if the diff has fewer than 50 added lines, the pipeline is forced to `passes = 1` + `verify = true`. Small changes are not worth the multi-pass cost.
- **Straight-through**: if `passes = 1` and `verify = false`, the pipeline short-circuits to `analyze_with_backend()` and behaves exactly like the single-pass backend with no voting or verifier.
- Otherwise, the configured `passes` value is clamped to the number of available lenses (currently 3).

## Clustering

Findings from all lanes are grouped into clusters of the “same problem” using a greedy algorithm. Two findings are placed in the same cluster when:

- they are in the **same file**;
- their line numbers are within **3 lines** of each other;
- their titles have a Jaccard similarity of **≥ 0.5**.

**Title tokenization** normalizes titles before comparison:

- lowercased;
- punctuation stripped;
- English and Chinese stopwords removed;
- ASCII text split into words;
- CJK text tokenized as single characters plus bigrams (so Chinese titles still get measurable overlap).

Each cluster is merged into one representative finding:

- **severity** = the highest severity among cluster members;
- **description** = the longest description among cluster members;
- **additional_locations** = the deduplicated union of all member locations.

## Voting

A cluster’s vote count is the number of distinct passes that contributed a finding to it. Repeats from the same pass count only once.

- **≥ 2 votes**: the representative finding is accepted directly.
- **1 vote**: the representative finding is a single-vote finding.
  - If `verify = false`, it is dropped.
  - If `verify = true`, it is sent to the verifier.

## Verifier

The verifier re-checks each single-vote finding with a separate, tool-backed review call. It receives the finding JSON and the relevant diff section and must decide whether it is a real, triggerable defect.

The verifier must return exactly one JSON object of this shape:

```json
{ "verdict": "confirmed", "confidence": 0.6, "reason": "..." }
```

Only `verdict == "confirmed"` with `confidence >= 0.6` is accepted. Anything else — `rejected`, a parse failure, a timeout, or a backend error — is treated as rejected and the finding is dropped.

Verifier constraints:

- temperature forced to `0.0`;
- budget = `max_tool_calls / 2`;
- timeout = 120 s.

## Output stats

`PipelineStats` records what happened during the run:

| Field | Meaning |
|---|---|
| `passes_run` | Number of lanes that completed successfully. |
| `pass_findings` | Findings count per lane, in lane order. |
| `clusters` | Number of clusters formed after merging. |
| `voted_in` | Findings accepted directly by ≥ 2 votes. |
| `verified_in` | Single-vote findings accepted by the verifier. |
| `dropped` | Single-vote findings discarded (verify off or verifier rejected). |

The final summary text is taken from the lane that produced the most findings.

## See also

- [`src/pipeline.rs`](../src/pipeline.rs) — the implementation.
- [`specs/05-review-pipeline.md`](../specs/05-review-pipeline.md) — the full specification.
- [`specs/04-agent-backend.md`](../specs/04-agent-backend.md) — the single-pass backend underneath.
- [`docs/web-ide.md`](web-ide.md) — develop mode, which reuses the same agent loop
  and tool sandbox with a write-capable tool profile instead of the review lanes.
