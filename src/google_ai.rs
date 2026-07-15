// Google AI Studio 原生 GenerateContent <-> OpenAI Chat Completions 翻译层。
//
// 对外仍保持 OpenAI chat/completions 形态；对上游使用
// /models/{model}:generateContent 与 /models/{model}:streamGenerateContent?alt=sse。

use crate::proxy::{next_sse_event, sse_data, StreamOutcome, TokenUsage};
use bytes::Bytes;
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use std::{io, time::Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

pub fn generate_content_path(model: &str, stream: bool) -> String {
    let model = model.trim().trim_start_matches('/');
    let model_path = if model.starts_with("models/") || model.starts_with("tunedModels/") {
        model.to_string()
    } else {
        format!("models/{model}")
    };
    if stream {
        format!("/{model_path}:streamGenerateContent?alt=sse")
    } else {
        format!("/{model_path}:generateContent")
    }
}

pub fn openai_to_google_request(body: &Value) -> Result<Value, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;
    let mut contents = Vec::new();
    let mut system_parts = Vec::new();

    for msg in obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "缺少 messages 字段".to_string())?
    {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        if matches!(role, "system" | "developer") {
            system_parts.extend(content_to_google_parts(msg.get("content"))?);
            continue;
        }

        if role == "tool" {
            let name = msg
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| msg.get("tool_call_id").and_then(Value::as_str))
                .unwrap_or("tool");
            let content = msg.get("content").and_then(plain_text).unwrap_or_default();
            contents.push(json!({
                "role": "function",
                "parts": [{
                    "functionResponse": {
                        "name": name,
                        "response": {"result": content}
                    }
                }]
            }));
            continue;
        }

        let google_role = if role == "assistant" { "model" } else { "user" };
        let mut parts = content_to_google_parts(msg.get("content"))?;
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let Some(function) = call.get("function") {
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let args = parse_json_string_or_value(function.get("arguments"))
                        .unwrap_or_else(|| json!({}));
                    if !name.is_empty() {
                        parts.push(json!({
                            "functionCall": {
                                "name": name,
                                "args": args
                            }
                        }));
                    }
                }
            }
        }
        if !parts.is_empty() {
            contents.push(json!({
                "role": google_role,
                "parts": parts,
            }));
        }
    }

    let mut out = Map::new();
    out.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        out.insert(
            "systemInstruction".into(),
            json!({
                "parts": system_parts,
            }),
        );
    }
    if let Some(config) = generation_config(obj) {
        out.insert("generationConfig".into(), config);
    }
    if let Some(tools) = google_tools(obj.get("tools")) {
        out.insert("tools".into(), tools);
    }
    if let Some(tool_config) = google_tool_config(obj.get("tool_choice")) {
        out.insert("toolConfig".into(), tool_config);
    }
    Ok(Value::Object(out))
}

pub fn google_to_openai_response(body: Value, request_model: &str) -> Value {
    let choice = body
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let parts = choice
        .and_then(|c| c.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let content = collect_text(&parts);
    let tool_calls = collect_function_calls(&parts);
    let mut message = json!({
        "role": "assistant",
        "content": if content.is_empty() { Value::Null } else { Value::String(content) },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish_reason = choice
        .and_then(|c| c.get("finishReason"))
        .and_then(Value::as_str)
        .map(map_finish_reason)
        .unwrap_or("stop");
    json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": request_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": google_usage_to_openai(body.get("usageMetadata")),
    })
}

pub fn spawn_stream_translator<S>(
    stream: S,
    request_model: String,
    started: Instant,
) -> (
    oneshot::Receiver<StreamOutcome>,
    impl Stream<Item = Result<Bytes, io::Error>>,
)
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    let (usage_tx, usage_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut stream = stream;
        let mut buffer = String::new();
        let mut usage = TokenUsage::default();
        let mut first_token_ms = None;
        let mut sent_role = false;
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some((event, consumed)) = next_sse_event(&buffer) {
                        buffer.drain(..consumed);
                        let Some(data) = sse_data(&event) else {
                            continue;
                        };
                        if data.trim() == "[DONE]" {
                            continue;
                        }
                        let value = match serde_json::from_str::<Value>(&data) {
                            Ok(value) => value,
                            Err(err) => {
                                error = Some(format!("Google AI SSE JSON 解析失败: {err}"));
                                break;
                            }
                        };
                        accumulate_google_usage(value.get("usageMetadata"), &mut usage);
                        let chunks =
                            google_chunk_to_openai_chunks(&value, &request_model, &mut sent_role);
                        for chunk in chunks {
                            if first_token_ms.is_none() && chunk_has_delta(&chunk) {
                                first_token_ms = Some(started.elapsed().as_millis() as i64);
                            }
                            if send_sse(&tx, &chunk).await.is_err() {
                                let _ =
                                    usage_tx.send(StreamOutcome::success(usage, first_token_ms));
                                return;
                            }
                        }
                    }
                }
                Err(err) => {
                    error = Some(err.to_string());
                    break;
                }
            }
            if error.is_some() {
                break;
            }
        }
        if error.is_none() {
            let done = Bytes::from_static(b"data: [DONE]\n\n");
            let _ = tx.send(Ok(done)).await;
        }
        let outcome = match error {
            Some(error) => StreamOutcome::failed(usage, first_token_ms, error),
            None => StreamOutcome::success(usage, first_token_ms),
        };
        let _ = usage_tx.send(outcome);
    });
    (usage_rx, ReceiverStream::new(rx))
}

fn content_to_google_parts(content: Option<&Value>) -> Result<Vec<Value>, String> {
    match content {
        Some(Value::String(text)) => Ok(vec![json!({"text": text})]),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" | "input_text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            parts.push(json!({"text": text}));
                        }
                    }
                    "image_url" => {
                        if let Some(url) = item
                            .get("image_url")
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)
                        {
                            parts.push(image_url_to_part(url)?);
                        }
                    }
                    "input_image" => {
                        if let Some(url) = item
                            .get("image_url")
                            .or_else(|| item.get("file_id"))
                            .and_then(Value::as_str)
                        {
                            parts.push(image_url_to_part(url)?);
                        }
                    }
                    _ => {}
                }
            }
            Ok(parts)
        }
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(other) => Ok(vec![json!({"text": other.to_string()})]),
    }
}

fn image_url_to_part(url: &str) -> Result<Value, String> {
    if let Some(data) = url.strip_prefix("data:") {
        let (mime, encoded) = data
            .split_once(";base64,")
            .ok_or_else(|| "Google AI 只支持 data:...;base64,... 图片".to_string())?;
        return Ok(json!({
            "inlineData": {
                "mimeType": mime,
                "data": encoded,
            }
        }));
    }
    Ok(json!({
        "fileData": {
            "mimeType": "image/*",
            "fileUri": url,
        }
    }))
}

fn generation_config(obj: &Map<String, Value>) -> Option<Value> {
    let mut config = Map::new();
    copy_number(obj, &mut config, "temperature", "temperature");
    copy_number(obj, &mut config, "top_p", "topP");
    copy_number(obj, &mut config, "top_k", "topK");
    copy_number(obj, &mut config, "max_tokens", "maxOutputTokens");
    copy_number(obj, &mut config, "max_completion_tokens", "maxOutputTokens");
    if let Some(stop) = obj.get("stop") {
        let stops = match stop {
            Value::String(text) => vec![Value::String(text.clone())],
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| Value::String(s.to_string()))
                .collect(),
            _ => Vec::new(),
        };
        if !stops.is_empty() {
            config.insert("stopSequences".into(), Value::Array(stops));
        }
    }
    if config.is_empty() {
        None
    } else {
        Some(Value::Object(config))
    }
}

fn copy_number(src: &Map<String, Value>, dst: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = src.get(from) {
        if value.is_number() {
            dst.insert(to.into(), value.clone());
        }
    }
}

fn google_tools(tools: Option<&Value>) -> Option<Value> {
    let declarations = tools?
        .as_array()?
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            let mut declaration = Map::new();
            declaration.insert("name".into(), Value::String(name.to_string()));
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                declaration.insert("description".into(), Value::String(description.to_string()));
            }
            if let Some(parameters) = function.get("parameters") {
                declaration.insert("parameters".into(), parameters.clone());
            }
            Some(Value::Object(declaration))
        })
        .collect::<Vec<_>>();
    if declarations.is_empty() {
        None
    } else {
        Some(json!([{"functionDeclarations": declarations}]))
    }
}

fn google_tool_config(tool_choice: Option<&Value>) -> Option<Value> {
    match tool_choice {
        Some(Value::String(choice)) if choice == "none" => {
            Some(json!({"functionCallingConfig": {"mode": "NONE"}}))
        }
        Some(Value::String(choice)) if choice == "auto" => {
            Some(json!({"functionCallingConfig": {"mode": "AUTO"}}))
        }
        Some(Value::Object(obj)) => {
            let name = obj
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)?;
            Some(json!({
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [name]
                }
            }))
        }
        _ => None,
    }
}

fn google_chunk_to_openai_chunks(
    value: &Value,
    request_model: &str,
    sent_role: &mut bool,
) -> Vec<Value> {
    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let parts = candidate
        .and_then(|c| c.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let finish_reason = candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(Value::as_str)
        .map(map_finish_reason);
    let mut chunks = Vec::new();
    let mut delta = Map::new();
    if !*sent_role {
        delta.insert("role".into(), Value::String("assistant".into()));
        *sent_role = true;
    }
    let text = collect_text(&parts);
    if !text.is_empty() {
        delta.insert("content".into(), Value::String(text));
    }
    let calls = collect_function_call_deltas(&parts);
    if !calls.is_empty() {
        delta.insert("tool_calls".into(), Value::Array(calls));
    }
    if !delta.is_empty() {
        chunks.push(openai_stream_chunk(
            request_model,
            Value::Object(delta),
            Value::Null,
        ));
    }
    if let Some(reason) = finish_reason {
        chunks.push(openai_stream_chunk(
            request_model,
            json!({}),
            Value::String(reason.to_string()),
        ));
    }
    chunks
}

fn openai_stream_chunk(request_model: &str, delta: Value, finish_reason: Value) -> Value {
    json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4().simple()),
        "object": "chat.completion.chunk",
        "created": Utc::now().timestamp(),
        "model": request_model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    })
}

fn collect_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn collect_function_calls(parts: &[Value]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| {
            let call = part.get("functionCall")?;
            let name = call.get("name").and_then(Value::as_str)?;
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "id": format!("call_{}", Uuid::new_v4().simple()),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args.to_string(),
                }
            }))
        })
        .collect()
}

fn collect_function_call_deltas(parts: &[Value]) -> Vec<Value> {
    collect_function_calls(parts)
        .into_iter()
        .enumerate()
        .map(|(index, call)| {
            json!({
                "index": index,
                "id": call.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": call.get("function").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

fn google_usage_to_openai(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total = usage
        .and_then(|u| u.get("totalTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or(input + output);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": total,
    })
}

fn accumulate_google_usage(value: Option<&Value>, usage: &mut TokenUsage) {
    let Some(value) = value else {
        return;
    };
    let input = value
        .get("promptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(usage.input);
    let output = value
        .get("candidatesTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(usage.output);
    usage.input = input;
    usage.output = output;
}

async fn send_sse(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    value: &Value,
) -> Result<(), mpsc::error::SendError<Result<Bytes, io::Error>>> {
    tx.send(Ok(Bytes::from(format!("data: {value}\n\n")))).await
}

fn chunk_has_delta(value: &Value) -> bool {
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"))
        .and_then(Value::as_object)
        .is_some_and(|delta| delta.contains_key("content") || delta.contains_key("tool_calls"))
}

fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content_filter",
        "MALFORMED_FUNCTION_CALL" => "tool_calls",
        _ => "stop",
    }
}

fn parse_json_string_or_value(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::String(text)) => serde_json::from_str(text).ok(),
        Some(value) => Some(value.clone()),
        None => None,
    }
}

fn plain_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_chat_request_to_google_contents() {
        let body = json!({
            "model": "gpt",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 8
        });
        let out = openai_to_google_request(&body).unwrap();
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(out["contents"][0]["role"], "user");
        assert_eq!(out["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(out["generationConfig"]["maxOutputTokens"], 8);
    }

    #[test]
    fn translates_google_response_to_openai_chat() {
        let body = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 2,
                "totalTokenCount": 5
            }
        });
        let out = google_to_openai_response(body, "gemini-2.5-pro");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["usage"]["total_tokens"], 5);
    }
}
