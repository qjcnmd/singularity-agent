# Verification Runtime

Miniharness v0.0.8 adds a Verification Runtime so validation is not reduced to a hard-coded `run_tests` command. A production coding harness needs to know what changed, how the project is structured, which checks are available, which checks are risky, what a failure means, what the next repair observation should say, and whether the task is ready to hand back.

The compact boundary is:

```txt
ToolRuntime
  -> registered verification tool
  -> VerificationRuntime
  -> ProjectDetector + CommandDiscovery
  -> ImpactAnalyzer
  -> VerificationPlan
  -> VerificationPolicy
  -> CommandRuntime
  -> FailureParser + RepairHintGenerator
  -> CompletionAssessor
  -> trace + context observation
```

## Runtime Boundaries

`ToolRuntime` owns schema validation, tool policy, timeout handling, and compact `ToolResult` wrapping. Verification tools that execute checks declare `uses_command_runtime=true`.

`VerificationRuntime` owns project detection, command discovery, impact analysis, plan generation, verification policy, result interpretation, evidence shaping, repair hints, flaky handling, repair budget accounting, and completion assessment.

`CommandRuntime` owns process execution. VerificationRuntime never calls `subprocess` directly. Every executable check is a `CommandRequest`, and every run returns a `CommandResult`.

`MutationRuntime` owns agent-authored file edits. VerificationRuntime receives changed file, transaction, and changeset context, but test/formatter/build side effects are recorded as command side effects, not mutation transactions.

`ContextManager` stores verification tool observations like any other tool result. The model-facing message receives only a bounded summary: plan status, check statuses, failed checks, parsed failures, repair hints, and completion assessment. Full command output stays in command artifacts and trace.

`GitRuntime` is still reserved. Verification records transaction/change-set identifiers and uses command side-effect reporting, but does not stage, commit, reset, clean, or push.

## Project Detection And Command Discovery

`ProjectDetector` scans workspace configuration files including:

```txt
package.json, pnpm-lock.yaml, yarn.lock, package-lock.json,
pyproject.toml, requirements.txt, setup.py, Cargo.toml, go.mod,
pom.xml, build.gradle, Makefile, justfile, tox.ini, pytest.ini,
tsconfig.json, ruff.toml, .eslintrc, eslint.config.js, .github/workflows
```

It produces `ProjectProfile`:

```txt
language, languages, package_manager, framework, test_frameworks,
lint_tools, typecheck_tools, build_tools, workspace_kind,
available_commands, evidence_files
```

`CommandDiscovery` extracts commands from `package.json` scripts, Python configuration, Make/just targets, Cargo, Go, Maven, and Gradle. Discovered commands include a `CommandRequest`, source file, confidence, kind, and description. Commands are not hard-coded to one test runner.

## Impact Analysis And Plans

`ImpactAnalyzer` turns changed files and task intent into:

```txt
changed_files, affected_modules, likely_tests, requires_full_test,
requires_build, requires_typecheck, requires_manual_review,
risk_reasons, risk_level, transaction_id, changeset_id
```

Docs-only changes produce a lightweight strategy with skipped tests and a manual-review check. Source, tests, project config, dependency locks, CI workflows, deployment config, and public package surfaces increase the required checks and risk level.

`VerificationPlan` separates:

```txt
required_checks
optional_checks
skipped_checks
blocked_checks
```

Each `VerificationCheck` carries:

```txt
id, kind, command, scope, required, timeout, risk_tags,
failure_policy, policy_decision, policy_reasons, skip_reason
```

## Policy And Execution

`VerificationPolicy` returns:

```txt
allow | require_review | deny | blocked
```

It applies verification-specific rules before execution and still delegates normal command risk classification to `CommandPolicy`.

High-risk checks are not silently treated as low-risk. Package manager install/sync, container commands, database migrations, shell commands, and networked integration verification require review or are blocked by command policy.

When a check is allowed, VerificationRuntime calls:

```txt
CommandRuntime.run(CommandRequest, transaction_id=...)
```

The verification result stores the `verification_check_id -> command_id` relationship in evidence and trace. Direct `run_command` tool usage for verification-like commands returns `verification_runtime_required`.

## Results, Evidence, And Repair

`VerificationResult` records:

```txt
check_id, kind, status, failure_type, evidence,
repair_hints, confidence_impact, duration_ms, attempts
```

Statuses are:

```txt
passed, failed, skipped, blocked, flaky, timeout, inconclusive
```

`VerificationEvidence` records:

```txt
command_id, command, exit_code, output_excerpt, artifact_path,
parsed_failures, duration_ms, timestamp
```

Only bounded excerpts enter observations. Full stdout/stderr remains owned by CommandRuntime artifacts.

Failure parsers currently cover:

```txt
Python traceback
pytest failures
TypeScript tsc errors
ESLint errors
npm build errors
generic stderr/stdout fallback
```

`RepairHintGenerator` converts parsed failures into targeted observations with likely file, line, failing test, and next inspection target. The repair loop does not edit files itself; edits still go through Workspace Mutation Runtime.

`RepairLoopController` tracks:

```txt
max_iterations
max_total_commands
max_total_time_seconds
max_same_failure_retries
stop_on_new_high_risk_change
```

Repeated identical failures, command/time budget exhaustion, and high-risk expansion can block further self-repair.

## Flaky Checks And Completion

Unit and integration checks can use `rerun_on_flaky`. If a failed check passes on rerun or otherwise changes status, the final result is `flaky`, not `passed`.

`CompletionAssessor` returns:

```txt
ready
ready_with_warnings
blocked
failed
needs_review
```

It includes:

```txt
confidence, passed_checks, failed_checks, skipped_checks,
warnings, remaining_risks
```

The agent should only claim completion when the latest completion assessment is `ready` or `ready_with_warnings`, and must report warnings and remaining risks.

## Trace Audit Fields

Verification trace events use event name `verification` and include:

```txt
verification_plan_id
verification_check_id
transaction_id
changeset_id
project_profile
impact_analysis
command_id
policy_decision
check_kind
status
failure_type
parsed_failures
evidence_artifact
duration_ms
confidence_impact
repair_hints
completion_assessment
```

Plan creation, check results, and final assessment are each recorded as structured audit events.

## Current Support And Extension

Current project support includes Python/pytest/ruff/mypy/build, Node/TypeScript package scripts, Cargo, Go, Maven, Gradle, Make, and just targets.

To extend support:

1. Add project markers or dependency inference in `ProjectDetector`.
2. Add command extraction in `CommandDiscovery`.
3. Add or refine risk rules in `VerificationPolicy`.
4. Add a `FailureParser` for the tool's output shape.
5. Add focused tests for detection, planning, parsing, and result classification.
