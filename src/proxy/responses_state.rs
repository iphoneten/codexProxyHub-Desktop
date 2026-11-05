use super::*;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SESSIONS: usize = 256;
const MAX_IDENTITIES_PER_SESSION: usize = 4;
const MAX_REJECTED_PER_IDENTITY: usize = 128;
const SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
const PERSIST_REFRESH_SECONDS: i64 = 5 * 60;
const PENDING_TTL_SECONDS: i64 = 5 * 60;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ResponsesSessionBinding {
    pub provider: String,
    pub request_model: String,
    pub provider_identity: String,
    pub updated_at: i64,
    #[serde(default)]
    rejected: BTreeMap<String, Vec<String>>,
}

#[derive(Default)]
pub(super) struct ResponsesStateStore {
    path: Option<PathBuf>,
    entries: Mutex<HashMap<String, ResponsesSessionBinding>>,
    io_lock: Mutex<()>,
    persist_pending: AtomicBool,
    persist_dirty: AtomicBool,
}

#[derive(Serialize, Deserialize)]
struct StoredResponsesState {
    version: u32,
    sessions: BTreeMap<String, ResponsesSessionBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ResponsesAffinityKey {
    pub key: String,
    pub persistent: bool,
}

impl AppState {
    pub(super) fn responses_binding(&self, key: &str) -> Option<ResponsesSessionBinding> {
        self.responses_state.binding(key)
    }

    pub(super) fn rejected_responses_state(
        &self,
        key: &str,
        provider_identity: &str,
    ) -> HashSet<String> {
        self.responses_state.rejected(key, provider_identity)
    }

    pub(super) fn remember_responses_binding(
        &self,
        key: &str,
        provider: &str,
        request_model: &str,
        provider_identity: &str,
        expected_owner_identity: Option<&str>,
    ) {
        if self.responses_state.remember_binding(
            key,
            provider,
            request_model,
            provider_identity,
            expected_owner_identity,
        ) {
            persist_responses_state(self);
        }
    }

    pub(super) fn remember_rejected_responses_state(
        &self,
        key: &str,
        provider_identity: &str,
        fingerprints: impl IntoIterator<Item = String>,
    ) {
        if self
            .responses_state
            .remember_rejected(key, provider_identity, fingerprints)
        {
            persist_responses_state(self);
        }
    }
}

impl ResponsesStateStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            entries: Mutex::new(HashMap::new()),
            io_lock: Mutex::new(()),
            persist_pending: AtomicBool::new(false),
            persist_dirty: AtomicBool::new(false),
        }
    }

    pub(super) fn binding(&self, key: &str) -> Option<ResponsesSessionBinding> {
        if key.is_empty() {
            return None;
        }
        let now = Utc::now().timestamp();
        let mut entries = self.entries.lock();
        if entries
            .get(key)
            .is_some_and(|entry| is_expired(entry.updated_at, now))
        {
            entries.remove(key);
            return None;
        }
        entries.get(key).cloned()
    }

    pub(super) fn rejected(&self, key: &str, provider_identity: &str) -> HashSet<String> {
        self.binding(key)
            .and_then(|entry| entry.rejected.get(provider_identity).cloned())
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    pub(super) fn remember_binding(
        &self,
        key: &str,
        provider: &str,
        request_model: &str,
        provider_identity: &str,
        expected_owner_identity: Option<&str>,
    ) -> bool {
        if !valid_session_key(key)
            || provider.trim().is_empty()
            || request_model.trim().is_empty()
            || !valid_digest(provider_identity)
        {
            return false;
        }
        let now = Utc::now().timestamp();
        let mut entries = self.entries.lock();
        let may_update = match entries.get(key) {
            None => expected_owner_identity.is_none(),
            Some(entry) if entry.provider_identity.is_empty() => expected_owner_identity.is_none(),
            Some(entry)
                if entry.provider_identity == provider_identity && entry.provider == provider =>
            {
                true
            }
            Some(entry) => {
                expected_owner_identity.is_some_and(|expected| entry.provider_identity == expected)
            }
        };
        if !may_update {
            return false;
        }
        let entry = entries.entry(key.to_string()).or_default();
        let changed = entry.provider != provider
            || entry.request_model != request_model
            || entry.provider_identity != provider_identity;
        let refresh_due = now.saturating_sub(entry.updated_at) >= PERSIST_REFRESH_SECONDS;
        entry.provider = provider.to_string();
        entry.request_model = request_model.to_string();
        entry.provider_identity = provider_identity.to_string();
        entry.updated_at = now;
        prune_entries(&mut entries, now);
        changed || refresh_due
    }

    pub(super) fn remember_rejected(
        &self,
        key: &str,
        provider_identity: &str,
        fingerprints: impl IntoIterator<Item = String>,
    ) -> bool {
        if !valid_session_key(key) || !valid_digest(provider_identity) {
            return false;
        }
        let fingerprints = fingerprints
            .into_iter()
            .filter(|value| valid_digest(value))
            .collect::<Vec<_>>();
        if fingerprints.is_empty() {
            return false;
        }

        let now = Utc::now().timestamp();
        let mut entries = self.entries.lock();
        let entry = entries.entry(key.to_string()).or_default();
        entry.updated_at = now;
        let rejected = entry
            .rejected
            .entry(provider_identity.to_string())
            .or_default();
        let mut changed = false;
        for fingerprint in fingerprints {
            if !rejected.contains(&fingerprint) {
                rejected.push(fingerprint);
                changed = true;
            }
        }
        if rejected.len() > MAX_REJECTED_PER_IDENTITY {
            rejected.drain(..rejected.len() - MAX_REJECTED_PER_IDENTITY);
        }
        while entry.rejected.len() > MAX_IDENTITIES_PER_SESSION {
            let Some(oldest) = entry.rejected.keys().next().cloned() else {
                break;
            };
            if oldest == provider_identity && entry.rejected.len() > 1 {
                let next = entry
                    .rejected
                    .keys()
                    .find(|identity| identity.as_str() != provider_identity)
                    .cloned();
                if let Some(next) = next {
                    entry.rejected.remove(&next);
                    continue;
                }
            }
            entry.rejected.remove(&oldest);
        }
        prune_entries(&mut entries, now);
        changed
    }

    fn save(&self) -> io::Result<bool> {
        let Some(path) = self.path.as_deref() else {
            return Ok(false);
        };
        let now = Utc::now().timestamp();
        let sessions = {
            let mut entries = self.entries.lock();
            prune_entries(&mut entries, now);
            entries
                .iter()
                .filter(|(key, value)| {
                    valid_session_key(key)
                        && !value.provider.is_empty()
                        && !value.request_model.is_empty()
                        && valid_digest(&value.provider_identity)
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        write_state_file(
            path,
            &StoredResponsesState {
                version: FILE_VERSION,
                sessions,
            },
        )?;
        Ok(true)
    }

    fn load(&self) -> io::Result<usize> {
        let Some(path) = self.path.as_deref() else {
            return Ok(0);
        };
        let Some(stored) = read_state_file(path)? else {
            return Ok(0);
        };
        if stored.version != FILE_VERSION {
            return Err(invalid_file());
        }
        let now = Utc::now().timestamp();
        let mut restored = HashMap::new();
        for (key, mut entry) in stored.sessions {
            if !valid_session_key(&key)
                || entry.provider.is_empty()
                || entry.request_model.is_empty()
                || !valid_digest(&entry.provider_identity)
                || is_expired(entry.updated_at, now)
            {
                continue;
            }
            entry.rejected.retain(|identity, fingerprints| {
                if !valid_digest(identity) {
                    return false;
                }
                fingerprints.retain(|value| valid_digest(value));
                if fingerprints.len() > MAX_REJECTED_PER_IDENTITY {
                    fingerprints.drain(..fingerprints.len() - MAX_REJECTED_PER_IDENTITY);
                }
                !fingerprints.is_empty()
            });
            restored.insert(key, entry);
        }
        prune_entries(&mut restored, now);
        let count = restored.len();
        *self.entries.lock() = restored;
        Ok(count)
    }
}

pub(super) async fn restore_responses_state(state: &AppState) {
    let store = Arc::clone(&state.responses_state);
    match tokio::task::spawn_blocking(move || {
        let _guard = store.io_lock.lock();
        store.load()
    })
    .await
    {
        Ok(Ok(count)) if count > 0 => {
            crate::runtime_log::record("INFO", format!("已恢复 {count} 个 Responses 会话路由绑定"))
        }
        Ok(Ok(_)) => {}
        Ok(Err(err)) => crate::runtime_log::record(
            "WARN",
            format!(
                "读取 Responses 会话路由绑定失败（{:?}），本次运行将重新学习",
                err.kind()
            ),
        ),
        Err(_) => crate::runtime_log::record(
            "WARN",
            "读取 Responses 会话路由绑定的文件任务异常，本次运行将重新学习",
        ),
    }
}

pub(super) fn persist_responses_state(state: &AppState) {
    let store = Arc::clone(&state.responses_state);
    store.persist_dirty.store(true, AtomicOrdering::Release);
    if store.persist_pending.swap(true, AtomicOrdering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        loop {
            store.persist_dirty.store(false, AtomicOrdering::Release);
            let task_store = Arc::clone(&store);
            let result = tokio::task::spawn_blocking(move || {
                let _guard = task_store.io_lock.lock();
                // 串行锁内读取最新内存状态，避免迟到任务覆盖较新的绑定。
                task_store.save()
            })
            .await;
            if !matches!(result, Ok(Ok(_))) {
                let reason = match result {
                    Ok(Err(err)) => format!("{:?}", err.kind()),
                    _ => "文件任务异常".to_string(),
                };
                crate::runtime_log::record(
                    "WARN",
                    format!("Responses 会话路由绑定写入失败（{reason}），本次更新仅在内存中有效"),
                );
            }
            if store.persist_dirty.swap(false, AtomicOrdering::AcqRel) {
                continue;
            }
            store.persist_pending.store(false, AtomicOrdering::Release);
            if store.persist_dirty.load(AtomicOrdering::Acquire)
                && !store.persist_pending.swap(true, AtomicOrdering::AcqRel)
            {
                continue;
            }
            break;
        }
    });
}

pub(super) async fn flush_responses_state(state: &AppState) {
    let store = Arc::clone(&state.responses_state);
    let task_store = Arc::clone(&store);
    let result = tokio::task::spawn_blocking(move || {
        let _guard = task_store.io_lock.lock();
        task_store.save()
    })
    .await;
    if !matches!(result, Ok(Ok(_))) {
        crate::runtime_log::record(
            "WARN",
            "代理停止时未能写入 Responses 会话路由绑定，下次启动可能需要重新学习",
        );
    }
}

pub(super) fn responses_affinity_key(
    body: &Value,
    headers: &HeaderMap,
    api_key_id: &str,
    model: &str,
) -> ResponsesAffinityKey {
    let explicit = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| ("prompt-cache", value))
        .or_else(|| body_string(body, "session_id").map(|value| ("body-session", value)))
        .or_else(|| header_string(headers, "session_id").map(|value| ("session", value)))
        .or_else(|| body_string(body, "conversation_id").map(|value| ("body-conversation", value)))
        .or_else(|| header_string(headers, "conversation_id").map(|value| ("conversation", value)));
    if let Some((kind, value)) = explicit {
        return ResponsesAffinityKey {
            key: format!(
                "responses-session:{}",
                digest(&format!("{kind}\0{api_key_id}\0{value}"))
            ),
            persistent: true,
        };
    }

    if let Some(item_id) = first_opaque_item_id(body) {
        return ResponsesAffinityKey {
            key: format!(
                "responses-lineage:{}",
                digest(&format!("{api_key_id}\0{item_id}"))
            ),
            persistent: true,
        };
    }

    if let Some(previous_response_id) = body_string(body, "previous_response_id") {
        return ResponsesAffinityKey {
            key: format!(
                "responses-lineage:{}",
                digest(&format!("{api_key_id}\0previous\0{previous_response_id}"))
            ),
            persistent: true,
        };
    }

    ResponsesAffinityKey {
        key: api_key_affinity_key(model, api_key_id),
        persistent: false,
    }
}

pub(super) fn responses_provider_identity(
    provider: &ProviderConfig,
    body: &Value,
    request_headers: &HeaderMap,
) -> String {
    let effective_headers = upstream_headers(provider, request_headers, false);
    let mut components = vec![
        provider.provider_type.trim().to_ascii_lowercase(),
        provider
            .base_url
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase(),
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
        provider.auth_account_id.clone().unwrap_or_default(),
    ];
    let effective_credential = ["authorization", "x-api-key", "api-key"]
        .into_iter()
        .filter_map(|name| effective_headers.get(name))
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join("\0");
    if provider.auth_account_id.is_some() {
        let account_header = effective_headers
            .get("chatgpt-account-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .trim();
        let token_identity = oauth_token_security_identity(&provider.api_key).unwrap_or_default();
        let stable_identity = format!("{account_header}\0{token_identity}");
        components.push(digest(
            if account_header.is_empty() && token_identity.is_empty() {
                if effective_credential.is_empty() {
                    provider.api_key.trim()
                } else {
                    &effective_credential
                }
            } else {
                &stable_identity
            },
        ));
    } else {
        components.push(digest(if effective_credential.is_empty() {
            provider.api_key.trim()
        } else {
            &effective_credential
        }));
    }
    for name in [
        "chatgpt-account-id",
        "openai-project",
        "openai-organization",
        "x-openai-project-id",
    ] {
        components.push(
            effective_headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase(),
        );
    }
    digest(&components.join("\0"))
}

pub(super) fn responses_has_opaque_state(body: &Value) -> bool {
    body.get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
        || body
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    opaque_item_fingerprint_is_some(item)
                        || replay_item_id_fingerprint(item).is_some()
                })
            })
}

pub(super) fn strip_rejected_responses_state(body: &mut Value, rejected: &HashSet<String>) -> bool {
    if rejected.is_empty() {
        return false;
    }
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if object
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(previous_response_fingerprint)
        .is_some_and(|fingerprint| rejected.contains(&fingerprint))
    {
        object.remove("previous_response_id");
        changed = true;
    }
    if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
        items.retain_mut(|item| {
            if opaque_item_fingerprint(item)
                .is_some_and(|fingerprint| rejected.contains(&fingerprint))
            {
                changed = true;
                return false;
            }
            if replay_item_id_fingerprint(item)
                .is_some_and(|fingerprint| rejected.contains(&fingerprint))
            {
                item.as_object_mut().unwrap().remove("id");
                changed = true;
            }
            true
        });
    }
    changed
}

pub(super) fn strip_all_responses_state(body: &mut Value) -> Vec<String> {
    let Some(object) = body.as_object_mut() else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    if let Some(value) = object
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        removed.push(previous_response_fingerprint(value));
        object.remove("previous_response_id");
    }
    if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
        items.retain_mut(|item| {
            if let Some(fingerprint) = opaque_item_fingerprint(item) {
                removed.push(fingerprint);
                return false;
            }
            if let Some(fingerprint) = replay_item_id_fingerprint(item) {
                removed.push(fingerprint);
                item.as_object_mut().unwrap().remove("id");
            }
            true
        });
    }
    removed
}

pub(super) fn is_encrypted_state_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("invalid_encrypted_content")
        || lower.contains("thinking_signature_invalid")
        || lower.contains("invalid encrypted summary")
        || lower.contains("invalid_encrypted_summary")
        || lower.contains("item_id did not match the target item id")
        || lower.contains("could not decode the compaction blob")
        || lower.contains("was created under a different azure openai resource")
        || ((lower.contains("encrypted content") || lower.contains("encrypted_content"))
            && (lower.contains("could not be verified")
                || lower.contains("could not be decrypted")
                || lower.contains("could not decrypt")
                || lower.contains("could not verify")
                || lower.contains("not issued to this caller")
                || lower.contains("different account or model")))
}

pub(super) fn redact_encrypted_state_error(message: &str) -> String {
    if is_encrypted_state_rejection(message) {
        "invalid_encrypted_content: 上游无法复用 Responses 会话状态；状态可能来自其他账号、项目、资源或模型"
            .to_string()
    } else {
        truncate(message)
    }
}

fn body_string<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn header_string<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
}

fn first_opaque_item_id(body: &Value) -> Option<&str> {
    body.get("input")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| opaque_item_fingerprint_is_some(item))?
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn opaque_item_fingerprint_is_some(item: &Value) -> bool {
    opaque_item_fingerprint(item).is_some()
}

fn opaque_item_fingerprint(item: &Value) -> Option<String> {
    let object = item.as_object()?;
    let item_type = object.get("type").and_then(Value::as_str)?;
    if !matches!(
        item_type,
        "reasoning"
            | "compaction"
            | "compaction_summary"
            | "context_compaction"
            | "encrypted_content"
    ) {
        return None;
    }
    if let Some(encrypted) = object
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Some(digest(&format!("encrypted\0{encrypted}")));
    }
    object
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|id| digest(&format!("item-id\0{id}")))
}

fn replay_item_id_fingerprint(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str)? {
        "message"
        | "function_call"
        | "function_call_output"
        | "custom_tool_call"
        | "custom_tool_call_output" => {}
        _ => return None,
    }
    item.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(|id| digest(&format!("replay-item-id\0{id}")))
}

fn previous_response_fingerprint(value: &str) -> String {
    digest(&format!("previous-response\0{value}"))
}

fn digest(value: &str) -> String {
    let bytes = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_session_key(value: &str) -> bool {
    let Some((prefix, digest)) = value.rsplit_once(':') else {
        return false;
    };
    matches!(prefix, "responses-session" | "responses-lineage") && valid_digest(digest)
}

fn is_expired(updated_at: i64, now: i64) -> bool {
    updated_at <= 0 || now.saturating_sub(updated_at) > SESSION_TTL_SECONDS
}

fn prune_entries(entries: &mut HashMap<String, ResponsesSessionBinding>, now: i64) {
    entries.retain(|key, entry| {
        valid_session_key(key)
            && if entry.provider.is_empty() || entry.request_model.is_empty() {
                entry.updated_at > 0 && now.saturating_sub(entry.updated_at) <= PENDING_TTL_SECONDS
            } else {
                !is_expired(entry.updated_at, now)
            }
    });
    if entries.len() <= MAX_SESSIONS {
        return;
    }
    let pending = entries
        .iter()
        .filter(|(_, entry)| entry.provider.is_empty() || entry.request_model.is_empty())
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in pending
        .into_iter()
        .take(entries.len().saturating_sub(MAX_SESSIONS))
    {
        entries.remove(&key);
    }
    if entries.len() <= MAX_SESSIONS {
        return;
    }
    let mut oldest = entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.updated_at))
        .collect::<Vec<_>>();
    oldest.sort_by_key(|(_, updated_at)| *updated_at);
    for (key, _) in oldest.into_iter().take(entries.len() - MAX_SESSIONS) {
        entries.remove(&key);
    }
}

fn write_state_file(path: &Path, state: &StoredResponsesState) -> io::Result<()> {
    let bytes = serde_json::to_vec(state).map_err(|_| invalid_file())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid_file());
    }
    let Some(directory) = path.parent() else {
        return Err(invalid_file());
    };
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    if !fs::symlink_metadata(directory)?.file_type().is_dir() {
        return Err(invalid_file());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = directory.join(format!(".responses-affinity-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_state_file(path: &Path) -> io::Result<Option<StoredResponsesState>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(invalid_file());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid_file());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| invalid_file())
}

fn invalid_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Responses 会话路由绑定文件格式、版本或大小无效",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, key: &str, account: Option<&str>) -> ProviderConfig {
        let mut provider: ProviderConfig = serde_json::from_value(json!({
            "name": name,
            "provider_type": "openai",
            "base_url": "https://example.test/v1",
            "api_key": key,
            "models": ["gpt-test"],
            "responses_mode": "native"
        }))
        .unwrap();
        provider.auth_account_id = account.map(ToOwned::to_owned);
        provider
    }

    #[test]
    fn session_key_prefers_explicit_identifiers_and_never_contains_them() {
        let headers = HeaderMap::new();
        let body = json!({"model":"gpt-test", "prompt_cache_key":"private-session"});
        let key = responses_affinity_key(&body, &headers, "local-key", "gpt-test");
        assert!(key.persistent);
        assert!(key.key.starts_with("responses-session:"));
        assert!(!key.key.contains("private-session"));
    }

    #[test]
    fn lineage_key_uses_opaque_item_without_persisting_id_or_ciphertext() {
        let body = json!({"model":"gpt-test", "input":[{
            "type":"reasoning", "id":"rs_private", "encrypted_content":"secret-cipher"
        }]});
        let key = responses_affinity_key(&body, &HeaderMap::new(), "", "gpt-test");
        assert!(key.persistent);
        assert!(key.key.starts_with("responses-lineage:"));
        assert!(!key.key.contains("rs_private"));
        assert!(!key.key.contains("secret-cipher"));
    }

    #[test]
    fn previous_response_id_gets_a_private_best_effort_lineage_key() {
        let body = json!({"model":"gpt-test", "previous_response_id":"resp_private"});
        let key = responses_affinity_key(&body, &HeaderMap::new(), "", "gpt-test");
        assert!(key.persistent);
        assert!(key.key.starts_with("responses-lineage:"));
        assert!(!key.key.contains("resp_private"));
    }

    #[test]
    fn stripping_state_preserves_visible_messages_and_tools() {
        let mut body = json!({
            "model":"gpt-test",
            "previous_response_id":"resp_private",
            "input":[
                {"type":"message","role":"user","content":"continue"},
                {"type":"message","role":"assistant","content":"visible","encrypted_content":"extension-must-stay"},
                {"type":"reasoning","id":"rs_private","encrypted_content":"secret-cipher","summary":[]},
                {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let removed = strip_all_responses_state(&mut body);
        assert_eq!(removed.len(), 2);
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["input"].as_array().unwrap().len(), 4);
        let serialized = body.to_string();
        assert!(!serialized.contains("secret-cipher"));
        assert!(serialized.contains("extension-must-stay"));
        assert!(serialized.contains("call_1"));
        assert!(serialized.contains("continue"));
    }

    #[test]
    fn rejected_filter_only_removes_previously_rejected_items() {
        let mut first = json!({"input":[
            {"type":"reasoning","id":"rs_a","encrypted_content":"cipher-a"},
            {"type":"reasoning","id":"rs_b","encrypted_content":"cipher-b"}
        ]});
        let removed = strip_all_responses_state(&mut first);
        let mut second = json!({"input":[
            {"type":"reasoning","id":"rs_a","encrypted_content":"cipher-a"},
            {"type":"reasoning","id":"rs_b","encrypted_content":"replacement-b"}
        ]});
        let rejected = [removed[0].clone()].into_iter().collect();
        assert!(strip_rejected_responses_state(&mut second, &rejected));
        assert_eq!(second["input"].as_array().unwrap().len(), 1);
        assert_eq!(second["input"][0]["encrypted_content"], "replacement-b");
    }

    #[test]
    fn stripping_replayed_ids_preserves_messages_attachments_and_tool_links() {
        let mut body = json!({"model":"gpt-test", "input":[
            {"type":"message","id":"msg_old","role":"assistant","content":[
                {"type":"output_text","text":"visible","annotations":[]}
            ]},
            {"type":"message","role":"user","content":[
                {"type":"input_image","image_url":"https://example.test/image.png","detail":"auto"}
            ]},
            {"type":"function_call","id":"fc_old","call_id":"call_1","name":"read","arguments":"{}"},
            {"type":"function_call_output","id":"fco_old","call_id":"call_1","output":"ok"},
            {"type":"custom_tool_call","id":"ctc_old","call_id":"call_2","name":"edit","input":"patch"},
            {"type":"custom_tool_call_output","id":"ctco_old","call_id":"call_2","output":"done"}
        ]});
        assert!(responses_has_opaque_state(&body));
        let removed = strip_all_responses_state(&mut body);
        assert_eq!(removed.len(), 5);
        assert!(!responses_has_opaque_state(&body));
        assert_eq!(
            body,
            json!({"model":"gpt-test", "input":[
                {"type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"visible","annotations":[]}
                ]},
                {"type":"message","role":"user","content":[
                    {"type":"input_image","image_url":"https://example.test/image.png","detail":"auto"}
                ]},
                {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"},
                {"type":"custom_tool_call","call_id":"call_2","name":"edit","input":"patch"},
                {"type":"custom_tool_call_output","call_id":"call_2","output":"done"}
            ]})
        );
    }

    #[test]
    fn rejected_replayed_id_keeps_content_and_new_item_ids() {
        let mut rejected_body = json!({"input":[
            {"type":"message","id":"msg_old","role":"assistant","content":"visible"}
        ]});
        let rejected = strip_all_responses_state(&mut rejected_body)
            .into_iter()
            .collect();
        let mut body = json!({"input":[
            {"type":"message","id":"msg_old","role":"assistant","content":"visible"},
            {"type":"message","id":"msg_new","role":"assistant","content":"new"}
        ]});
        assert!(strip_rejected_responses_state(&mut body, &rejected));
        assert_eq!(
            body,
            json!({"input":[
                {"type":"message","role":"assistant","content":"visible"},
                {"type":"message","id":"msg_new","role":"assistant","content":"new"}
            ]})
        );
        assert!(!strip_rejected_responses_state(&mut body, &rejected));
    }

    #[test]
    fn provider_identity_tracks_security_domain_but_not_oauth_refresh_token() {
        let body = json!({"model":"gpt-test"});
        let headers = HeaderMap::new();
        let plain_a = responses_provider_identity(&provider("a", "key-a", None), &body, &headers);
        let plain_b = responses_provider_identity(&provider("a", "key-b", None), &body, &headers);
        assert_ne!(plain_a, plain_b);

        let opaque_a = responses_provider_identity(
            &provider("auth:a", "opaque-token-a", Some("account-a")),
            &body,
            &headers,
        );
        let opaque_b = responses_provider_identity(
            &provider("auth:a", "opaque-token-b", Some("account-a")),
            &body,
            &headers,
        );
        assert_ne!(opaque_a, opaque_b);

        let token_a = "header.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdCJ9LCJzdWIiOiJ1c2VyIiwiZXhwIjoxfQ.signature";
        let token_b = "header.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdCJ9LCJzdWIiOiJ1c2VyIiwiZXhwIjoyfQ.signature";
        let token_other = "header.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoib3RoZXIifSwic3ViIjoidXNlciIsImV4cCI6Mn0.signature";
        let auth_a = responses_provider_identity(
            &provider("auth:a", token_a, Some("account-a")),
            &body,
            &headers,
        );
        let auth_b = responses_provider_identity(
            &provider("auth:a", token_b, Some("account-a")),
            &body,
            &headers,
        );
        let auth_other = responses_provider_identity(
            &provider("auth:a", token_other, Some("account-a")),
            &body,
            &headers,
        );
        assert_eq!(auth_a, auth_b);
        assert_ne!(auth_a, auth_other);

        let mut custom_a = provider("custom", "placeholder", None);
        custom_a
            .extra_headers
            .insert("Authorization".into(), "Bearer actual-a".into());
        let mut custom_b = custom_a.clone();
        custom_b
            .extra_headers
            .insert("Authorization".into(), "Bearer actual-b".into());
        assert_ne!(
            responses_provider_identity(&custom_a, &body, &headers),
            responses_provider_identity(&custom_b, &body, &headers)
        );
    }

    #[test]
    fn recognizes_reported_encrypted_state_errors() {
        assert!(is_encrypted_state_rejection(
            "The encrypted content for item rs_test could not be verified. Reason: Encrypted content could not be decrypted or parsed."
        ));
        assert!(is_encrypted_state_rejection(
            "reasoning encrypted_content was not issued to this caller"
        ));
        assert!(is_encrypted_state_rejection(
            "The requested item was created under a different Azure OpenAI resource. Use the same resource that created the item to access it."
        ));
        assert!(!is_encrypted_state_rejection(
            "invalid request: temperature"
        ));
    }

    #[test]
    fn store_roundtrip_contains_only_hashes_not_session_or_ciphertext() {
        let directory =
            std::env::temp_dir().join(format!("routehub-responses-state-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("responses-affinity.json");
        let store = ResponsesStateStore::new(path.clone());
        let key = format!("responses-session:{}", digest("private-session"));
        let identity = digest("private-provider");
        let rejected = digest("secret-ciphertext");
        assert!(store.remember_binding(&key, "auth:test", "gpt-test", &identity, None));
        assert!(store.remember_rejected(&key, &identity, [rejected.clone()]));
        assert!(store.save().unwrap());
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("private-session"));
        assert!(!raw.contains("secret-ciphertext"));

        let restored = ResponsesStateStore::new(path);
        assert_eq!(restored.load().unwrap(), 1);
        let binding = restored.binding(&key).unwrap();
        assert_eq!(binding.provider, "auth:test");
        assert!(restored.rejected(&key, &identity).contains(&rejected));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_concurrent_completion_cannot_overwrite_a_new_owner() {
        let store = ResponsesStateStore::default();
        let key = format!("responses-session:{}", digest("session"));
        let owner_a = digest("owner-a");
        let owner_b = digest("owner-b");
        let owner_c = digest("owner-c");
        assert!(store.remember_binding(&key, "A", "model", &owner_a, None));
        assert!(store.remember_binding(&key, "B", "model", &owner_b, Some(&owner_a),));
        assert!(!store.remember_binding(&key, "C", "model", &owner_c, Some(&owner_a),));
        let binding = store.binding(&key).unwrap();
        assert_eq!(binding.provider, "B");
        assert_eq!(binding.provider_identity, owner_b);
    }

    #[test]
    fn generic_message_with_encrypted_error_code_is_redacted_and_classified() {
        let message = clean_upstream_error(
            r#"{"error":{"code":"invalid_encrypted_content","message":"invalid request"}}"#,
        );
        assert!(is_encrypted_state_rejection(&message));
        assert!(!message.contains("invalid request"));
    }
}
