use crate::{
    error::{AgentBuildError, AgentError},
    events::EventSink,
    provider::{CompletionRequest, LlmProvider, ToolChoice},
    tool::Toolbox,
};
use models::agent::{AgentInput, ContentPart, Message, Role, ToolResultPart, Usage};
use models::events::{
    AgentEvent, InputMessageEvent, MessageCompleteEvent, MessageStartEvent, MessageStopEvent,
    RunCompleteEvent, ToolCompleteEvent, ToolExecutingEvent,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub stuck_threshold: usize,
    pub nudge_threshold: usize,
    pub max_tokens: Option<u32>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            stuck_threshold: 5,
            nudge_threshold: 3,
            max_tokens: None,
        }
    }
}

pub struct Agent {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) system_prompt: String,
    pub(crate) toolbox: Option<Arc<dyn Toolbox>>,
    pub(crate) handoff_tool: Option<String>,
    pub(crate) config: AgentConfig,
    pub(crate) history: Vec<Message>,
}

#[derive(Debug)]
pub struct RunOutput {
    pub result: AgentResult,
    pub usage: Usage,
}

#[derive(Debug)]
pub enum AgentResult {
    Completed { text: String },
    Handoff { tool_name: String, data: Value },
}

pub struct AgentBuilder {
    provider: Arc<dyn LlmProvider>,
    system_prompt: String,
    toolbox: Option<Arc<dyn Toolbox>>,
    handoff_tool: Option<String>,
    config: AgentConfig,
    history: Vec<Message>,
}

impl AgentBuilder {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            system_prompt: String::new(),
            toolbox: None,
            handoff_tool: None,
            config: AgentConfig::default(),
            history: Vec::new(),
        }
    }

    pub fn with_system_prompt(mut self, p: impl Into<String>) -> Self {
        self.system_prompt = p.into();
        self
    }

    pub fn with_toolbox(mut self, t: Arc<dyn Toolbox>) -> Self {
        self.toolbox = Some(t);
        self
    }

    pub fn with_handoff_tool(mut self, n: impl Into<String>) -> Self {
        self.handoff_tool = Some(n.into());
        self
    }

    pub fn with_config(mut self, c: AgentConfig) -> Self {
        self.config = c;
        self
    }

    pub fn with_history(mut self, h: Vec<Message>) -> Self {
        self.history = h;
        self
    }

    pub fn build(self) -> Result<Agent, AgentBuildError> {
        if self.config.nudge_threshold >= self.config.stuck_threshold {
            return Err(AgentBuildError::InvalidConfig {
                nudge: self.config.nudge_threshold,
                stuck: self.config.stuck_threshold,
            });
        }
        Ok(Agent {
            provider: self.provider,
            system_prompt: self.system_prompt,
            toolbox: self.toolbox,
            handoff_tool: self.handoff_tool,
            config: self.config,
            history: self.history,
        })
    }
}

fn extract_tool_calls(parts: &[ContentPart]) -> Vec<(String, String, Value)> {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc) => Some((tc.id.clone(), tc.name.clone(), tc.input.clone())),
            ContentPart::Text(_) | ContentPart::ToolResult(_) | ContentPart::Thinking(_) => None,
        })
        .collect()
}

fn extract_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            ContentPart::ToolCall(_) | ContentPart::ToolResult(_) | ContentPart::Thinking(_) => {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_fingerprint(tool_calls: &[(String, String, Value)]) -> String {
    tool_calls
        .iter()
        .map(|(_, name, input)| format!("{name}:{input}"))
        .collect::<Vec<_>>()
        .join("|")
}

impl Agent {
    pub fn builder(provider: Arc<dyn LlmProvider>) -> AgentBuilder {
        AgentBuilder::new(provider)
    }

    pub async fn run(
        &mut self,
        input: AgentInput,
        events: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<RunOutput, AgentError> {
        let run_id = Uuid::new_v4().to_string();

        let input_msg = input.to_message();
        events.emit(AgentEvent::InputMessage(InputMessageEvent {
            message_id: input.message_id(),
            input,
        }));
        self.history.push(input_msg);

        let mut total_usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        let mut iteration: u32 = 0;
        let mut recent_fingerprints: VecDeque<String> = VecDeque::new();

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if iteration >= self.config.max_iterations {
                return Err(AgentError::MaxIterationsExceeded {
                    max: self.config.max_iterations,
                });
            }
            iteration += 1;

            let tools = self.toolbox.as_ref().map(|t| t.specs()).unwrap_or_default();
            let request = CompletionRequest {
                messages: &self.history,
                system: if self.system_prompt.is_empty() {
                    None
                } else {
                    Some(self.system_prompt.clone())
                },
                tools,
                tool_choice: ToolChoice::Auto,
                max_tokens: self.config.max_tokens,
            };

            let msg_id = Uuid::new_v4().to_string();
            events.emit(AgentEvent::MessageStart(MessageStartEvent {
                message_id: msg_id.clone(),
                role: Role::Assistant,
            }));

            let response = self
                .provider
                .complete(request, &msg_id, events)
                .await
                .map_err(AgentError::Provider)?;

            events.emit(AgentEvent::MessageStop(MessageStopEvent {
                message_id: msg_id.clone(),
            }));

            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;

            let assistant_msg = Message {
                id: msg_id.clone(),
                role: Role::Assistant,
                parts: response.parts.clone(),
            };
            events.emit(AgentEvent::MessageComplete(MessageCompleteEvent {
                message_id: msg_id,
                message: assistant_msg.clone(),
            }));
            self.history.push(assistant_msg);

            let tool_calls = extract_tool_calls(&response.parts);

            if tool_calls.is_empty() {
                events.emit(AgentEvent::RunComplete(RunCompleteEvent {
                    message_id: run_id.clone(),
                    usage: total_usage.clone(),
                    iterations: iteration,
                }));
                return Ok(RunOutput {
                    result: AgentResult::Completed {
                        text: extract_text(&response.parts),
                    },
                    usage: total_usage,
                });
            }

            if let Some(ref handoff_name) = self.handoff_tool
                && let Some((_, name, data)) = tool_calls.iter().find(|(_, n, _)| n == handoff_name)
            {
                events.emit(AgentEvent::RunComplete(RunCompleteEvent {
                    message_id: run_id.clone(),
                    usage: total_usage.clone(),
                    iterations: iteration,
                }));
                return Ok(RunOutput {
                    result: AgentResult::Handoff {
                        tool_name: name.clone(),
                        data: data.clone(),
                    },
                    usage: total_usage,
                });
            }

            let fingerprint = tool_fingerprint(&tool_calls);
            recent_fingerprints.push_back(fingerprint.clone());
            if recent_fingerprints.len() > self.config.stuck_threshold {
                recent_fingerprints.pop_front();
            }

            if recent_fingerprints.len() >= self.config.stuck_threshold
                && recent_fingerprints.iter().all(|f| f == &fingerprint)
            {
                return Err(AgentError::StuckInLoop {
                    tool_name: tool_calls[0].1.clone(),
                    count: self.config.stuck_threshold,
                });
            }

            let should_nudge = recent_fingerprints.len() >= self.config.nudge_threshold
                && recent_fingerprints.iter().all(|f| f == &fingerprint);

            if should_nudge {
                for (tool_call_id, _, _) in &tool_calls {
                    let nudge_msg = Message {
                        id: format!("nudge:{tool_call_id}"),
                        role: Role::Tool,
                        parts: vec![ContentPart::ToolResult(ToolResultPart {
                            tool_call_id: tool_call_id.clone(),
                            output: "You have called this tool with identical arguments multiple times. Please try a different approach.".to_string(),
                            is_error: false,
                        })],
                    };
                    self.history.push(nudge_msg);
                }
                continue;
            }

            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            for (tool_call_id, name, input) in &tool_calls {
                let result_msg_id = format!("result:{tool_call_id}");

                events.emit(AgentEvent::ToolExecuting(ToolExecutingEvent {
                    message_id: result_msg_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                }));

                let (output, is_error) = match &self.toolbox {
                    None => (
                        format!("no toolbox available to execute tool '{name}'"),
                        true,
                    ),
                    Some(toolbox) => match toolbox.execute(name, input.clone()).await {
                        Ok(v) => (v.to_string(), false),
                        Err(e) => (e.to_string(), true),
                    },
                };

                events.emit(AgentEvent::ToolComplete(ToolCompleteEvent {
                    message_id: result_msg_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    output: output.clone(),
                    is_error,
                }));

                self.history.push(Message {
                    id: result_msg_id,
                    role: Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: tool_call_id.clone(),
                        output,
                        is_error,
                    })],
                });
            }
        }
    }
}
