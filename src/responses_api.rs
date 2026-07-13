// OpenAI Chat Completions -> Responses API 反向翻译层
//
// 适用场景：客户端发 /v1/chat/completions，但上游只支持 /v1/responses。
// 三个入口：
//   - chat_to_responses_request: Chat 请求 -> Responses 请求 body
//   - responses_to_chat_response: Responses 响应 -> Chat 响应（非流式）
//   - spawn_responses_stream_translator: Responses SSE -> Chat SSE（状态机）

use crate::proxy::{next_sse_event, sse_data, TokenUsage};
use bytes::Bytes;
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

// ============================== 请求翻译 ==============================

pub fn chat_to_responses_request(body: &Value) -> Result<Value, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少 model 字段".to_string())?
        .to_string();

    // Chat -> Responses：把 system 消息合并到 top-level instructions
    // 其它消息按顺序转成 input items（message / function_call / function_call_output）
    let mut instructions: Vec<String> = Vec::new();
    let mut input_items: Vec<Value> = Vec::new();

    if let Some(arr) = obj.get("messages").and_then(Value::as_array) {
        for msg in arr {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "system" => {
                    if let Some(text) = extract_plain_text(msg.get("content")) {
                        if !text.is_empty() {
                            instructions.push(text);
                        }
                    }
                }
                "user" => {
                    let content = convert_user_content(msg.get("content"))?;
                    input_items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
                "assistant" => {
                    // 文本部分（若有）
                    if let Some(text) = msg.get("content").and_then(|c| extract_plain_text(Some(c)))
                    {
                        if !text.is_empty() {
                            input_items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}],
                            }));
                        }
                    }
                    // tool_calls -> function_call items
                    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let call_id = call
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = call
                                .pointer("/function/name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let arguments = call
                                .pointer("/function/arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string();
                            input_items.push(json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                    }
                    // 兼容旧字段 function_call
                    if let Some(fc) = msg.get("function_call") {
                        if let Some(name) = fc.get("name").and_then(Value::as_str) {
                            let arguments = fc
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string();
                            input_items.push(json!({
                                "type": "function_call",
                                "call_id": format!("call_{}", Uuid::new_v4().simple()),
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                    }
                }
                "tool" => {
                    let call_id = msg
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let output = tool_output_text(msg.get("content"));
                    input_items.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                }
                _ => {
                    // function / 其它：当纯文本 user 处理
                    if let Some(text) = extract_plain_text(msg.get("content")) {
                        if !text.is_empty() {
                            input_items.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "input_text", "text": text}],
                            }));
                        }
                    }
                }
            }
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model));
    out.insert("input".into(), Value::Array(input_items));
    if !instructions.is_empty() {
        out.insert(
            "instructions".into(),
            Value::String(instructions.join("\n\n")),
        );
    }

    // 采样参数
    if let Some(v) = obj.get("temperature") {
        out.insert("temperature".into(), v.clone());
    }
    if let Some(v) = obj.get("top_p") {
        out.insert("top_p".into(), v.clone());
    }
    if let Some(v) = obj.get("stream") {
        out.insert("stream".into(), v.clone());
    }
    // max_tokens -> max_output_tokens
    if let Some(v) = obj
        .get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
    {
        out.insert("max_output_tokens".into(), v.clone());
    }
    // stop 直接透传（Responses 也叫 stop）
    if let Some(v) = obj.get("stop") {
        out.insert("stop".into(), v.clone());
    }
    // user 透传
    if let Some(v) = obj.get("user") {
        out.insert("user".into(), v.clone());
    }
    // response_format=json_object -> text.format
    if let Some(rf) = obj.get("response_format") {
        if rf.get("type").and_then(Value::as_str) == Some("json_object") {
            out.insert("text".into(), json!({"format": {"type": "json_object"}}));
        } else if rf.get("type").and_then(Value::as_str) == Some("json_schema") {
            // 尽量转 json_schema：Responses 用 text.format
            if let Some(schema) = rf.get("json_schema") {
                out.insert(
                    "text".into(),
                    json!({"format": {"type": "json_schema", "schema": schema}}),
                );
            }
        }
    }

    // tools：结构从 {type:"function", function:{name,parameters}} 扁平成 {type:"function", name, parameters}
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let mut out_tools = Vec::new();
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            let Some(func) = tool.get("function") else {
                continue;
            };
            let Some(name) = func.get("name").and_then(Value::as_str) else {
                continue;
            };
            let description = func
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parameters = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            out_tools.push(json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }));
        }
        if !out_tools.is_empty() {
            out.insert("tools".into(), Value::Array(out_tools));
        }
    }

    if let Some(tc) = obj.get("tool_choice") {
        if let Some(mapped) = convert_tool_choice(tc) {
            out.insert("tool_choice".into(), mapped);
        }
    }

    Ok(Value::Object(out))
}

fn convert_tool_choice(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" | "none" | "required" => Some(Value::String(s.clone())),
            _ => None,
        },
        Value::Object(_) => {
            let ty = tc.get("type").and_then(Value::as_str)?;
            if ty == "function" {
                let name = tc.pointer("/function/name").and_then(Value::as_str)?;
                Some(json!({"type": "function", "name": name}))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_plain_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let mut buf = String::new();
            for part in arr {
                let ty = part.get("type").and_then(Value::as_str);
                if matches!(ty, Some("text") | Some("input_text") | Some("output_text")) {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        buf.push_str(t);
                    }
                }
            }
            Some(buf)
        }
        _ => None,
    }
}

fn convert_user_content(content: Option<&Value>) -> Result<Value, String> {
    match content {
        Some(Value::String(s)) => Ok(json!([{"type": "input_text", "text": s}])),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        out.push(json!({"type": "input_text", "text": text}));
                    }
                    Some("image_url") => {
                        let url = part
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "image_url.url 缺失".to_string())?;
                        // Responses API: input_image 用一个 image_url 字段承载 URL 或 data URI
                        out.push(json!({
                            "type": "input_image",
                            "image_url": url,
                        }));
                    }
                    Some("input_audio") => {
                        out.push(json!({
                            "type": "input_text",
                            "text": "[audio content omitted]"
                        }));
                    }
                    _ => continue,
                }
            }
            if out.is_empty() {
                out.push(json!({"type": "input_text", "text": ""}));
            }
            Ok(Value::Array(out))
        }
        Some(Value::Null) | None => Ok(json!([{"type": "input_text", "text": ""}])),
        Some(other) => Ok(json!([{"type": "input_text", "text": other.to_string()}])),
    }
}

fn tool_output_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut buf = String::new();
            for part in arr {
                let ty = part.get("type").and_then(Value::as_str);
                if matches!(ty, Some("text") | Some("input_text") | Some("output_text")) {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        buf.push_str(t);
                    }
                }
            }
            buf
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

// ============================== 非流式响应翻译 ==============================

pub fn responses_to_chat_response(body: Value, request_model: &str) -> Value {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(request_model)
        .to_string();

    let mut text_buf = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(items) = body.get("output").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            let ty = part.get("type").and_then(Value::as_str);
                            if matches!(ty, Some("output_text") | Some("text")) {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    text_buf.push_str(t);
                                }
                            }
                        }
                    }
                }
                Some("function_call") | Some("custom_tool_call") => {
                    let call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let arguments =
                        if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
                            encode_custom_tool_arguments(
                                item.get("input").and_then(Value::as_str).unwrap_or(""),
                            )
                        } else {
                            item.get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string()
                        };
                    tool_calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }));
                }
                _ => {}
            }
        }
    }
    // 兜底：Responses API 有时给 output_text 简化字段
    if text_buf.is_empty() {
        if let Some(t) = body.get("output_text").and_then(Value::as_str) {
            text_buf.push_str(t);
        }
    }

    let status = body.get("status").and_then(Value::as_str);
    let incomplete_reason = body
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    let finish_reason = map_finish_reason(status, incomplete_reason, !tool_calls.is_empty());

    let mut message = Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    message.insert(
        "content".into(),
        if text_buf.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(text_buf)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    let (input_tokens, output_tokens) = extract_usage(&body);

    json!({
        "id": if id.is_empty() { format!("chatcmpl-{}", Uuid::new_v4().simple()) } else { id },
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    })
}

fn encode_custom_tool_arguments(input: &str) -> String {
    serde_json::to_string(&json!({"input": input}))
        .unwrap_or_else(|_| "{\"input\":\"\"}".to_string())
}

fn map_finish_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
    has_tool_calls: bool,
) -> &'static str {
    match status {
        Some("completed") => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
        Some("incomplete") => match incomplete_reason {
            Some("max_output_tokens") | Some("max_tokens") => "length",
            Some("content_filter") | Some("safety") => "content_filter",
            _ => "stop",
        },
        Some("failed") => "stop",
        _ => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
    }
}

fn extract_usage(body: &Value) -> (i64, i64) {
    let usage = body.get("usage");
    let input = usage
        .and_then(|u| u.get("input_tokens").or_else(|| u.get("prompt_tokens")))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| {
            u.get("output_tokens")
                .or_else(|| u.get("completion_tokens"))
        })
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (input, output)
}

// ============================== 流式 SSE 翻译 ==============================

/// 把上游 Responses 流式响应转换成 OpenAI chat.completion.chunk 流。
#[allow(dead_code)]
pub fn spawn_responses_stream_translator(
    resp: reqwest::Response,
    request_model: String,
) -> (
    oneshot::Receiver<TokenUsage>,
    ReceiverStream<Result<Bytes, io::Error>>,
) {
    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| io::Error::other(err.to_string())));
    spawn_responses_stream_translator_from_stream(stream, request_model)
}

pub fn spawn_responses_stream_translator_from_stream<S>(
    stream: S,
    request_model: String,
) -> (
    oneshot::Receiver<TokenUsage>,
    ReceiverStream<Result<Bytes, io::Error>>,
)
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(64);
    let (u_tx, u_rx) = oneshot::channel::<TokenUsage>();
    tokio::spawn(async move {
        let mut state = StreamState::new(request_model);
        let mut buffer = String::new();
        let mut stream = Box::pin(stream);
        if send_json_chunk(&tx, &state.opening_delta()).await.is_err() {
            let _ = u_tx.send(state.usage);
            return;
        }

        while let Some(chunk) = stream.next().await {
            match chunk {
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
                        let Ok(payload) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        let outputs = state.handle_event(&payload);
                        for out in outputs {
                            if send_json_chunk(&tx, &out).await.is_err() {
                                let _ = u_tx.send(state.usage);
                                return;
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = tx
                        .send(Err(io::Error::other(format!("上游流错误: {err}"))))
                        .await;
                    let _ = u_tx.send(state.usage);
                    return;
                }
            }
        }
        if !state.finished {
            let _ = tx
                .send(Err(io::Error::other(
                    "Responses 上游流未收到完成事件就已结束",
                )))
                .await;
            let _ = u_tx.send(state.usage);
            return;
        }
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
        let _ = u_tx.send(state.usage);
    });
    (u_rx, ReceiverStream::new(rx))
}

async fn send_json_chunk(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    value: &Value,
) -> Result<(), mpsc::error::SendError<Result<Bytes, io::Error>>> {
    let line = format!("data: {}\n\n", value);
    tx.send(Ok(Bytes::from(line))).await
}

struct StreamState {
    chunk_id: String,
    created: i64,
    model: String,
    request_model: String,
    // output_item index -> openai tool_calls slot
    tool_slots: HashMap<u64, usize>,
    custom_tool_indexes: HashSet<u64>,
    custom_tool_inputs: HashMap<u64, String>,
    custom_arguments_emitted: HashSet<u64>,
    tool_slots_seq: usize,
    usage: TokenUsage,
    finish_reason: Option<String>,
    finished: bool,
}

impl StreamState {
    fn new(request_model: String) -> Self {
        Self {
            chunk_id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            created: Utc::now().timestamp(),
            model: request_model.clone(),
            request_model,
            tool_slots: HashMap::new(),
            custom_tool_indexes: HashSet::new(),
            custom_tool_inputs: HashMap::new(),
            custom_arguments_emitted: HashSet::new(),
            tool_slots_seq: 0,
            usage: TokenUsage::default(),
            finish_reason: None,
            finished: false,
        }
    }

    fn opening_delta(&self) -> Value {
        self.build_chunk(json!({"role": "assistant"}), None)
    }

    fn build_chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.chunk_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        })
    }

    fn build_terminal_chunk(&mut self) -> Value {
        self.finished = true;
        let reason = self.finish_reason.clone().unwrap_or_else(|| {
            if self.tool_slots.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .to_string()
        });
        let mut chunk = self.build_chunk(json!({}), Some(reason.as_str()));
        chunk["usage"] = json!({
            "prompt_tokens": self.usage.input,
            "completion_tokens": self.usage.output,
            "total_tokens": self.usage.input + self.usage.output,
        });
        chunk
    }

    fn handle_event(&mut self, payload: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        match payload.get("type").and_then(Value::as_str) {
            Some("response.created") | Some("response.in_progress") => {
                // 更新 model / id / 初始 usage
                if let Some(resp) = payload.get("response") {
                    if let Some(m) = resp.get("model").and_then(Value::as_str) {
                        if self.model == self.request_model {
                            self.model = m.to_string();
                        }
                    }
                    if let Some(id) = resp.get("id").and_then(Value::as_str) {
                        self.chunk_id = format!(
                            "chatcmpl-{}",
                            id.trim_start_matches("resp_").trim_start_matches("resp-")
                        );
                    }
                    if let Some(u) = resp.get("usage") {
                        self.accumulate_usage(u);
                    }
                }
            }
            Some("response.output_item.added") => {
                let index = payload.get("output_index").and_then(Value::as_u64);
                let item = payload.get("item");
                if let (Some(index), Some(item)) = (index, item) {
                    let item_type = item.get("type").and_then(Value::as_str);
                    if matches!(item_type, Some("function_call") | Some("custom_tool_call")) {
                        let slot = self.tool_slots_seq;
                        self.tool_slots_seq += 1;
                        self.tool_slots.insert(index, slot);
                        if item_type == Some("custom_tool_call") {
                            self.custom_tool_indexes.insert(index);
                            if let Some(input) = item.get("input").and_then(Value::as_str) {
                                self.custom_tool_inputs.insert(index, input.to_string());
                            }
                        }
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let initial_args = if item_type == Some("custom_tool_call") {
                            ""
                        } else {
                            item.get("arguments").and_then(Value::as_str).unwrap_or("")
                        };
                        out.push(self.build_chunk(
                            json!({
                                "tool_calls": [{
                                    "index": slot,
                                    "id": call_id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": initial_args,
                                    }
                                }]
                            }),
                            None,
                        ));
                    }
                }
            }
            Some("response.custom_tool_call_input.delta") => {
                let index = payload.get("output_index").and_then(Value::as_u64);
                let delta = payload.get("delta").and_then(Value::as_str);
                if let (Some(index), Some(delta)) = (index, delta) {
                    self.custom_tool_inputs
                        .entry(index)
                        .or_default()
                        .push_str(delta);
                }
            }
            Some("response.custom_tool_call_input.done") => {
                let index = payload.get("output_index").and_then(Value::as_u64);
                if let Some(index) = index {
                    if let Some(input) = payload.get("input").and_then(Value::as_str) {
                        self.custom_tool_inputs.insert(index, input.to_string());
                    }
                    if let Some(chunk) = self.custom_arguments_chunk(index) {
                        out.push(chunk);
                    }
                }
            }
            Some("response.output_item.done") => {
                let index = payload.get("output_index").and_then(Value::as_u64);
                let item = payload.get("item");
                if let (Some(index), Some(item)) = (index, item) {
                    if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
                        if let Some(input) = item.get("input").and_then(Value::as_str) {
                            self.custom_tool_inputs.insert(index, input.to_string());
                        }
                        if let Some(chunk) = self.custom_arguments_chunk(index) {
                            out.push(chunk);
                        }
                    }
                }
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        out.push(self.build_chunk(json!({"content": delta}), None));
                    }
                }
            }
            Some("response.function_call_arguments.delta") => {
                let index = payload.get("output_index").and_then(Value::as_u64);
                let delta = payload.get("delta").and_then(Value::as_str);
                if let (Some(index), Some(delta)) = (index, delta) {
                    if let Some(slot) = self.tool_slots.get(&index).copied() {
                        out.push(self.build_chunk(
                            json!({
                                "tool_calls": [{
                                    "index": slot,
                                    "function": {"arguments": delta}
                                }]
                            }),
                            None,
                        ));
                    }
                }
            }
            Some("response.function_call_arguments.done") => {
                // 完整值来了：多数上游此前已经发过 delta，一般不需要重发
                // 若之前没有收到 delta（有些实现直接一次给完），补一条
                let index = payload.get("output_index").and_then(Value::as_u64);
                let args = payload.get("arguments").and_then(Value::as_str);
                if let (Some(index), Some(args)) = (index, args) {
                    if let Some(slot) = self.tool_slots.get(&index).copied() {
                        if !args.is_empty() {
                            out.push(self.build_chunk(
                                json!({
                                    "tool_calls": [{
                                        "index": slot,
                                        "function": {"arguments": args}
                                    }]
                                }),
                                None,
                            ));
                        }
                    }
                }
            }
            Some("response.completed") => {
                let pending_custom = self.custom_tool_indexes.iter().copied().collect::<Vec<_>>();
                for index in pending_custom {
                    if let Some(chunk) = self.custom_arguments_chunk(index) {
                        out.push(chunk);
                    }
                }
                if let Some(resp) = payload.get("response") {
                    if let Some(u) = resp.get("usage") {
                        self.accumulate_usage(u);
                    }
                    let status = resp.get("status").and_then(Value::as_str);
                    let incomplete_reason = resp
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str);
                    self.finish_reason = Some(
                        map_finish_reason(status, incomplete_reason, !self.tool_slots.is_empty())
                            .to_string(),
                    );
                }
                if !self.finished {
                    out.push(self.build_terminal_chunk());
                }
            }
            Some("response.failed") | Some("response.incomplete") => {
                if let Some(resp) = payload.get("response") {
                    let status = resp.get("status").and_then(Value::as_str);
                    let incomplete_reason = resp
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str);
                    self.finish_reason = Some(
                        map_finish_reason(status, incomplete_reason, !self.tool_slots.is_empty())
                            .to_string(),
                    );
                    if let Some(u) = resp.get("usage") {
                        self.accumulate_usage(u);
                    }
                }
                if !self.finished {
                    out.push(self.build_terminal_chunk());
                }
            }
            Some("error") => {
                let msg = payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error");
                let error_type = payload
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream_error");
                out.push(json!({
                    "error": {
                        "message": msg,
                        "type": error_type
                    }
                }));
                self.finished = true;
            }
            _ => {}
        }
        out
    }

    fn custom_arguments_chunk(&mut self, index: u64) -> Option<Value> {
        if !self.custom_tool_indexes.contains(&index)
            || !self.custom_arguments_emitted.insert(index)
        {
            return None;
        }
        let slot = self.tool_slots.get(&index).copied()?;
        let input = self
            .custom_tool_inputs
            .get(&index)
            .map(String::as_str)
            .unwrap_or("");
        let arguments = encode_custom_tool_arguments(input);
        Some(self.build_chunk(
            json!({
                "tool_calls": [{
                    "index": slot,
                    "function": {"arguments": arguments}
                }]
            }),
            None,
        ))
    }

    fn accumulate_usage(&mut self, u: &Value) {
        if let Some(v) = u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(Value::as_i64)
        {
            if v > 0 {
                self.usage.input = v;
            }
        }
        if let Some(v) = u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(Value::as_i64)
        {
            if v > 0 {
                self.usage.output = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_extracts_system_to_instructions() {
        let body = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ]
        });
        let out = chat_to_responses_request(&body).unwrap();
        assert_eq!(out["instructions"], "be brief");
        assert_eq!(out["input"][0]["type"], "message");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(out["input"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn request_maps_tool_calls_and_tool_output() {
        let body = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"loc\":\"sh\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ]
        });
        let out = chat_to_responses_request(&body).unwrap();
        let items = out["input"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_1");
        assert_eq!(items[1]["name"], "get_weather");
        assert_eq!(items[1]["arguments"], "{\"loc\":\"sh\"}");
        assert_eq!(items[2]["type"], "function_call_output");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[2]["output"], "sunny");
    }

    #[test]
    fn request_max_tokens_renamed() {
        let body = json!({"model": "gpt-5", "messages": [], "max_tokens": 128});
        let out = chat_to_responses_request(&body).unwrap();
        assert_eq!(out["max_output_tokens"], 128);
        assert!(out.get("max_tokens").is_none());
    }

    #[test]
    fn request_image_url_becomes_input_image() {
        let body = json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "https://x.png"}}
            ]}]
        });
        let out = chat_to_responses_request(&body).unwrap();
        let content = &out["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "https://x.png");
    }

    #[test]
    fn response_maps_text_and_tools() {
        let body = json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_2", "name": "search",
                 "arguments": "{\"q\":\"a\"}"}
            ],
            "usage": {"input_tokens": 4, "output_tokens": 6}
        });
        let out = responses_to_chat_response(body, "gpt-5");
        let choice = &out["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["tool_calls"][0]["id"], "call_2");
        assert_eq!(
            choice["message"]["tool_calls"][0]["function"]["name"],
            "search"
        );
        assert_eq!(out["usage"]["prompt_tokens"], 4);
        assert_eq!(out["usage"]["completion_tokens"], 6);
        assert_eq!(out["usage"]["total_tokens"], 10);
    }

    #[test]
    fn response_maps_custom_tool_calls() {
        let body = json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [{
                "type": "custom_tool_call",
                "call_id": "call_patch",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** End Patch"
            }]
        });

        let out = responses_to_chat_response(body, "gpt-5");
        let call = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(call["id"], "call_patch");
        assert_eq!(call["function"]["name"], "apply_patch");
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap()
                ["input"],
            "*** Begin Patch\n*** End Patch"
        );
    }

    #[test]
    fn stream_state_emits_text_delta() {
        let mut state = StreamState::new("gpt-5".into());
        state.handle_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_x", "model": "gpt-5"}
        }));
        let out = state.handle_event(&json!({
            "type": "response.output_text.delta",
            "delta": "hi"
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "hi");
    }

    #[test]
    fn stream_state_emits_tool_call_deltas() {
        let mut state = StreamState::new("gpt-5".into());
        // added
        let out = state.handle_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_z", "name": "search"}
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_z"
        );
        // arguments delta
        let out = state.handle_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"q\":\""
        }));
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\""
        );
    }

    #[test]
    fn stream_state_emits_custom_tool_input_as_arguments() {
        let mut state = StreamState::new("gpt-5".into());
        let out = state.handle_event(&json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "custom_tool_call",
                "call_id": "call_patch",
                "name": "apply_patch"
            }
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_patch"
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "apply_patch"
        );

        assert!(state
            .handle_event(&json!({
                "type": "response.custom_tool_call_input.delta",
                "output_index": 1,
                "delta": "*** Begin Patch\n"
            }))
            .is_empty());
        let out = state.handle_event(&json!({
            "type": "response.custom_tool_call_input.done",
            "output_index": 1,
            "input": "*** Begin Patch\n*** End Patch"
        }));
        assert_eq!(
            serde_json::from_str::<Value>(
                out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )
            .unwrap()["input"],
            "*** Begin Patch\n*** End Patch"
        );
    }

    #[test]
    fn stream_state_terminal_chunk_includes_usage() {
        let mut state = StreamState::new("gpt-5".into());
        state.handle_event(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {"input_tokens": 3, "output_tokens": 5}
            }
        }));
        assert!(state.finished);
        assert_eq!(state.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn stream_state_error_emits_error_object_not_text_delta() {
        let mut state = StreamState::new("gpt-5".into());
        let out = state.handle_event(&json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": "Concurrency limit exceeded for account, please retry later"
            }
        }));

        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["error"]["type"], "rate_limit_error");
        assert_eq!(
            out[0]["error"]["message"],
            "Concurrency limit exceeded for account, please retry later"
        );
        assert!(out[0]["choices"].is_null());
        assert!(state.finished);
    }
}
