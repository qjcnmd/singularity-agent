//! Bounded execution-plan validation failures.

use std::collections::BTreeSet;

use super::{AgentPlanStep, AgentPlanStepStatus, MAX_PLAN_STEP_CHARS, MAX_PLAN_STEPS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentPlanValidationFailure {
    Empty,
    TooManySteps,
    EmptyStep,
    StepTooLong,
    DuplicateStep,
    MultipleInProgress,
}

impl AgentPlanValidationFailure {
    fn code(self) -> &'static str {
        match self {
            Self::Empty => "plan_steps_empty",
            Self::TooManySteps => "plan_step_limit_exceeded",
            Self::EmptyStep => "plan_step_empty",
            Self::StepTooLong => "plan_step_too_long",
            Self::DuplicateStep => "plan_step_duplicate",
            Self::MultipleInProgress => "plan_multiple_in_progress",
        }
    }

    fn message(self) -> String {
        match self {
            Self::Empty => "plan must contain at least one step".to_string(),
            Self::TooManySteps => {
                format!("plan must not contain more than {MAX_PLAN_STEPS} steps")
            }
            Self::EmptyStep => "plan steps must not be empty".to_string(),
            Self::StepTooLong => {
                format!("plan steps must not exceed {MAX_PLAN_STEP_CHARS} characters")
            }
            Self::DuplicateStep => "plan steps must be unique".to_string(),
            Self::MultipleInProgress => "plan may have at most one in_progress step".to_string(),
        }
    }
}

pub(super) fn validate(steps: &[AgentPlanStep]) -> Result<(), String> {
    validate_contract(steps).map_err(AgentPlanValidationFailure::message)
}

pub(super) fn validate_code(steps: &[AgentPlanStep]) -> Result<(), &'static str> {
    validate_contract(steps).map_err(AgentPlanValidationFailure::code)
}

fn validate_contract(steps: &[AgentPlanStep]) -> Result<(), AgentPlanValidationFailure> {
    if steps.is_empty() {
        return Err(AgentPlanValidationFailure::Empty);
    }
    if steps.len() > MAX_PLAN_STEPS {
        return Err(AgentPlanValidationFailure::TooManySteps);
    }
    let mut unique_steps = BTreeSet::new();
    let mut in_progress_count = 0usize;
    for plan_step in steps {
        let normalized_step = plan_step.step.trim();
        if normalized_step.is_empty() {
            return Err(AgentPlanValidationFailure::EmptyStep);
        }
        if normalized_step.chars().count() > MAX_PLAN_STEP_CHARS {
            return Err(AgentPlanValidationFailure::StepTooLong);
        }
        if !unique_steps.insert(normalized_step.to_string()) {
            return Err(AgentPlanValidationFailure::DuplicateStep);
        }
        if plan_step.status == AgentPlanStepStatus::InProgress {
            in_progress_count += 1;
        }
    }
    if in_progress_count > 1 {
        return Err(AgentPlanValidationFailure::MultipleInProgress);
    }
    Ok(())
}
