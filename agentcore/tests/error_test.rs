#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm,
    )
)]

use agentcore::{AgentError, LlmError, ToolCallError};

#[test]
fn agent_error_cancelled_display() {
    let e = AgentError::Cancelled;
    assert_eq!(e.to_string(), "cancelled");
}

#[test]
fn agent_error_max_iterations_display() {
    let e = AgentError::MaxIterationsExceeded { max: 50 };
    assert!(e.to_string().contains("50"));
}

#[test]
fn agent_error_stuck_display() {
    let e = AgentError::StuckInLoop {
        tool_name: "search".into(),
        count: 5,
    };
    assert!(e.to_string().contains("search"));
    assert!(e.to_string().contains("5"));
}

#[test]
fn tool_call_error_invalid_input_display() {
    let e = ToolCallError::InvalidInput("bad json".into());
    assert!(e.to_string().contains("bad json"));
}

#[test]
fn llm_error_api_error_display() {
    let e = LlmError::ApiError {
        status: 429,
        message: "rate limit".into(),
    };
    assert!(e.to_string().contains("429"));
}
