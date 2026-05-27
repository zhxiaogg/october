use crate::{error::LlmError, events::EventSink, tool::ToolSpec};
use async_trait::async_trait;
use models::agent::{ContentPart, Message, Usage};

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub parts: Vec<ContentPart>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone)]
pub enum ToolChoice {
    Auto,
    Any,
    Required(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_id(&self) -> &str;
    async fn complete(
        &self,
        request: CompletionRequest,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError>;
}
