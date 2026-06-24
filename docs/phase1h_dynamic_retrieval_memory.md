# Phase 1H Dynamic Retrieval And Memory Learning

Phase 1H makes retrieval and learning explicit executor steps instead of relying on the model to remember to read or search.

## Implemented

- `RetrievalOrchestrator` produces bounded retrieval guidance from:
  - the current rolling `PlanStep`
  - latest `FailureAnalysis`
  - changed files
  - `TaskContract`
  - `ProjectIndex` impact and test-impact results
- `Planner` records dynamic retrieval after:
  - verification failures with suspect files
  - diff observations with changed files
- Retrieval output is stored in `EvidenceLedger.retrieval_results` and rendered into planner context as `dynamic_retrieval`.
- `LessonExtractor` only forwards final reports to memory when the final report is `completed` and verification is ready.
- Production graph wiring attaches the shared `ProjectIndex` and `MemoryLearningPipeline` to `Planner`.

## Retrieval Output

The retrieval result is a structured ledger item:

```text
trigger
current_step_id
files_to_read
index_queries
memory_queries
changed_files
project_index.impact
project_index.test_impact
evidence_sources
trust_level
```

`files_to_read` is guidance. It does not mark files as inspected. A model or tool must still read the files before they count as inspected evidence.

## Memory Learning Boundary

`LessonExtractor` does not write model guesses to durable memory. It delegates extraction and policy checks to `MemoryLearningPipeline`, and it only calls memory after:

```text
FinalReport.status == completed
verification_summary.status in ready / ready_with_warnings / passed / completed
```

Existing `MemoryPolicy` still handles stable evidence, redaction, quarantine, and manual acceptance rules.

## Boundaries

- No Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future work.
- No new dependency.
- No new planner state machine phase.
- Retrieval results are component-generated and traceable, but still treated as untrusted context until files are actually read.
