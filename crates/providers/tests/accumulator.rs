use futures::stream;
use kyora_protocol::{BlockStart, ContentBlock, StopReason, StreamEvent, Usage};
use kyora_providers::{Accumulator, ProviderError, collect};
use serde_json::json;

fn start(usage: Usage) -> StreamEvent {
    StreamEvent::MessageStart {
        id: Some("response-id".into()),
        model: "test-model".into(),
        usage,
    }
}

fn accumulator() -> Accumulator {
    let mut accumulator = Accumulator::new();
    accumulator.push(start(Usage::default())).unwrap();
    accumulator
}

fn stop(accumulator: &mut Accumulator) {
    accumulator
        .push(StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        })
        .unwrap();
    accumulator.push(StreamEvent::MessageStop).unwrap();
}

#[test]
fn text_deltas_and_blocks_are_ordered_by_index() {
    let mut acc = accumulator();
    for event in [
        StreamEvent::BlockStart {
            index: 8,
            block: BlockStart::Text,
        },
        StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Text,
        },
        StreamEvent::TextDelta {
            index: 8,
            text: "second".into(),
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "hel".into(),
        },
        StreamEvent::TextDelta {
            index: 2,
            text: "lo".into(),
        },
        StreamEvent::BlockStop { index: 2 },
        StreamEvent::BlockStop { index: 8 },
    ] {
        acc.push(event).unwrap();
    }
    stop(&mut acc);
    let response = acc.finish().unwrap();
    assert_eq!(response.id.as_deref(), Some("response-id"));
    assert_eq!(response.model, "test-model");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        response.content,
        vec![
            ContentBlock::Text {
                text: "hello".into()
            },
            ContentBlock::Text {
                text: "second".into()
            }
        ]
    );
}

#[test]
fn thinking_and_signature_deltas_are_replayed_verbatim() {
    let mut acc = accumulator();
    for event in [
        StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Thinking,
        },
        StreamEvent::ThinkingDelta {
            index: 0,
            thinking: "think\n".into(),
        },
        StreamEvent::ThinkingDelta {
            index: 0,
            thinking: "more".into(),
        },
        StreamEvent::SignatureDelta {
            index: 0,
            signature: "sig".into(),
        },
        StreamEvent::SignatureDelta {
            index: 0,
            signature: "nature".into(),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Thinking,
        },
        StreamEvent::BlockStop { index: 1 },
    ] {
        acc.push(event).unwrap();
    }
    stop(&mut acc);
    assert_eq!(
        acc.finish().unwrap().content,
        vec![
            ContentBlock::Thinking {
                thinking: "think\nmore".into(),
                signature: Some("signature".into())
            },
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: None
            },
        ]
    );
}

#[test]
fn chunked_tool_json_is_parsed_at_block_stop() {
    let mut acc = accumulator();
    acc.push(StreamEvent::BlockStart {
        index: 0,
        block: BlockStart::ToolUse {
            id: "call".into(),
            name: "echo".into(),
        },
    })
    .unwrap();
    for partial_json in ["{\"te", "xt\":\"hé", "llo\",\"nested\":[1,true]}"] {
        acc.push(StreamEvent::ToolInputDelta {
            index: 0,
            partial_json: partial_json.into(),
        })
        .unwrap();
    }
    assert!(acc.invalid_tool_inputs().is_empty());
    acc.push(StreamEvent::BlockStop { index: 0 }).unwrap();
    stop(&mut acc);
    assert!(acc.invalid_tool_inputs().is_empty());
    assert_eq!(
        acc.finish().unwrap().content,
        vec![ContentBlock::ToolUse {
            id: "call".into(),
            name: "echo".into(),
            input: json!({"text": "héllo", "nested": [1, true]})
        }]
    );
}

#[test]
fn tool_json_is_strict_and_invalid_input_is_flagged() {
    for (raw, expected, invalid) in [
        ("", json!({}), false),
        ("{}", json!({}), false),
        ("{\"a\":", json!("{\"a\":"), true),
        ("{} trailing", json!("{} trailing"), true),
        ("{} {}", json!("{} {}"), true),
        (" ", json!(" "), true),
        ("\"valid string\"", json!("valid string"), false),
    ] {
        let mut acc = accumulator();
        acc.push(StreamEvent::BlockStart {
            index: 3,
            block: BlockStart::ToolUse {
                id: "call".into(),
                name: "echo".into(),
            },
        })
        .unwrap();
        acc.push(StreamEvent::ToolInputDelta {
            index: 3,
            partial_json: raw.into(),
        })
        .unwrap();
        acc.push(StreamEvent::BlockStop { index: 3 }).unwrap();
        assert_eq!(
            acc.invalid_tool_inputs(),
            if invalid { &[3][..] } else { &[][..] }
        );
        stop(&mut acc);
        assert_eq!(
            acc.finish().unwrap().content,
            vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: "echo".into(),
                input: expected
            }]
        );
    }
}

#[test]
fn cumulative_usage_preserves_initial_counters_without_double_counting() {
    let mut acc = Accumulator::new();
    acc.push(start(Usage {
        input_tokens: 10,
        output_tokens: 1,
        cache_creation_input_tokens: 2,
        cache_read_input_tokens: 3,
    }))
    .unwrap();
    for usage in [
        Usage {
            output_tokens: 4,
            ..Usage::default()
        },
        Usage {
            input_tokens: 12,
            output_tokens: 6,
            cache_creation_input_tokens: 5,
            ..Usage::default()
        },
        Usage::default(),
    ] {
        acc.push(StreamEvent::MessageDelta {
            stop_reason: None,
            usage,
        })
        .unwrap();
    }
    stop(&mut acc);
    let usage = acc.finish().unwrap().usage;
    assert_eq!(
        usage,
        Usage {
            input_tokens: 12,
            output_tokens: 6,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 3
        }
    );
    assert_eq!(usage.total(), 26);
}

#[test]
fn opaque_data_is_preserved() {
    let raw = json!({"data": [1, "redacted", null]});
    let mut acc = accumulator();
    acc.push(StreamEvent::BlockStart {
        index: 0,
        block: BlockStart::Opaque {
            provider: "test".into(),
            kind: "reasoning".into(),
            raw: raw.clone(),
        },
    })
    .unwrap();
    acc.push(StreamEvent::BlockStop { index: 0 }).unwrap();
    stop(&mut acc);
    assert_eq!(
        acc.finish().unwrap().content,
        vec![ContentBlock::Opaque {
            provider: "test".into(),
            kind: "reasoning".into(),
            raw
        }]
    );
}

#[test]
fn inconsistent_stream_structure_is_rejected() {
    let begin = || start(Usage::default());
    let block = || StreamEvent::BlockStart {
        index: 0,
        block: BlockStart::Text,
    };
    for events in [
        vec![StreamEvent::MessageStop],
        vec![begin(), begin()],
        vec![begin(), block(), block()],
        vec![
            begin(),
            StreamEvent::TextDelta {
                index: 0,
                text: "unknown".into(),
            },
        ],
        vec![
            begin(),
            block(),
            StreamEvent::ThinkingDelta {
                index: 0,
                thinking: "wrong".into(),
            },
        ],
        vec![
            begin(),
            block(),
            StreamEvent::SignatureDelta {
                index: 0,
                signature: "wrong".into(),
            },
        ],
        vec![
            begin(),
            block(),
            StreamEvent::ToolInputDelta {
                index: 0,
                partial_json: "{}".into(),
            },
        ],
        vec![begin(), block(), StreamEvent::MessageStop],
        vec![
            begin(),
            block(),
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::TextDelta {
                index: 0,
                text: "late".into(),
            },
        ],
        vec![begin(), StreamEvent::MessageStop, block()],
    ] {
        let mut acc = Accumulator::new();
        let result = events.into_iter().try_for_each(|event| acc.push(event));
        assert!(matches!(result, Err(ProviderError::Protocol(_))));
    }
    assert!(matches!(
        Accumulator::new().finish(),
        Err(ProviderError::Protocol(_))
    ));
    let mut acc = accumulator();
    acc.push(StreamEvent::MessageStop).unwrap();
    assert!(matches!(acc.finish(), Err(ProviderError::Protocol(_))));
}

#[tokio::test]
async fn collect_propagates_stream_errors_and_rejects_truncation() {
    let events = vec![
        Ok(start(Usage::default())),
        Err(ProviderError::Transport("lost connection".into())),
    ];
    assert!(matches!(
        collect(Box::pin(stream::iter(events))).await,
        Err(ProviderError::Transport(_))
    ));
    assert!(matches!(
        collect(Box::pin(stream::iter([Ok(start(Usage::default()))]))).await,
        Err(ProviderError::Protocol(_))
    ));
}
