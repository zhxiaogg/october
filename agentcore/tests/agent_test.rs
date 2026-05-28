#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm,
    )
)]

mod support;
use support::{CollectingEventSink, MockProvider, MockToolbox};

use agentcore::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_simple_text_completion() {
    let mut agent = Agent::builder(MockProvider::text("Hello, world!"))
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();
    let output = agent
        .run(
            AgentInput::user_message("msg-1", "hi"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    match output.result {
        AgentResult::Completed { text } => assert_eq!(text, "Hello, world!"),
        other => panic!(
            "expected Completed, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
    assert_eq!(output.usage.output_tokens, 5);
}

#[tokio::test]
async fn test_completion_emits_message_complete_events() {
    let mut agent = Agent::builder(MockProvider::text("done")).build().unwrap();
    let sink = CollectingEventSink::new();
    agent
        .run(
            AgentInput::user_message("msg-1", "go"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let ids = sink.message_complete_ids();
    assert!(
        !ids.is_empty(),
        "expected at least 1 MessageComplete event, got 0"
    );
}

#[tokio::test]
async fn test_input_message_event_emitted() {
    let mut agent = Agent::builder(MockProvider::text("ok")).build().unwrap();
    let sink = CollectingEventSink::new();
    agent
        .run(
            AgentInput::user_message("msg-1", "x"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let input_event = sink.events().into_iter().find_map(|e| match e {
        AgentEvent::InputMessage(ie) => Some(ie),
        _ => None,
    });
    let ie = input_event.unwrap();
    assert_eq!(ie.message_id, "msg-1");
    assert!(matches!(ie.input, AgentInput::UserMessage(_)));
}

#[tokio::test]
async fn test_run_complete_event_emitted() {
    let mut agent = Agent::builder(MockProvider::text("ok")).build().unwrap();
    let sink = CollectingEventSink::new();
    agent
        .run(
            AgentInput::user_message("msg-1", "x"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let count = sink
        .events()
        .iter()
        .filter(|e| matches!(e, AgentEvent::RunComplete(_)))
        .count();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_tool_call_cycle() {
    let provider = MockProvider::tool_then_text("tc1", "search", json!({"q": "rust"}), "found it");
    let toolbox = MockToolbox::echo("search");
    let mut agent = Agent::builder(provider)
        .with_toolbox(toolbox)
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();

    let output = agent
        .run(
            AgentInput::user_message("msg-1", "search rust"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    match output.result {
        AgentResult::Completed { text } => assert_eq!(text, "found it"),
        other => panic!(
            "expected Completed, got {:?}",
            std::mem::discriminant(&other)
        ),
    }

    let events = sink.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecuting(_)))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolComplete(_)))
    );
}

#[tokio::test]
async fn test_tool_result_message_id_is_derived_from_tool_call_id() {
    let provider = MockProvider::tool_then_text("tc1", "calc", json!({"x": 1}), "result: 1");
    let toolbox = MockToolbox::echo("calc");
    let mut agent = Agent::builder(provider)
        .with_toolbox(toolbox)
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();

    agent
        .run(
            AgentInput::user_message("msg-1", "calc"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let tc = sink
        .events()
        .into_iter()
        .find_map(|e| match e {
            AgentEvent::ToolComplete(tc) => Some(tc),
            _ => None,
        })
        .unwrap();
    assert_eq!(tc.message_id, format!("result:{}", tc.tool_call_id));
}

#[tokio::test]
async fn test_handoff_tool_returns_handoff_result() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "hc1".to_string(),
            name: "handoff".to_string(),
            input: json!({"answer": 42}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }]);
    let mut agent = Agent::builder(provider)
        .with_handoff_tool("handoff")
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();

    let output = agent
        .run(
            AgentInput::user_message("msg-1", "go"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    match output.result {
        AgentResult::Handoff { tool_name, data } => {
            assert_eq!(tool_name, "handoff");
            assert_eq!(data["answer"], 42);
        }
        other => panic!("expected Handoff, got {:?}", std::mem::discriminant(&other)),
    }
}

#[tokio::test]
async fn test_resume_with_tool_result() {
    let history = vec![
        Message {
            id: "m0".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: "question".into(),
            })],
        },
        Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "hc1".into(),
                name: "handoff".into(),
                input: json!({}),
            })],
        },
    ];
    let provider = MockProvider::text("thanks for the answer");
    let mut agent = Agent::builder(provider)
        .with_history(history)
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();

    let output = agent
        .run(
            AgentInput::tool_result("hc1", "42", false),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(output.result, AgentResult::Completed { .. }));

    let ie = sink
        .events()
        .into_iter()
        .find_map(|e| match e {
            AgentEvent::InputMessage(ie) => Some(ie),
            _ => None,
        })
        .unwrap();
    assert_eq!(ie.message_id, "result:hc1");
    assert!(matches!(ie.input, AgentInput::ToolResult(_)));
}

#[tokio::test]
async fn test_max_iterations_exceeded() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "t1".into(),
            name: "loop_tool".into(),
            input: json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 5,
            output_tokens: 2,
        },
    }]);
    let toolbox = MockToolbox::echo("loop_tool");
    let config = AgentConfig {
        max_iterations: 3,
        stuck_threshold: 10,
        nudge_threshold: 8,
        max_tokens: None,
    };
    let mut agent = Agent::builder(provider)
        .with_toolbox(toolbox)
        .with_config(config)
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();

    let err = agent
        .run(
            AgentInput::user_message("msg-1", "go"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::MaxIterationsExceeded { max: 3 }));
}

#[tokio::test]
async fn test_stuck_detection() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "s1".into(),
            name: "stuck_tool".into(),
            input: json!({"x": 1}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 5,
            output_tokens: 2,
        },
    }]);
    let toolbox = MockToolbox::echo("stuck_tool");
    let config = AgentConfig {
        max_iterations: 20,
        stuck_threshold: 3,
        nudge_threshold: 2,
        max_tokens: None,
    };
    let mut agent = Agent::builder(provider)
        .with_toolbox(toolbox)
        .with_config(config)
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();

    let err = agent
        .run(
            AgentInput::user_message("msg-1", "go"),
            &sink,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::StuckInLoop { .. }));
}

#[tokio::test]
async fn test_cancellation() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "c1".into(),
            name: "some_tool".into(),
            input: json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 5,
            output_tokens: 2,
        },
    }]);
    let toolbox = MockToolbox::echo("some_tool");
    let mut agent = Agent::builder(provider)
        .with_toolbox(toolbox)
        .build()
        .unwrap();
    let sink = CollectingEventSink::new();
    let token = CancellationToken::new();
    token.cancel();

    let err = agent
        .run(AgentInput::user_message("msg-1", "go"), &sink, token)
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Cancelled));
}
