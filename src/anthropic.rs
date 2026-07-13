// OpenAI Chat Completions <-> Anthropic Messages 双向翻译
//
// - openai_to_anthropic_request: 请求体翻译（含 system 抽取、tool 消息合并、tools、多模态）
// - anthropic_to_openai_response: 非流式响应翻译回 OpenAI chat.completion 结构
// - spawn_stream_translator: 流式 SSE 翻译，Anthropic 事件序列 -> OpenAI chunk 序列
//
// 硬编码：Anthropic max_tokens 默认 4096；anthropic-version 由 proxy 侧统一填 2023-06-01

use crate::proxy::{next_sse_event, sse_data, TokenUsage};
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::{collections::HashMap, io};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const DEFAULT_MAX_TOKENS: i64 = 4096;

// ============================== 请求翻译 ==============================

pub fn openai_to_anthropic_request(body: &Value) -> Result<Value, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少 model 字段".to_string())?
        .to_string();

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(arr) = obj.get("messages").and_then(Value::as_array) {
        for msg in arr {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "system" => {
                    if let Some(text) = extract_plain_text(msg.get("content")) {
                        if !text.is_empty() {
                            system_parts.push(text);
                        }
                    }
                }
                "user" => {
                    let content = convert_user_content(msg.get("content"))?;
                    messages.push(json!({"role": "user", "content": content}));
                }
                "assistant" => {
                    let content = convert_assistant_content(msg)?;
                    messages.push(json!({"role": "assistant", "content": content}));
                }
                "tool" => {
                    let tool_call_id = msg
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let result_content = normalize_tool_result_content(msg.get("content"));
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": result_content,
                    });
                    // tool_result 必须挂在 user role 的 content 里；若上一条已是 user，则追加，否则新建
                    let mut appended = false;
                    if let Some(last) = messages.last_mut() {
                        if last.get("role").and_then(Value::as_str) == Some("user") {
                            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut)
                            {
                                arr.push(block.clone());
                                appended = true;
                            }
                        }
                    }
                    if !appended {
                        messages.push(json!({"role": "user", "content": [block]}));
                    }
                }
                _ => {
                    // function / 其它，尽力当 user 处理
                    if let Some(text) = extract_plain_text(msg.get("content")) {
                        if !text.is_empty() {
                            messages.push(json!({
                                "role": "user",
                                "content": [{"type": "text", "text": text}]
                            }));
                        }
                    }
                }
            }
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model));
    out.insert(
        "max_tokens".into(),
        Value::Number(
            obj.get("max_tokens")
                .and_then(Value::as_i64)
                .or_else(|| obj.get("max_completion_tokens").and_then(Value::as_i64))
                .unwrap_or(DEFAULT_MAX_TOKENS)
                .into(),
        ),
    );

    // JSON mode -> system 提示（Anthropic 无原生 JSON mode）
    if let Some(rf) = obj.get("response_format") {
        if rf.get("type").and_then(Value::as_str) == Some("json_object") {
            system_parts.push(
                "You must respond with a single valid JSON object, without any additional text or markdown fences."
                    .to_string(),
            );
        }
    }

    if !system_parts.is_empty() {
        out.insert("system".into(), Value::String(system_parts.join("\n\n")));
    }
    out.insert("messages".into(), Value::Array(messages));

    // 采样参数直接透传
    if let Some(v) = obj.get("temperature") {
        out.insert("temperature".into(), v.clone());
    }
    if let Some(v) = obj.get("top_p") {
        out.insert("top_p".into(), v.clone());
    }
    if let Some(v) = obj.get("top_k") {
        out.insert("top_k".into(), v.clone());
    }
    if let Some(v) = obj.get("stream") {
        out.insert("stream".into(), v.clone());
    }
    // stop -> stop_sequences
    if let Some(stop) = obj.get("stop") {
        let seqs = match stop {
            Value::String(s) => vec![Value::String(s.clone())],
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        if !seqs.is_empty() {
            out.insert("stop_sequences".into(), Value::Array(seqs));
        }
    }
    // metadata.user_id
    if let Some(user) = obj.get("user").and_then(Value::as_str) {
        out.insert("metadata".into(), json!({ "user_id": user }));
    }

    // tools 翻译
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let mut anthropic_tools = Vec::new();
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
            let schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            anthropic_tools.push(json!({
                "name": name,
                "description": description,
                "input_schema": schema,
            }));
        }
        if !anthropic_tools.is_empty() {
            out.insert("tools".into(), Value::Array(anthropic_tools));
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
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            "none" => None,
            _ => None,
        },
        Value::Object(_) => {
            let ty = tc.get("type").and_then(Value::as_str)?;
            if ty == "function" {
                let name = tc.pointer("/function/name").and_then(Value::as_str)?;
                Some(json!({"type": "tool", "name": name}))
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
                if part.get("type").and_then(Value::as_str) == Some("text") {
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
        Some(Value::String(s)) => Ok(json!([{"type": "text", "text": s}])),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        out.push(json!({"type": "text", "text": text}));
                    }
                    Some("image_url") => {
                        let url = part
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "image_url.url 缺失".to_string())?;
                        out.push(convert_image_url(url));
                    }
                    Some("input_audio") => {
                        // Anthropic 目前不支持音频，降级为文本占位
                        out.push(json!({
                            "type": "text",
                            "text": "[audio content omitted]"
                        }));
                    }
                    _ => {
                        // 未知类型，忽略但不报错
                        continue;
                    }
                }
            }
            if out.is_empty() {
                out.push(json!({"type": "text", "text": ""}));
            }
            Ok(Value::Array(out))
        }
        Some(Value::Null) | None => Ok(json!([{"type": "text", "text": ""}])),
        Some(other) => Ok(json!([{"type": "text", "text": other.to_string()}])),
    }
}

fn convert_image_url(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        // data:[media_type];base64,<data>
        if let Some((meta, data)) = rest.split_once(',') {
            let mut media_type = "image/png";
            let is_base64 = meta.contains(";base64");
            if let Some(mt) = meta.split(';').next() {
                if !mt.is_empty() {
                    media_type = mt;
                }
            }
            if is_base64 {
                return json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    }
                });
            }
        }
    }
    json!({
        "type": "image",
        "source": {"type": "url", "url": url}
    })
}

fn convert_assistant_content(msg: &Value) -> Result<Value, String> {
    let mut blocks: Vec<Value> = Vec::new();
    if let Some(text) = msg.get("content").and_then(|c| extract_plain_text(Some(c))) {
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args_raw = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }
    // 兼容旧字段 function_call
    if let Some(fc) = msg.get("function_call") {
        if let Some(name) = fc.get("name").and_then(Value::as_str) {
            let args_raw = fc.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let input: Value = serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": format!("call_{}", Uuid::new_v4().simple()),
                "name": name,
                "input": input,
            }));
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    Ok(Value::Array(blocks))
}

fn normalize_tool_result_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(arr)) => {
            let mut out = Vec::new();
            for part in arr {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        out.push(json!({"type": "text", "text": text}));
                    }
                    Some("image_url") => {
                        let url = part
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.push(convert_image_url(url));
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                Value::String(String::new())
            } else {
                Value::Array(out)
            }
        }
        Some(other) => Value::String(other.to_string()),
        None => Value::String(String::new()),
    }
}

// ============================== 非流式响应翻译 ==============================

pub fn anthropic_to_openai_response(body: Value, request_model: &str) -> Value {
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
    if let Some(arr) = body.get("content").and_then(Value::as_array) {
        for block in arr {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        text_buf.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    let args = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args,
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let stop_reason = body.get("stop_reason").and_then(Value::as_str);
    let finish_reason = map_finish_reason(stop_reason, !tool_calls.is_empty());

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

    let (input_tokens, output_tokens) = extract_anthropic_usage(&body);

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

fn map_finish_reason(stop_reason: Option<&str>, has_tool_calls: bool) -> &'static str {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
    }
}

fn extract_anthropic_usage(body: &Value) -> (i64, i64) {
    let usage = body.get("usage");
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (input, output)
}

// ============================== 流式 SSE 翻译 ==============================

/// 把上游 Anthropic 流式响应转换成 OpenAI chat.completion.chunk 流。
/// 返回：客户端字节流 + 最终 usage oneshot。
pub fn spawn_stream_translator<S>(
    stream: S,
    request_model: String,
) -> (
    oneshot::Receiver<TokenUsage>,
    ReceiverStream<Result<Bytes, io::Error>>,
)
where
    S: futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(64);
    let (u_tx, u_rx) = oneshot::channel::<TokenUsage>();
    tokio::spawn(async move {
        let mut state = StreamState::new(request_model);
        let mut buffer = String::new();
        let mut stream = Box::pin(stream);
        // 首个 chunk 先发 role=assistant，OpenAI 客户端惯例
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
                        if payload.get("type").and_then(Value::as_str) == Some("error") {
                            let message = payload
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("Anthropic upstream error");
                            let error_type = payload
                                .pointer("/error/type")
                                .and_then(Value::as_str)
                                .unwrap_or("upstream_error");
                            let error = json!({
                                "error": {
                                    "message": message,
                                    "type": error_type
                                }
                            });
                            let _ = send_json_chunk(&tx, &error).await;
                            let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                            let _ = u_tx.send(state.usage);
                            return;
                        }
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
                    "Anthropic 上游流未收到 message_stop 就已结束",
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
    // content_block index -> openai tool_calls index
    tool_slots: HashMap<u64, usize>,
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
        let reason = self
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string());
        let mut chunk = self.build_chunk(json!({}), Some(reason.as_str()));
        // 附带 usage，OpenAI include_usage 惯例
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
            Some("message_start") => {
                if let Some(msg) = payload.get("message") {
                    if let Some(m) = msg.get("model").and_then(Value::as_str) {
                        if self.model == self.request_model {
                            self.model = m.to_string();
                        }
                    }
                    if let Some(id) = msg.get("id").and_then(Value::as_str) {
                        // 保留原 chatcmpl- 前缀风格
                        self.chunk_id = format!("chatcmpl-{}", id.trim_start_matches("msg_"));
                    }
                    if let Some(u) = msg.get("usage") {
                        if let Some(v) = u.get("input_tokens").and_then(Value::as_i64) {
                            self.usage.input = v;
                        }
                        if let Some(v) = u.get("output_tokens").and_then(Value::as_i64) {
                            self.usage.output = v;
                        }
                    }
                }
            }
            Some("content_block_start") => {
                let index = payload.get("index").and_then(Value::as_u64);
                let block = payload.get("content_block");
                if let (Some(index), Some(block)) = (index, block) {
                    if let Some("tool_use") = block.get("type").and_then(Value::as_str) {
                        let slot = self.tool_slots_seq;
                        self.tool_slots_seq += 1;
                        self.tool_slots.insert(index, slot);
                        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        out.push(self.build_chunk(
                            json!({
                                "tool_calls": [{
                                    "index": slot,
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": "",
                                    }
                                }]
                            }),
                            None,
                        ));
                    }
                }
            }
            Some("content_block_delta") => {
                let index = payload.get("index").and_then(Value::as_u64);
                let delta = payload.get("delta");
                if let (Some(index), Some(delta)) = (index, delta) {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    out.push(self.build_chunk(json!({"content": t}), None));
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(slot) = self.tool_slots.get(&index).copied() {
                                let partial = delta
                                    .get("partial_json")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                out.push(self.build_chunk(
                                    json!({
                                        "tool_calls": [{
                                            "index": slot,
                                            "function": {"arguments": partial}
                                        }]
                                    }),
                                    None,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("content_block_stop") => {
                // 无输出
            }
            Some("message_delta") => {
                if let Some(delta) = payload.get("delta") {
                    if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                        let has_tools = !self.tool_slots.is_empty();
                        self.finish_reason =
                            Some(map_finish_reason(Some(reason), has_tools).to_string());
                    }
                }
                if let Some(u) = payload.get("usage") {
                    if let Some(v) = u.get("input_tokens").and_then(Value::as_i64) {
                        if v > 0 {
                            self.usage.input = v;
                        }
                    }
                    if let Some(v) = u.get("output_tokens").and_then(Value::as_i64) {
                        if v > 0 {
                            self.usage.output = v;
                        }
                    }
                }
            }
            Some("message_stop") => {
                if !self.finished {
                    out.push(self.build_terminal_chunk());
                }
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_extracts_system_and_maps_max_tokens() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hi"}
            ]
        });
        let translated = openai_to_anthropic_request(&body).unwrap();
        assert_eq!(translated["system"], "you are helpful");
        assert_eq!(translated["max_tokens"], 4096);
        assert_eq!(translated["messages"][0]["role"], "user");
        assert_eq!(translated["messages"][0]["content"][0]["type"], "text");
        assert_eq!(translated["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn request_merges_tool_results_into_user() {
        let body = json!({
            "model": "claude-3",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "toolu_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"loc\":\"sh\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "toolu_1", "content": "sunny"}
            ]
        });
        let out = openai_to_anthropic_request(&body).unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
        // 最后一条应是含 tool_result 的 user
        let last = &out["messages"][2];
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "toolu_1");
        // assistant 的 tool_use.input 是 JSON 对象，不是字符串
        let assistant = &out["messages"][1];
        assert_eq!(assistant["content"][0]["type"], "tool_use");
        assert_eq!(assistant["content"][0]["input"]["loc"], "sh");
    }

    #[test]
    fn request_converts_image_data_uri() {
        let body = json!({
            "model": "claude-3",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {
                    "url": "data:image/jpeg;base64,ABC"
                }}
            ]}]
        });
        let out = openai_to_anthropic_request(&body).unwrap();
        let content = &out["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "ABC");
    }

    #[test]
    fn response_maps_stop_reason_and_tool_calls() {
        let anthropic = json!({
            "id": "msg_01",
            "model": "claude-3",
            "content": [
                {"type": "text", "text": "Sure."},
                {"type": "tool_use", "id": "toolu_9",
                 "name": "get_weather", "input": {"loc": "sh"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 12, "output_tokens": 5}
        });
        let openai = anthropic_to_openai_response(anthropic, "gpt-4o");
        let choice = &openai["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["role"], "assistant");
        assert_eq!(choice["message"]["tool_calls"][0]["id"], "toolu_9");
        assert_eq!(
            choice["message"]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        assert_eq!(openai["usage"]["prompt_tokens"], 12);
        assert_eq!(openai["usage"]["completion_tokens"], 5);
        assert_eq!(openai["usage"]["total_tokens"], 17);
    }

    #[test]
    fn stream_state_produces_role_and_text_chunks() {
        let mut state = StreamState::new("claude-3".into());
        // opening
        let opening = state.opening_delta();
        assert_eq!(opening["choices"][0]["delta"]["role"], "assistant");
        // message_start
        state.handle_event(&json!({
            "type": "message_start",
            "message": {"id": "msg_x", "model": "claude-3", "usage": {"input_tokens": 8, "output_tokens": 0}}
        }));
        assert_eq!(state.usage.input, 8);
        // content_block_start text
        state.handle_event(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }));
        // delta
        let out = state.handle_event(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello"}
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "hello");
        // message_delta -> stop reason
        state.handle_event(&json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 4}
        }));
        assert_eq!(state.usage.output, 4);
        assert_eq!(state.finish_reason.as_deref(), Some("stop"));
        // message_stop -> terminal
        let out = state.handle_event(&json!({"type": "message_stop"}));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(out[0]["usage"]["total_tokens"], 12);
    }

    #[test]
    fn stream_state_handles_tool_use_deltas() {
        let mut state = StreamState::new("claude-3".into());
        state.handle_event(&json!({
            "type": "message_start",
            "message": {"id": "msg_y", "model": "claude-3", "usage": {"input_tokens": 5, "output_tokens": 0}}
        }));
        // 工具块开始 -> 分配 slot 0
        let out = state.handle_event(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_a", "name": "search"}
        }));
        assert_eq!(out.len(), 1);
        let tc = &out[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "toolu_a");
        assert_eq!(tc["function"]["name"], "search");
        // 部分 arguments
        let out = state.handle_event(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"q\":\""}
        }));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\""
        );
    }

    #[tokio::test]
    async fn stream_translator_emits_error_chunk_without_assistant_text() {
        let event = json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": "Concurrency limit exceeded for account, please retry later"
            }
        });
        let bytes = Bytes::from(format!("data: {event}\n\n"));
        let input = futures_util::stream::iter([Ok::<Bytes, io::Error>(bytes)]);
        let (_usage_rx, mut stream) = spawn_stream_translator(input, "claude-test".to_string());
        let mut out = String::new();

        while let Some(chunk) = stream.next().await {
            out.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert!(out.contains("\"error\""));
        assert!(out.contains("Concurrency limit exceeded"));
        assert!(!out.contains("[stream error:"));
    }
}
