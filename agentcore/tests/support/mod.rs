#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm,
    )
)]

use agentcore::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub struct MockProvider {
    responses: Vec<CompletionResponse>,
    call_index: Mutex<usize>,
}

impl MockProvider {
    pub fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses,
            call_index: Mutex::new(0),
        })
    }

    pub fn text(text: &str) -> Arc<Self> {
        Self::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }])
    }

    pub fn tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self> {
        Self::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: reply.to_string(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                },
            },
        ])
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_id(&self) -> &str {
        "mock-model"
    }

    async fn complete(
        &self,
        _request: CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let mut idx = self.call_index.lock().unwrap();
        let response = self.responses[*idx % self.responses.len()].clone();
        *idx += 1;
        Ok(response)
    }
}

type ToolHandler = Arc<dyn Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync>;

pub struct MockToolbox {
    specs: Vec<ToolSpec>,
    handler: ToolHandler,
}

impl MockToolbox {
    pub fn new(
        specs: Vec<ToolSpec>,
        handler: impl Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            specs,
            handler: Arc::new(handler),
        })
    }

    pub fn echo(name: &str) -> Arc<Self> {
        let spec = ToolSpec {
            name: name.to_string(),
            description: "echo tool".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        Self::new(vec![spec], |_, input| Ok(input))
    }
}

#[async_trait]
impl Toolbox for MockToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        (self.handler)(name, input)
    }
}

pub struct CollectingEventSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl CollectingEventSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn message_complete_ids(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::MessageComplete(mc) => Some(mc.message_id),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for CollectingEventSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}
