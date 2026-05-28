use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentBuildError {
    #[error("nudge_threshold ({nudge}) must be less than stuck_threshold ({stuck})")]
    InvalidConfig { nudge: usize, stuck: usize },
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("max iterations exceeded (max={max})")]
    MaxIterationsExceeded { max: u32 },

    #[error("stuck in loop: tool '{tool_name}' called identically {count} times")]
    StuckInLoop { tool_name: String, count: usize },

    #[error("provider error: {0}")]
    Provider(#[from] LlmError),

    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimit {
        retry_after: Option<std::time::Duration>,
    },

    #[error("provider overloaded")]
    Overloaded,

    #[error("api error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("network error: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Error)]
pub enum ToolCallError {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("execution error: {0}")]
    Execution(#[source] Box<dyn std::error::Error + Send + Sync>),
}
