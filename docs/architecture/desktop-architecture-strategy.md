# Singularity Desktop Architecture Strategy

Singularity targets Rust Core + Tauri Desktop + TypeScript UI + Python Plugin Management. This is a staged architecture decision, not a Rust or Tauri implementation in the current Python CLI baseline.

## Target Shape

```text
Tauri Desktop / TypeScript UI / CLI clients
-> AgentHost
-> Rust Core control plane candidates
-> Python Plugin Management and existing Python component adapters
-> Tool Broker
-> ToolExecutor / PolicyEngine / ApprovalGate / TraceRecorder
```

The first implementation boundary is AgentHost around the current Python component. Rust Core is introduced only where the host contract becomes stable enough to move process, IPC, locking, event fanout, artifact serving, and supervision logic without changing agent behavior.

## Why This Stack

Rust Core is a good fit for the future host/control plane because it gives a small native binary, explicit ownership, predictable concurrency, local IPC options, and safer process supervision. Those are core-product properties once Singularity becomes a resident desktop component instead of a one-shot CLI process.

Tauri keeps the desktop shell native and small while letting the UI use TypeScript. It avoids bundling a full browser component per app, keeps local filesystem and command permissions explicit, and gives a clear command boundary between UI and component.

TypeScript UI is the practical choice for timeline, approval, trace, protocol, artifact, and workspace-state views. The product needs rich interactive state inspection, not a terminal renderer stretched into a desktop app.

Python Plugin Management preserves the current working component and plugin ecosystem. Python remains where local tools, providers, evaluation hooks, and user extensions are cheapest to write and maintain. It becomes an explicit plugin/component layer instead of the long-term product core.

## Rejected Alternatives

Pure Python GUI is rejected because it keeps process lifecycle, UI rendering, plugin execution, and component ownership in one language/process family. That makes desktop packaging and crash isolation weaker, and it does not create a clean AgentHost boundary.

Electron is rejected because Singularity is local-first and component-heavy. Shipping another full Chromium/Node desktop stack adds memory and update surface without enough benefit over Tauri.

Go Core is rejected because Go would solve packaging and concurrency, but Rust gives stronger native ownership semantics, better fit with Tauri, and a smaller long-term boundary for sensitive local process control.

Java/Spring is rejected because its service framework model is too heavy for a local desktop agent component and would push the project toward server-style layering.

All Rust UI is rejected because the product needs fast iteration on dense inspection views. Rust should own stable host/control-plane code, not every UI interaction.

Pure Web SaaS is rejected because Singularity's safety model is local-first: workspace access, approvals, traces, memory, and artifacts must remain local unless a future sync feature is explicitly designed.

## AgentHost As Core Boundary

AgentHost becomes the product boundary because it is the first layer that every client can share. It owns start, resume, cancel, approval submission, state snapshots, artifact reads, event streaming, health, and recovery.

CLI is demoted from core to client because it should only parse user intent, render results, and return exit codes. Component state, policy decisions, tool execution, trace, and recovery must live behind AgentHost so desktop and CLI behave the same way.

## Tool Broker And MCP

MCP must go through Tool Broker because direct MCP access from UI or model output would bypass schemas, policy, approval, replay, idempotency, trace, and backend ownership. MCP tools are normal tool declarations with side-effect metadata and must execute through ToolExecutor and the owning component.

## Python Component Position

Python should not stay the core component because the desktop product needs a stable local host boundary, native lifecycle control, and UI-independent process ownership. Python remains valuable as Plugin Management and implementation layer during the transition.

## Non-Goals

- no Tauri implementation in this phase
- no Rust workspace in this phase
- no automatic migration or deletion of user data
