//! 协议转换回归：全部使用固定的合成请求和内存流，不访问真实上游。
use super::*;

async fn translate_chat(payload: &str, byte_chunks: bool) -> (Vec<Value>, StreamOutcome) {
    let chunks = if byte_chunks {
        payload
            .as_bytes()
            .iter()
            .map(|byte| Bytes::from(vec![*byte]))
            .collect::<Vec<_>>()
    } else {
        vec![Bytes::copy_from_slice(payload.as_bytes())]
    };
    let result = chat_sse_stream_to_responses(
        futures_util::stream::iter(chunks.into_iter().map(Ok::<_, io::Error>)),
        "test-model".to_string(),
        HashSet::new(),
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let outcome = result.usage_rx.unwrap();
    let bytes = to_bytes(result.response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let mut text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut events = Vec::new();
    while let Some((event, consumed)) = next_sse_event(&text) {
        text.drain(..consumed);
        if let Some(data) = sse_data(&event) {
            if data.trim() != "[DONE]" {
                events.push(serde_json::from_str(&data).unwrap());
            }
        }
    }
    (events, outcome.await.unwrap())
}

#[tokio::test]
async fn protocol_chat_stream_keeps_trailing_usage() {
    let (events, outcome) = translate_chat(
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        ),
        false,
    )
    .await;
    assert!(outcome.error.is_none());
    assert_eq!(outcome.usage.input, 7);
    assert_eq!(outcome.usage.output, 3);
    let completed = events
        .iter()
        .filter(|e| e["type"] == "response.completed")
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["response"]["usage"]["total_tokens"], 10);
}

#[tokio::test]
async fn protocol_chat_stream_does_not_turn_eof_into_success() {
    let (events, outcome) = translate_chat(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        false,
    )
    .await;
    assert!(outcome.error.is_some());
    assert!(events.iter().any(|e| e["type"] == "response.failed"));
    assert!(!events.iter().any(|e| e["type"] == "response.completed"));
}

#[tokio::test]
async fn protocol_chat_stream_preserves_utf8_across_byte_chunks() {
    let (events, outcome) = translate_chat(
        "data: {\"choices\":[{\"delta\":{\"content\":\"你好🌍\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
        true,
    )
    .await;
    assert!(outcome.error.is_none());
    let text = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect::<String>();
    assert_eq!(text, "你好🌍");
}

#[tokio::test]
async fn protocol_chat_stream_and_json_preserve_truncation_reason() {
    for (finish, expected) in [
        ("length", "max_output_tokens"),
        ("content_filter", "content_filter"),
    ] {
        let chat = json!({"choices":[{"message":{"content":"partial"},"finish_reason":finish}]});
        let response = chat_to_response(chat, "test-model", &HashSet::new());
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], expected);
        let chunk = json!({"choices":[{"delta":{"content":"partial"},"finish_reason":finish}]});
        let (events, _) =
            translate_chat(&format!("data: {chunk}\n\ndata: [DONE]\n\n"), false).await;
        assert!(!events.iter().any(|e| e["type"] == "response.completed"));
        let terminal = events
            .iter()
            .find(|e| e["type"] == "response.incomplete")
            .unwrap();
        assert_eq!(
            terminal["response"]["incomplete_details"]["reason"],
            expected
        );
    }
}

#[test]
fn protocol_parallel_tool_history_remains_one_assistant_turn() {
    let request = json!({"model":"test-model", "input":[
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"checking"}]},
        {"type":"function_call","call_id":"call_a","name":"first","arguments":"{}"},
        {"type":"function_call","call_id":"call_b","name":"second","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_a","output":"A"},
        {"type":"function_call_output","call_id":"call_b","output":"B"}
    ]});
    let chat = responses_to_chat_body(&request).unwrap();
    assert_eq!(chat["messages"].as_array().unwrap().len(), 3);
    assert_eq!(chat["messages"][0]["content"], "checking");
    assert_eq!(
        chat["messages"][0]["tool_calls"].as_array().unwrap().len(),
        2
    );
    assert_eq!(chat["messages"][1]["tool_call_id"], "call_a");
    assert_eq!(chat["messages"][2]["tool_call_id"], "call_b");
    let roundtrip = crate::responses_api::chat_to_responses_request(&chat).unwrap();
    assert_eq!(roundtrip["input"], request["input"]);
}

#[test]
fn protocol_request_roundtrip_preserves_supported_options() {
    let chat = json!({
        "model":"test-model", "messages":[
            {"role":"developer","content":"follow these instructions"},
            {"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.test/image.png","detail":"low"}}]}
        ],
        "parallel_tool_calls":false, "reasoning_effort":"high",
        "tools":[{"type":"function","function":{"name":"lookup","strict":false,"parameters":{"type":"object"}}}],
        "response_format":{"type":"json_schema","json_schema":{"name":"result","description":"a result","schema":{"type":"object"},"strict":true}}
    });
    let responses = crate::responses_api::chat_to_responses_request(&chat).unwrap();
    // developer 不能降级为普通 user 指令。
    assert_eq!(responses["input"][0]["role"], "developer");
    assert_eq!(responses["input"][1]["content"][0]["detail"], "low");
    assert_eq!(responses["tools"][0]["strict"], false);
    assert_eq!(responses["parallel_tool_calls"], false);
    assert_eq!(responses["reasoning"]["effort"], "high");
    let roundtrip = responses_to_chat_body(&responses).unwrap();
    assert_eq!(roundtrip["response_format"], chat["response_format"]);
    assert_eq!(roundtrip["parallel_tool_calls"], false);
    assert_eq!(roundtrip["reasoning_effort"], "high");
    assert_eq!(roundtrip["tools"][0]["function"]["strict"], false);
    assert_eq!(
        roundtrip["messages"][1]["content"][0]["image_url"]["detail"],
        "low"
    );
}

#[tokio::test]
async fn protocol_usage_probe_does_not_cut_off_trailing_usage() {
    let chunks = [
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
    ];
    let (usage_rx, stream) = stream_with_usage_probe(
        futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<_, io::Error>(Bytes::from(chunk))),
        ),
        SseProbeKind::Chat,
        Instant::now(),
        None,
        CancellationToken::new(),
    );
    let result = chat_sse_stream_to_responses(
        stream,
        "test-model".into(),
        HashSet::new(),
        Instant::now(),
        CancellationToken::new(),
    )
    .unwrap();
    let rx = result.usage_rx.unwrap();
    let body = to_bytes(result.response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("\"total_tokens\":11"));
    let translated = rx.await.unwrap();
    let native = usage_rx.await.unwrap();
    assert_eq!(native.usage.input, 9);
    assert_eq!(translated.usage.input, 9);
    assert!(native.error.is_none());
    assert!(translated.error.is_none());
}

#[tokio::test]
async fn protocol_legal_empty_finish_survives_actual_chat_bridge() {
    for (reason, expected) in [
        ("length", "max_output_tokens"),
        ("content_filter", "content_filter"),
    ] {
        let role = json!({"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]});
        let finish = json!({"choices":[{"delta":{},"finish_reason":reason}]});
        let payload = format!("data: {role}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        let app = Router::new().fallback(move || {
            let payload = payload.clone();
            async move { ([("content-type", "text/event-stream")], payload) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = Client::new().get(&url).send().await.unwrap();
        let provider: ProviderConfig =
            serde_json::from_value(json!({"name":"test", "base_url":url, "api_key":"synthetic"}))
                .unwrap();
        let result = chat_stream_to_responses(
            response,
            "test-model".into(),
            HashSet::new(),
            Instant::now(),
            &provider,
            CancellationToken::new(),
        )
        .await;
        server.abort();
        let result = result.expect("合法无正文的结束原因不能被首帧探针判为失败");
        let usage = result.usage_rx.unwrap();
        let bytes = to_bytes(result.response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("response.incomplete"));
        assert!(text.contains(expected));
        let outcome = usage.await.unwrap();
        assert!(outcome.error.is_none());
        assert!(outcome.first_token_ms.is_none());
    }
}

#[tokio::test]
async fn protocol_finished_chat_has_bounded_tail_wait_and_keeps_usage() {
    for tail_error in [false, true] {
        let first = Bytes::from("data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n");
        let usage = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<_, io::Error>(Bytes::from("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n"))
        });
        let tail: ProxyByteStream = if tail_error {
            Box::pin(futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "synthetic tail timeout",
                ))
            }))
        } else {
            Box::pin(futures_util::stream::pending())
        };
        let stream = futures_util::stream::once(async move { Ok::<_, io::Error>(first) })
            .chain(usage)
            .chain(tail);
        let (native_usage, stream) = stream_with_usage_probe(
            stream,
            SseProbeKind::Chat,
            Instant::now(),
            None,
            CancellationToken::new(),
        );
        let result = chat_sse_stream_to_responses(
            stream,
            "test-model".into(),
            HashSet::new(),
            Instant::now(),
            CancellationToken::new(),
        )
        .unwrap();
        let translated_usage = result.usage_rx.unwrap();
        let bytes = tokio::time::timeout(
            Duration::from_secs(2),
            to_bytes(result.response.into_body(), 1024 * 1024),
        )
        .await
        .expect("finish_reason 已确认完成，不能无期限等待上游关闭")
        .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("response.completed"));
        assert!(!text.contains("response.failed"));
        assert!(text.contains("\"total_tokens\":10"));
        for outcome in [native_usage.await.unwrap(), translated_usage.await.unwrap()] {
            assert!(outcome.error.is_none());
            assert_eq!(outcome.usage.input, 7);
            assert_eq!(outcome.usage.output, 3);
        }
    }
}

#[tokio::test]
async fn protocol_anthropic_bridge_closes_on_message_stop_without_eof() {
    let data = concat!(
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let app = Router::new().fallback(move || async move {
        let stream =
            futures_util::stream::once(async move { Ok::<_, io::Error>(Bytes::from(data)) })
                .chain(futures_util::stream::pending());
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new();
    let provider: ProviderConfig = serde_json::from_value(json!({
        "name":"test", "provider_type":"anthropic", "base_url":url, "api_key":"synthetic", "max_retries":0
    })).unwrap();
    let result = send_anthropic_chat_stream_as_responses(
        &client,
        &provider,
        &HeaderMap::new(),
        json!({"model":"test-model", "messages":[{"role":"user","content":"hello"}]}),
        "test-model".into(),
        HashSet::new(),
        Instant::now(),
        &RequestContext::detached(),
    )
    .await
    .unwrap();
    let rx = result.usage_rx.unwrap();
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(result.response.into_body(), 1024 * 1024),
    )
    .await
    .expect("Anthropic message_stop 不应再等待连接关闭")
    .unwrap();
    let outcome = tokio::time::timeout(Duration::from_millis(200), rx)
        .await
        .expect("原生统计通道也应结束")
        .unwrap();
    server.abort();
    assert!(outcome.error.is_none());
    assert_eq!(outcome.usage.input, 5);
    assert!(String::from_utf8(bytes.to_vec())
        .unwrap()
        .contains("response.completed"));
}

#[tokio::test]
async fn protocol_parallel_stream_calls_keep_arguments_before_metadata() {
    let first = json!({"choices":[{"delta":{"tool_calls":[
        {"index":0,"function":{"arguments":"{\"q\":"}},
        {"index":1,"id":"call_b","function":{"name":"second","arguments":"{\"n\":"}}
    ]}}]});
    let second = json!({"choices":[{"delta":{"tool_calls":[
        {"index":0,"id":"call_a","function":{"name":"first","arguments":"\"你好\"}"}},
        {"index":1,"function":{"arguments":"2}"}}
    ]}, "finish_reason":"tool_calls"}]});
    let (events, outcome) = translate_chat(
        &format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n"),
        true,
    )
    .await;
    assert!(outcome.error.is_none());
    for (index, expected) in [(1, "{\"q\":\"你好\"}"), (2, "{\"n\":2}")] {
        let args = events
            .iter()
            .filter(|e| {
                e["type"] == "response.function_call_arguments.delta" && e["output_index"] == index
            })
            .filter_map(|e| e["delta"].as_str())
            .collect::<String>();
        assert_eq!(args, expected);
        assert!(events
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.done"
                && e["output_index"] == index
                && e["arguments"] == expected));
    }
}

#[tokio::test]
async fn protocol_reverse_translators_keep_split_utf8() {
    let responses = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好🌍\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
    );
    let anthropic = concat!(
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好🌍\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let google = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"你好🌍\"}]},\"finishReason\":\"STOP\"}]}\n\n";
    let bytes = |text: &str| {
        text.as_bytes()
            .iter()
            .map(|byte| Bytes::from(vec![*byte]))
            .collect::<Vec<_>>()
    };
    let (rx_r, stream_r) = crate::responses_api::spawn_responses_stream_translator_from_stream(
        futures_util::stream::iter(bytes(responses).into_iter().map(Ok::<_, io::Error>)),
        "test-model".into(),
        Instant::now(),
        CancellationToken::new(),
    );
    let (rx_a, stream_a) = crate::anthropic::spawn_stream_translator(
        futures_util::stream::iter(bytes(anthropic).into_iter().map(Ok::<_, io::Error>)),
        "test-model".into(),
        Instant::now(),
        CancellationToken::new(),
    );
    let (rx_g, stream_g) = crate::google_ai::spawn_stream_translator(
        futures_util::stream::iter(bytes(google).into_iter().map(Ok::<_, reqwest::Error>)),
        "test-model".into(),
        Instant::now(),
        CancellationToken::new(),
    );
    let streams: Vec<_> = vec![
        (rx_r, Body::from_stream(stream_r)),
        (rx_a, Body::from_stream(stream_a)),
        (rx_g, Body::from_stream(stream_g)),
    ];
    for (rx, body) in streams {
        let data = to_bytes(body, 1024 * 1024).await.unwrap();
        assert!(String::from_utf8(data.to_vec()).unwrap().contains("你好🌍"));
        assert!(rx.await.unwrap().error.is_none());
    }
}

#[test]
fn protocol_loss_diagnostics_only_contain_field_paths() {
    let request = json!({
        "model":"synthetic-private-value", "input":"synthetic-private-value",
        "previous_response_id":"synthetic-private-value",
        "tools":[{"type":"synthetic-private-value", "secret":"synthetic-private-value"}]
    });
    let _ = responses_to_chat_body(&request).unwrap();
    let logs = crate::runtime_log::snapshot();
    assert!(logs.iter().any(|log| log
        .message
        .contains("协议转换 Responses→Chat：previous_response_id")));
    assert!(logs
        .iter()
        .any(|log| log.message.contains("协议转换 Responses→Chat：tools[0]")));
    assert!(logs
        .iter()
        .filter(|log| log.message.starts_with("协议转换"))
        .all(|log| !log.message.contains("synthetic-private-value")));
}
