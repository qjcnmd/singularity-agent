<!--
Sync Impact Report
- Version: 2.0.0 -> 3.0.0
- Modified principles:
  - I. Owner Intent Is Product Authority -> I. Owner Intent Is Product Authority
  - II. Mainstream Coding-Agent Baseline -> II. Pi-Led Coding-Agent Baseline
  - III. Daily Tool and Experiment Platform -> III. Daily Tool and Experiment Platform
  - IV. Small and Stable Core -> IV. Coherent Target Core
  - V. Reliable Real-Task Completion -> V. Evidence-Gated Real-Task Completion
  - VI. Clear Ownership and Maintainable Evolution ->
    VI. Clear Ownership and Maintainable Evolution
- Updated sections:
  - Product Scope and Boundaries
  - Development and Review Workflow
  - Governance
- Added sections: None
- Removed sections: None
- Follow-up TODOs: None
-->

# Singularity Constitution

## Core Principles

### I. Owner Intent Is Product Authority

The project owner's confirmed goals, usage expectations, priorities, and decisions are the
authority for product behavior. The active specification defines the accepted outcome for current
work. Code, tests, configuration, documentation, and reference products establish evidence about
the present system and available designs.

Before a material product change, the work MUST establish the intended user-visible outcome,
acceptance conditions, and behavior that remains stable. Questions to the owner MUST be reserved
for choices that materially change Singularity-specific behavior, scope, risk, cost, authority, or
long-term structure. Ordinary Coding Agent behavior follows Principle II.

Rationale: implementation artifacts have substantial AI-generated history, while product intent
belongs to the owner.

### II. Pi-Led Coding-Agent Baseline

Singularity is a general-purpose interactive Coding Agent. Pi is its primary product and design
reference for ordinary Coding Agent behavior. Relevant Pi source MUST be inspected when a decision
depends on interaction semantics, Agent execution, state ownership, or core structure. Other
mature harnesses supply focused evidence when their product surface or implementation is relevant.

The baseline covers project-aware conversation, repository inspection, file and shell operations,
sustained task context, user steering, autonomous task execution, and evidence-backed completion.
The smallest coherent design that serves Singularity's confirmed workflows is selected from the
available evidence. Owner questions focus on Singularity-specific differences and material
tradeoffs that the baseline cannot resolve.

Rationale: a primary reference keeps ordinary behavior coherent while preserving owner authority
over Singularity's scope.

### III. Daily Tool and Experiment Platform

Singularity serves two enduring roles: it is the owner's primary daily Coding Agent and a platform
for developing and evaluating Agent mechanisms. Daily task reliability takes priority when the two
roles compete.

An experiment MUST state the behavior or hypothesis under study, its bounded evaluation surface,
and its acceptance evidence. A mechanism enters the daily workflow after representative coding
tasks demonstrate practical value and the deterministic core contracts remain satisfied.

Rationale: daily use supplies meaningful research feedback, while a dependable workflow keeps
experiments grounded in real work.

### IV. Coherent Target Core

The core MUST be designed as one coherent system around the confirmed product workflow. Each
capability has a current consumer, one responsible owner, a narrow contract, and one authoritative
state source. A target design may replace existing module boundaries, interfaces, configuration,
and persisted shapes when the active specification requires a clearer system.

Core work MUST implement the approved target shape directly. The completed change contains one
active path for each behavior, one representation for each fact, and only abstractions with current
consumers. Superseded paths, duplicate state, unused indirection, and unreachable code are removed
within the same delivery boundary.

Rationale: a clean target model allows the whole core to converge instead of accumulating local
repairs around uncertain ownership.

### V. Evidence-Gated Real-Task Completion

Completion requires both deterministic core evidence and representative real-model coding tasks.
Deterministic tests MUST verify state transitions, ownership boundaries, failure behavior, and
public contracts at their responsible layer. Real-task evaluation MUST verify that the assembled
Agent can understand a repository, act correctly, validate its work, and report the outcome.

Evaluation tasks and checkers MUST be calibrated to confirmed daily workflows before their results
become a gate. Every real-model evaluation run requires the owner's explicit approval. A completion
claim states the observed evidence and any remaining uncertainty.

Rationale: internal correctness and useful Agent behavior are different properties, and the product
requires both.

### VI. Clear Ownership and Maintainable Evolution

Code and documentation MUST make product behavior, module ownership, state transitions, error
semantics, and durable design reasons understandable to people and future Agents. Abstractions MUST
reduce demonstrated complexity. Reviews classify individual findings by their underlying ownership
or fact-source failure and resolve the complete affected class within the authorized system
boundary.

Current implementation establishes observed behavior; the constitution and active specification
establish the target behavior. Work MUST inspect both, make the resulting responsibility model
explicit, and verify the assembled result at realistic boundaries.

Rationale: explicit ownership and evidence-based review allow structural work to converge on a
maintainable system.

## Product Scope and Boundaries

Singularity is a local Rust Coding Agent built primarily for the project owner's daily use. The
interactive terminal is the primary product surface. The non-interactive text and JSON event
interfaces support automation and evaluation through the same Agent capability.

The product delivers the mainstream Coding Agent workflow defined by Principle II. Additional
capabilities enter the product when they serve a confirmed daily workflow or a defined experiment
with observable acceptance evidence. Product growth follows observed use and validated experiments.

Architecture and capability work is admitted through four checks:

1. The outcome serves a confirmed workflow or defined experiment.
2. Its observable behavior and acceptance evidence are stated.
3. Its owner, authoritative state, and current consumer are identifiable.
4. The target design forms the smallest coherent system for the full approved scope.

## Development and Review Workflow

Work begins by classifying the request:

- Ordinary Coding Agent behavior uses the Pi-led baseline and current technical evidence.
- Singularity-specific behavior uses the owner's confirmed goals and active specification.
- A material unknown is brought to the owner in plain language with its practical consequence.

Each material change then follows this evidence loop:

1. State the desired outcome, acceptance conditions, affected scope, and stable behavior.
2. Inspect the current code, tests, configuration, documentation, and runtime path.
3. Inspect the relevant Pi implementation and any focused reference evidence that can change the
   decision.
4. Define the complete target ownership, state, interfaces, and transition boundary before editing.
5. Implement the target system and remove every directly superseded path and representation.
6. Run the deterministic gate closest to each contract.
7. With explicit owner approval, run the calibrated real-model evaluation gate.
8. Reconcile durable product, architecture, and operational truth in their responsible artifacts.

Reviews group findings by root responsibility and verify them against current source and runtime
evidence. Completion requires all confirmed material findings in the approved scope to be resolved,
the target ownership model to be internally consistent, and both evidence gates to pass.

## Governance

This constitution governs product specifications, architecture, implementation, review, and
documentation across the repository. Every material specification, plan, and implementation task
MUST read it before deciding product scope or long-term structure.

Authority is layered:

- This constitution records enduring owner-confirmed product direction and governance.
- The active Spec Kit specification records the accepted outcome and evidence for current work.
- Code, configuration, tests, Git state, and reproducible runs establish implemented reality.
- Architecture documentation explains the implemented design and changes with that design.
- Repository instructions define execution, navigation, validation, and reporting rules.

Amendments require the project owner's explicit confirmation and MUST state the affected principle
or scope, acceptance impact, and required revalidation. Semantic versioning applies:

- MAJOR for incompatible changes to product authority, core principles, or governance;
- MINOR for new principles or materially expanded requirements;
- PATCH for non-semantic clarification.

Reviews MUST identify constitution violations with concrete behavioral, source, or verification
evidence. A temporary deviation requires an owner-confirmed amendment with an explicit resolution
condition.

**Version**: 3.0.0 | **Ratified**: 2026-08-31 | **Last Amended**: 2026-08-31
