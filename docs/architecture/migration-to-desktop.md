# Migration To Desktop

The target architecture is Rust Core + Tauri Desktop + TypeScript UI + Python Plugin Management. This document defines the staged path without starting the Rust or Tauri implementation in v0.1.x.

## Non-Goals For This Phase

- no Tauri implementation
- no Electron
- no Rust rewrite
- no deletion of the existing Python component
- no remote approval server
- no background remote memory sync daemon
- no Git push, pull request, or branch automation

## Phase 0: Documentation Component

Status: this phase.

Deliverables:

- component map
- boundary contracts
- state model
- event model
- tool protocol contract
- policy/approval contract
- trace/audit contract
- schemas
- ADRs
- drift tests

Exit criteria:

- README and component-map agree on component names
- required docs/ADRs/schemas exist
- README names Singularity as the active project identity
- current tests remain green

## Phase 1: Desktop Transition AgentHost

Goal: make the existing Python component hostable without changing its behavior.

Deliverables:

- `AgentHost` facade over `KernelBootstrap` and `AgentKernel`
- start/resume/cancel run API
- submit approval API
- state snapshot API
- artifact read-by-ref API
- run-event stream adapter
- daemon-safe lifecycle tests

Rules:

- CLI becomes one client of AgentHost
- no UI code owns component objects
- Python remains the production execution path

## Phase 2: Local Daemon

Goal: isolate long-running session state from CLI/UI process lifetime.

Deliverables:

- local IPC boundary
- daemon health command
- single-writer workspace lock integration
- reconnect/replay from event sequence
- local-only auth boundary if needed
- crash recovery visible to clients

Rules:

- daemon stores local state only
- approval remains local
- event payloads stay redacted

## Phase 3: Tauri Desktop And TypeScript UI

Goal: ship desktop as a client over AgentHost/daemon.

Deliverables:

- run list and run detail
- live event timeline
- approval prompts
- tool call/protocol view
- trace/artifact viewer
- workspace state/recovery view
- final report view

Rules:

- Tauri invokes AgentHost/daemon commands, not Python internals
- TypeScript UI subscribes to run events and snapshots
- UI never edits trace, context, protocol, policy, or workspace-state files directly
- desktop clients use `singularity-agent` or `sg` commands for installed CLI interop, never bare `singularity`

## Phase 4: Rust Core Candidate

Goal: move stable host/control-plane pieces into Rust only after contracts prove stable.

Candidate Rust-owned areas:

- local daemon process shell
- IPC transport
- run-event fanout
- artifact serving by ref
- workspace locking
- process supervision wrappers
- configuration loading

Python remains owner of:

- planner/model/context/tool protocol until replaced intentionally
- existing tool/agent behavior
- plugin compatibility
- evaluation/replay until ported

## Phase 5: Python Plugin Management

Goal: make Python the explicit plugin component instead of an accidental monolith.

Deliverables:

- plugin process boundary or constrained host API
- tool broker registration
- policy-gated plugin permissions
- plugin trace/event bridge
- versioned plugin API

Rules:

- plugins never receive core component objects
- high-risk plugin tools still execute through ToolExecutor, PolicyEngine, CommandExecutor, SandboxManager, and trace
- dependency installation remains explicit and reviewed

## Migration Invariants

- local-first by default
- fail closed on missing policy, approval, or sandbox capability
- no raw secret persistence
- clients are replaceable
- session state is resumable
- every side effect has one owning component
- every phase leaves a runnable test behind

## Naming And Package

Names:

- product: Singularity
- Python package: `singularity`
- primary CLI: `singularity-agent`
- short CLI alias: `sg`

Rules:

- the package directory is `src/singularity`
- the installed commands are `singularity-agent` and `sg`
- do not add a bare `singularity` command because it conflicts with existing container tooling
- new Python imports use `singularity.*`

## Environment

Precedence:

```text
explicit CLI flag > SINGULARITY_* > config file > defaults
```

Required variables:

- `SINGULARITY_BASE_URL`
- `SINGULARITY_API_KEY`
- `SINGULARITY_MODEL`
- `SINGULARITY_HOME`
- `SINGULARITY_MODE`
- `SINGULARITY_PLUGIN_PATH`

Secrets must not be copied into config files.

## Config, State, And Cache

User directories on Linux-style systems:

```text
~/.config/singularity/
~/.local/share/singularity/
~/.cache/singularity/
```

Project-local user data uses `.singularity/`. AgentHost should expose state and artifacts by stable ids instead of requiring UI clients to parse local paths directly.
