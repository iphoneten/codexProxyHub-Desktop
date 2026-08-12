// 代理侧响应缓存。
//
// 命中条件：同一个（接口 + 模型 + 归一化请求体 + 流式标志 [+ API Key]）再次到来，
// 且上一次的成功响应还没过期。命中时不打上游、不选渠道、不占并发额度。
//
// 只缓存成功响应。流式响应先原样透传给客户端，同时旁路累积字节，
// 等流正常收尾（StreamOutcome 无 error）后整段落缓存；中途出错的流不落。
//
// 存内存不落盘：响应体里通常是用户对话内容，落盘等于多出一份需要单独管理的明文副本。

use super::*;
use sha2::{Digest, Sha256};

/// 缓存条目。响应头只留重建响应必需的那几个。
#[derive(Clone)]
pub(super) struct CachedResponse {
    pub(super) body: Bytes,
    pub(super) content_type: String,
    pub(super) stream: bool,
    /// 产出这份响应的渠道名，用于命中时的 allowed_providers 校验与日志展示
    pub(super) provider: String,
    pub(super) upstream_model: String,
    pub(super) usage: TokenUsage,
    stored_at: Instant,
}

impl CachedResponse {
    fn expired(&self, ttl_seconds: u64) -> bool {
        if ttl_seconds == 0 {
            return false;
        }
        self.stored_at.elapsed() >= Duration::from_secs(ttl_seconds)
    }

    /// 把缓存条目重建成一个可返回的 Response。
    /// 流式条目按原 SSE 字节整段重放——客户端拿到的事件序列与首次请求完全一致。
    pub(super) fn to_response(&self) -> Response {
        let content_type = if self.content_type.is_empty() {
            if self.stream {
                "text/event-stream".to_string()
            } else {
                "application/json".to_string()
            }
        } else {
            self.content_type.clone()
        };
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            // 让客户端/排查者能一眼看出这是缓存命中
            .header("x-routehub-cache", "hit");
        if self.stream {
            builder = builder.header(header::CACHE_CONTROL, "no-cache");
        }
        builder
            .body(Body::from(self.body.clone()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

/// 进程内 TTL + 容量上限的响应缓存。容量满时按插入顺序淘汰最旧条目。
#[derive(Default)]
pub(super) struct ResponseCache {
    entries: HashMap<String, CachedResponse>,
    /// 插入顺序，用于容量淘汰
    order: VecDeque<String>,
}

impl ResponseCache {
    pub(super) fn get(&mut self, key: &str, ttl_seconds: u64) -> Option<CachedResponse> {
        let entry = self.entries.get(key)?;
        if entry.expired(ttl_seconds) {
            // 过期条目顺手清掉，避免长期占着容量
            self.entries.remove(key);
            self.order.retain(|item| item != key);
            return None;
        }
        Some(entry.clone())
    }

    pub(super) fn insert(&mut self, key: String, value: CachedResponse, max_entries: usize) {
        if max_entries == 0 {
            return;
        }
        if self.entries.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    /// 清理所有已过期条目。配置热切后由调用侧择机触发。
    pub(super) fn purge_expired(&mut self, ttl_seconds: u64) {
        if ttl_seconds == 0 {
            return;
        }
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.expired(ttl_seconds))
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.entries.remove(&key);
            self.order.retain(|item| item != &key);
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 计算请求指纹。
///
/// 关键点：`stream` 必须进 key。流式与非流式的响应体格式不同（SSE vs JSON），
/// 拿非流式缓存去回流式请求会直接把客户端的 SSE 解析器喂坏。
///
/// `stream_options` 不进 key：它只影响上游是否附带 usage，不改变对话语义。
pub(super) fn cache_key(
    api: &str,
    model: &str,
    body: &Value,
    stream: bool,
    api_key_id: &str,
    isolate_by_api_key: bool,
) -> String {
    let mut normalized = body.clone();
    if let Some(obj) = normalized.as_object_mut() {
        obj.remove("stream");
        obj.remove("stream_options");
    }
    // serde_json 的 Map 默认按插入顺序序列化，同义请求可能因字段顺序不同产生不同指纹。
    // 这里做一次递归的键排序，让指纹只取决于内容。
    let canonical = canonicalize(&normalized);

    let mut hasher = Sha256::new();
    hasher.update(api.as_bytes());
    hasher.update([0u8]);
    hasher.update(model.as_bytes());
    hasher.update([0u8]);
    hasher.update(if stream { b"stream" as &[u8] } else { b"unary" });
    hasher.update([0u8]);
    if isolate_by_api_key {
        hasher.update(api_key_id.as_bytes());
    }
    hasher.update([0u8]);
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 递归按键名排序后序列化，消除字段顺序带来的指纹差异。
/// 数组顺序保留——messages 的顺序是有语义的。
fn canonicalize(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}:{}", key, canonicalize(&map[key])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonicalize).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// 客户端能否要求跳过缓存。`x-routehub-cache: no-store` / `no-cache` 时本次不读不写，
/// 用于排查「是不是缓存回了旧结果」。
pub(super) fn client_bypasses_cache(headers: &HeaderMap) -> bool {
    headers
        .get("x-routehub-cache")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            let lower = value.trim().to_ascii_lowercase();
            lower == "no-store" || lower == "no-cache" || lower == "bypass"
        })
        .unwrap_or(false)
}

/// 本次请求要不要走缓存。
pub(super) fn cache_enabled_for(cfg: &AppConfig, api: &str, headers: &HeaderMap) -> bool {
    cfg.response_cache.enabled
        && cfg.response_cache.covers_api(api)
        && !client_bypasses_cache(headers)
}

/// 把响应体收集成 Bytes 用于落缓存，同时返回一个可继续下发给客户端的 Response。
/// 非流式响应体已经完整在内存里，收集不引入额外等待。
pub(super) async fn buffer_response_for_cache(
    response: Response,
    max_body_bytes: usize,
) -> (Response, Option<Bytes>) {
    let (parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, max_body_bytes.max(1)).await else {
        // 超过上限或读取失败：不缓存，但响应已被消费，只能回一个错误
        return (
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": "响应体读取失败或超过缓存上限",
                        "type": "proxy_error"
                    }
                })),
            )
                .into_response(),
            None,
        );
    };
    let cacheable = bytes.len() <= max_body_bytes;
    let response = Response::from_parts(parts, Body::from(bytes.clone()));
    (response, cacheable.then_some(bytes))
}

/// 流式响应：原样透传给客户端，同时旁路累积字节。
/// 返回累积结果的接收端——只有流正常收尾时才应该落缓存，
/// 所以真正的 insert 由调用侧在拿到 StreamOutcome 后决定。
pub(super) fn tee_stream_for_cache(
    response: Response,
    max_body_bytes: usize,
) -> (Response, oneshot::Receiver<Option<Bytes>>) {
    let (parts, body) = response.into_parts();
    let (tx, rx) = oneshot::channel::<Option<Bytes>>();
    let mut buffer = Vec::new();
    let mut overflow = false;
    let mut sender = Some(tx);

    let stream = body.into_data_stream().map(move |item| {
        match &item {
            Ok(chunk) => {
                if !overflow {
                    if buffer.len() + chunk.len() > max_body_bytes {
                        // 超限就放弃缓存这一条，但不影响透传
                        overflow = true;
                        buffer = Vec::new();
                    } else {
                        buffer.extend_from_slice(chunk);
                    }
                }
            }
            Err(_) => {
                overflow = true;
                buffer = Vec::new();
            }
        }
        item
    });

    // 流被读完（或提前 drop）时把累积结果发出去
    let stream = stream.chain(futures_util::stream::poll_fn(move |_| {
        if let Some(tx) = sender.take() {
            let _ = tx.send(None);
        }
        std::task::Poll::Ready(None)
    }));

    let response = Response::from_parts(parts, Body::from_stream(stream));
    (response, rx)
}
