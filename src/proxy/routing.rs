use super::*;

pub(super) fn provider_attempts(
    cfg: &AppConfig,
    state: &AppState,
    model: &str,
    api: &str,
    allowed_providers: &[String],
) -> Vec<(ProviderConfig, String)> {
    let mut attempts = vec![model.to_string()];
    if let Some(fallbacks) = cfg.routing.model_fallbacks.get(model) {
        for item in fallbacks {
            if !attempts.contains(item) {
                attempts.push(item.clone());
            }
        }
    }

    let mut out = Vec::new();
    for request_model in attempts {
        let mut providers: Vec<ProviderConfig> = cfg
            .providers
            .iter()
            .filter(|p| {
                p.enabled
                    && api_key_allows_provider(allowed_providers, &p.name)
                    && provider_supports_model(p, &request_model)
                    && provider_supports_api(p, api)
            })
            .cloned()
            .collect();
        providers.extend(
            cfg.auth_accounts
                .iter()
                .filter(|account| account.enabled)
                .map(|account| auth_account_as_provider(account, &cfg.auth))
                .filter(|p| {
                    api_key_allows_provider(allowed_providers, &p.name)
                        && provider_supports_model(p, &request_model)
                        && provider_supports_api(p, api)
                }),
        );
        let auth_first = cfg.routing.auth_preference == "auth_first";
        let (preferred, secondary): (Vec<_>, Vec<_>) = providers
            .into_iter()
            .partition(|provider| provider.auth_account_id.is_some() == auth_first);
        for group in [preferred, secondary] {
            let ordered = order_by_priority(state, &request_model, group);
            out.extend(ordered.into_iter().map(|p| (p, request_model.clone())));
        }
    }
    out
}

fn order_by_priority(
    state: &AppState,
    model: &str,
    mut providers: Vec<ProviderConfig>,
) -> Vec<ProviderConfig> {
    providers.sort_by_key(|provider| (provider.priority, provider.name.clone()));
    let mut by_priority: HashMap<i32, Vec<ProviderConfig>> = HashMap::new();
    for provider in providers {
        by_priority
            .entry(provider.priority)
            .or_default()
            .push(provider);
    }
    let mut priorities: Vec<_> = by_priority.keys().copied().collect();
    priorities.sort();
    let mut ordered = Vec::new();
    for priority in priorities {
        ordered.extend(weighted_order(
            state,
            model,
            by_priority.remove(&priority).unwrap_or_default(),
        ));
    }
    ordered
}

pub(super) fn weighted_order(
    state: &AppState,
    model: &str,
    providers: Vec<ProviderConfig>,
) -> Vec<ProviderConfig> {
    let mut expanded = Vec::new();
    for provider in providers {
        for _ in 0..provider.weight.max(1) {
            expanded.push(provider.clone());
        }
    }
    if expanded.is_empty() {
        return expanded;
    }
    let counter = {
        let mut counters = state.counters.lock();
        counters
            .entry(model.to_string())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    };
    let start = counter.fetch_add(1, Ordering::Relaxed) % expanded.len();
    expanded.rotate_left(start);
    let mut seen = HashSet::new();
    let mut ordered = expanded
        .into_iter()
        .filter(|provider| seen.insert(provider.name.clone()))
        .map(|provider| {
            let health_rank = provider_health_rank(state, &provider.name);
            let inflight = provider_inflight(state, &provider.name);
            (provider, health_rank, inflight)
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|(a, a_health, a_inflight), (b, b_health, b_inflight)| {
        a_health.cmp(b_health).then_with(|| {
            let a_weight = a.weight.max(1) as u128;
            let b_weight = b.weight.max(1) as u128;
            ((*a_inflight as u128) * b_weight).cmp(&((*b_inflight as u128) * a_weight))
        })
    });
    ordered
        .into_iter()
        .map(|(provider, _, _)| provider)
        .collect()
}

pub(super) fn provider_health_rank(state: &AppState, provider: &str) -> u8 {
    state
        .provider_statuses
        .read()
        .get(provider)
        .map(|status| match status.status {
            ProviderCircuitStatus::Healthy => {
                if status.failures == 0 {
                    0
                } else {
                    1
                }
            }
            ProviderCircuitStatus::HalfOpen => 2,
            ProviderCircuitStatus::Open => 3,
        })
        .unwrap_or(0)
}

pub(super) fn provider_inflight(state: &AppState, provider: &str) -> usize {
    state
        .provider_loads
        .lock()
        .get(provider)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub(super) fn provider_supports_model(provider: &ProviderConfig, model: &str) -> bool {
    let normalized = normalize_model(model);
    provider
        .models
        .iter()
        .any(|m| normalize_model(m) == normalized)
        || provider
            .model_mapping
            .keys()
            .any(|m| normalize_model(m) == normalized)
}

/// Google Gemini 提供的 OpenAI 兼容端点。识别规则与 codeProxyHub 完全一致：
/// URL 同时包含 `generativelanguage.googleapis.com` 与 `/openai`。
///
/// 这个端点只支持 Chat Completions，不支持 Responses API，因此 responses 请求
/// 打到这类 provider 时必须强制走 chat 翻译（force_chat=true）。
pub(crate) fn is_google_openai_endpoint(base_url: &str) -> bool {
    let lowered = base_url.to_ascii_lowercase();
    lowered.contains("generativelanguage.googleapis.com") && lowered.contains("/openai")
}

pub(super) fn provider_supports_api(provider: &ProviderConfig, api: &str) -> bool {
    let supports_chat = !matches!(provider.capabilities.get("supports_chat"), Some(false));
    let supports_responses =
        !matches!(provider.capabilities.get("supports_responses"), Some(false));

    match api {
        // Anthropic providers are translated through /messages, so their native
        // supports_chat capability does not apply to the proxy-facing API.
        "chat" => provider.provider_type == "anthropic" || supports_chat,
        "responses" => {
            if provider.provider_type == "anthropic" {
                return true;
            }
            match provider.responses_mode.as_str() {
                // 显式指定 chat 代表用户要求将 Responses 请求转换到
                // /chat/completions，不应再被过期的 supports_chat:false 提前过滤。
                "chat" => true,
                "native" => supports_responses,
                _ => supports_responses || supports_chat,
            }
        }
        _ => true,
    }
}

pub(super) fn collect_models(config: &AppConfig, allowed_providers: &[String]) -> Vec<String> {
    let mut models = Vec::new();
    let providers = config
        .providers
        .iter()
        .filter(|p| p.enabled && api_key_allows_provider(allowed_providers, &p.name))
        .cloned()
        .chain(
            config
                .auth_accounts
                .iter()
                .filter(|account| account.enabled)
                .map(|account| auth_account_as_provider(account, &config.auth))
                .filter(|p| api_key_allows_provider(allowed_providers, &p.name)),
        );
    for provider in providers {
        for model in &provider.models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        for model in provider.model_mapping.keys() {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
    }
    models.sort();
    models
}

pub(super) fn body_model(body: &Value) -> Result<String, ProxyError> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ProxyError::new(StatusCode::BAD_REQUEST, "请求体缺少 model"))
}

pub(super) fn with_model(mut body: Value, model: &str) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
    }
    body
}

pub(super) fn apply_model_mapping(provider: &ProviderConfig, body: &mut Value) {
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return;
    };
    let normalized = normalize_model(model);
    let mapped = provider
        .model_mapping
        .get(model)
        .cloned()
        .or_else(|| {
            provider
                .model_mapping
                .iter()
                .find(|(k, _)| normalize_model(k) == normalized)
                .map(|(_, v)| v.clone())
        })
        .or_else(|| {
            provider
                .models
                .iter()
                .find(|m| normalize_model(m) == normalized)
                .cloned()
        });
    if let (Some(obj), Some(mapped)) = (body.as_object_mut(), mapped) {
        obj.insert("model".to_string(), Value::String(mapped));
    }
}

pub(super) fn apply_system_prompt_override(provider: &ProviderConfig, body: &mut Value) {
    let Some(prompt) = provider
        .system_prompt_override
        .as_ref()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(messages) = obj.get("messages").and_then(Value::as_array) else {
        return;
    };
    let mut next = vec![json!({"role": "system", "content": prompt})];
    next.extend(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
            .cloned(),
    );
    obj.insert("messages".to_string(), Value::Array(next));
}

pub(super) fn responses_to_chat_body(body: &Value) -> Result<Value, ProxyError> {
    let mut obj = Map::new();
    obj.insert(
        "model".to_string(),
        body.get("model")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    obj.insert(
        "messages".to_string(),
        Value::Array(responses_input_to_messages(body.get("input"))),
    );
    for key in [
        "temperature",
        "top_p",
        "max_tokens",
        "max_output_tokens",
        "stream",
    ] {
        if let Some(value) = body.get(key) {
            let target = if key == "max_output_tokens" {
                "max_tokens"
            } else {
                key
            };
            obj.insert(target.to_string(), value.clone());
        }
    }
    if let Some(tools) = responses_tools_to_chat_tools(body.get("tools")) {
        obj.insert("tools".to_string(), tools);
    }
    if let Some(tool_choice) = responses_tool_choice_to_chat(body.get("tool_choice")) {
        obj.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if let Some(Value::Array(messages)) = obj.get_mut("messages") {
            messages.insert(0, json!({"role": "system", "content": instructions}));
        }
    }
    Ok(Value::Object(obj))
}

pub(super) fn responses_input_to_messages(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(items)) => {
            let mut messages = Vec::new();
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") | None => {
                        let role = match item.get("role").and_then(Value::as_str).unwrap_or("user")
                        {
                            "developer" | "system" => "system",
                            "assistant" => "assistant",
                            _ => "user",
                        };
                        let content = responses_message_content_to_chat(role, item.get("content"));
                        messages.push(json!({"role": role, "content": content}));
                    }
                    Some("function_call") | Some("custom_tool_call") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let arguments = if item.get("type").and_then(Value::as_str)
                            == Some("custom_tool_call")
                        {
                            custom_tool_arguments(
                                item.get("input").and_then(Value::as_str).unwrap_or(""),
                            )
                        } else {
                            item.get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string()
                        };
                        messages.push(json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": call_id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments}
                            }]
                        }));
                    }
                    Some("function_call_output") | Some("custom_tool_call_output") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let output = item
                            .get("output")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                            .unwrap_or_else(|| {
                                item.get("output").map(Value::to_string).unwrap_or_default()
                            });
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": output
                        }));
                    }
                    Some("reasoning") => {}
                    _ => {
                        let text = item
                            .get("content")
                            .and_then(|c| responses_text_content(c, "user"))
                            .unwrap_or_default();
                        if !text.is_empty() {
                            messages.push(json!({"role": "user", "content": text}));
                        }
                    }
                }
            }
            if messages.is_empty() {
                messages.push(json!({"role": "user", "content": ""}));
            }
            messages
        }
        _ => vec![json!({"role": "user", "content": ""})],
    }
}

pub(super) fn responses_message_content_to_chat(role: &str, content: Option<&Value>) -> Value {
    let Some(content) = content else {
        return Value::String(String::new());
    };
    if role == "assistant" || role == "system" {
        return Value::String(responses_text_content(content, role).unwrap_or_default());
    }
    match content {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        out.push(json!({"type": "text", "text": text}));
                    }
                    Some("input_image") => {
                        if let Some(url) = part
                            .get("image_url")
                            .or_else(|| part.get("file_id"))
                            .and_then(Value::as_str)
                        {
                            out.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
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
        other => Value::String(other.to_string()),
    }
}

pub(super) fn responses_text_content(content: &Value, role: &str) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let ty = part.get("type").and_then(Value::as_str);
                let wanted = if role == "assistant" {
                    matches!(ty, Some("output_text") | Some("text"))
                } else {
                    matches!(ty, Some("input_text") | Some("output_text") | Some("text"))
                };
                if wanted {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
            Some(text)
        }
        other => Some(other.to_string()),
    }
}

pub(super) fn responses_tools_to_chat_tools(tools: Option<&Value>) -> Option<Value> {
    let tools = tools?.as_array()?;
    let out = tools
        .iter()
        .filter_map(|tool| match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                if tool.get("function").is_some() {
                    return Some(tool.clone());
                }
                let name = tool.get("name").and_then(Value::as_str)?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let parameters = tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters
                    }
                }))
            }
            Some("custom") => {
                let name = tool.get("name").and_then(Value::as_str)?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "input": {
                                    "type": "string",
                                    "description": "Raw input for this custom tool"
                                }
                            },
                            "required": ["input"],
                            "additionalProperties": false
                        }
                    }
                }))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

pub(super) fn responses_custom_tool_names(tools: Option<&Value>) -> HashSet<String> {
    tools
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn custom_tool_arguments(input: &str) -> String {
    serde_json::to_string(&json!({"input": input}))
        .unwrap_or_else(|_| "{\"input\":\"\"}".to_string())
}

pub(super) fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| arguments.to_string())
}

pub(super) fn responses_tool_choice_to_chat(tool_choice: Option<&Value>) -> Option<Value> {
    match tool_choice? {
        Value::String(s) => Some(Value::String(s.clone())),
        Value::Object(_) => {
            if matches!(
                tool_choice?.get("type").and_then(Value::as_str),
                Some("function") | Some("custom")
            ) {
                let name = tool_choice?.get("name").and_then(Value::as_str)?;
                Some(json!({"type": "function", "function": {"name": name}}))
            } else {
                Some(tool_choice?.clone())
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod preference_tests {
    use super::*;
    use crate::config::{AuthAccountConfig, RoutingConfig};
    use parking_lot::{Mutex, RwLock};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(RwLock::new(Arc::new(
                AppConfig::load("config.example.yaml").unwrap(),
            ))),
            clients: Arc::new(Mutex::new(HashMap::new())),
            counters: Arc::new(Mutex::new(HashMap::new())),
            keepalive_headers: Arc::new(Mutex::new(HashMap::new())),
            api_key_limiters: Arc::new(Mutex::new(HashMap::new())),
            provider_circuits: Arc::new(Mutex::new(HashMap::new())),
            provider_loads: Arc::new(Mutex::new(HashMap::new())),
            provider_statuses: Arc::new(RwLock::new(HashMap::new())),
            session_affinity: Arc::new(Mutex::new(HashMap::new())),
            usage_injection: Arc::new(Mutex::new(HashMap::new())),
            oauth_tokens: Arc::new(Mutex::new(HashMap::new())),
            oauth_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn sample_provider(name: &str, priority: i32) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            enabled: true,
            provider_type: "openai".to_string(),
            base_url: "https://example.test/v1".to_string(),
            website: None,
            api_key: "sk".to_string(),
            models: vec!["gpt-test".to_string()],
            model_mapping: HashMap::new(),
            extra_headers: HashMap::new(),
            capabilities: HashMap::from([
                ("supports_chat".to_string(), true),
                ("supports_responses".to_string(), true),
            ]),
            health_check_mode: "none".to_string(),
            model_sync_filter: "all".to_string(),
            responses_mode: "auto".to_string(),
            client_mode: "normal".to_string(),
            connect_timeout: 10,
            request_timeout: 30,
            stream_idle_timeout: 0,
            stream_max_duration: 0,
            debug_capture_sse: false,
            debug_sse_path: "logs/raw_sse".to_string(),
            debug_sse_max_events: 80,
            max_retries: 1,
            weight: 1,
            priority,
            description: None,
            system_prompt_override: None,
            strip_thought: false,
            persistent_session: false,
            persist_interval: 3.0,
            persist_max_wait: 0,
            persist_keepalive: false,
            persist_keepalive_interval: 30,
            persist_keepalive_model: None,
            persist_keepalive_prompt: "Hi".to_string(),
            extra: Default::default(),
            auth_account_id: None,
        }
    }

    fn sample_account(id: &str, priority: i32) -> AuthAccountConfig {
        AuthAccountConfig {
            id: id.to_string(),
            account_type: "openai".to_string(),
            name: id.to_string(),
            enabled: true,
            email: None,
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            account_id: Some(format!("chatgpt-{id}")),
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
            token_url: "https://auth.openai.com/oauth/token".to_string(),
            base_url: String::new(),
            expires_at: None,
            models: vec!["gpt-test".to_string()],
            model_mapping: HashMap::new(),
            weight: 1,
            priority,
            description: None,
        }
    }

    fn sample_config(preference: &str) -> AppConfig {
        let mut cfg = AppConfig::load("config.example.yaml").unwrap();
        cfg.routing = RoutingConfig {
            model_fallbacks: HashMap::new(),
            auth_preference: preference.to_string(),
            auth_proxy: String::new(),
        };
        cfg.auth.openai_models = vec!["gpt-test".to_string()];
        cfg.providers = vec![sample_provider("channel-a", 1)];
        cfg.auth_accounts = vec![sample_account("acct-b", 5)];
        cfg
    }

    #[test]
    fn auth_preference_orders_accounts_before_providers() {
        let state = test_state();
        let cfg = sample_config("auth_first");
        let names: Vec<_> = provider_attempts(&cfg, &state, "gpt-test", "responses", &[])
            .into_iter()
            .map(|(p, _)| p.name)
            .collect();
        assert_eq!(
            names,
            vec!["auth:acct-b".to_string(), "channel-a".to_string()]
        );
    }

    #[test]
    fn provider_preference_orders_providers_before_accounts() {
        let state = test_state();
        let cfg = sample_config("provider_first");
        let names: Vec<_> = provider_attempts(&cfg, &state, "gpt-test", "responses", &[])
            .into_iter()
            .map(|(p, _)| p.name)
            .collect();
        assert_eq!(
            names,
            vec!["channel-a".to_string(), "auth:acct-b".to_string()]
        );
    }
}
