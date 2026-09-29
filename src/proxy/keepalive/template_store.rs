use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = (MAX_REQUEST_BODY_BYTES + 1024 * 1024) as u64;

/// 文件操作放在 blocking 线程中，串行保存各次更新，避免旧快照覆盖新模板。
#[derive(Default)]
pub(crate) struct TemplateStore {
    directory: Option<PathBuf>,
    io_lock: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct StoredTemplate {
    version: u32,
    provider: String,
    base_url: String,
    body: Value,
    headers: BTreeMap<String, String>,
    #[serde(default)]
    succeeded: bool,
    #[serde(default)]
    failed: bool,
}

impl TemplateStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory: Some(directory),
            io_lock: Mutex::new(()),
        }
    }

    fn path(&self, provider: &ProviderConfig) -> Option<PathBuf> {
        // 渠道名称和 URL 不直接作为文件名，避免路径穿越或暴露 URL 中的私有信息。
        let identity = serde_json::to_vec(&(&provider.name, &provider.base_url)).ok()?;
        let name = Uuid::new_v5(&Uuid::NAMESPACE_URL, &identity);
        self.directory
            .as_ref()
            .map(|dir| dir.join(format!("{name}.json")))
    }

    fn save(
        &self,
        provider: &ProviderConfig,
        template: &NativeKeepaliveTemplate,
    ) -> io::Result<bool> {
        let Some(path) = self.path(provider) else {
            return Ok(false);
        };
        let directory = path.parent().expect("template has a directory");
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;
        if !fs::symlink_metadata(directory)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "模板目录不是普通目录",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let headers = safe_template_headers(&template.headers)
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_string()))
            })
            .collect();
        let stored = StoredTemplate {
            version: FILE_VERSION,
            provider: provider.name.clone(),
            base_url: provider.base_url.clone(),
            body: template.body.clone(),
            headers,
            succeeded: template.succeeded,
            failed: template.failed,
        };
        let bytes = serde_json::to_vec(&stored).map_err(|_| invalid_file())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid_file());
        }
        let temporary = directory.join(format!(".{}.tmp", Uuid::new_v4()));
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
            // 同目录原子替换；写入失败时保留上一次完整模板。
            fs::rename(&temporary, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map(|_| true)
    }

    fn load(&self, provider: &ProviderConfig) -> io::Result<Option<NativeKeepaliveTemplate>> {
        let Some(path) = self.path(provider) else {
            return Ok(None);
        };
        if let Some(directory) = path.parent() {
            match fs::symlink_metadata(directory) {
                Ok(meta) if !meta.file_type().is_dir() => return Err(invalid_file()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(err),
                _ => {}
            }
        }
        let metadata = match fs::symlink_metadata(&path) {
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
        let stored: StoredTemplate = serde_json::from_slice(&bytes).map_err(|_| invalid_file())?;
        if stored.version != FILE_VERSION
            || stored.provider != provider.name
            || stored.base_url != provider.base_url
            || !stored.body.is_object()
            || stored.body.get("input").is_none()
        {
            return Err(invalid_file());
        }
        let mut headers = HeaderMap::new();
        for (name, value) in stored.headers {
            let name =
                header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid_file())?;
            let value = HeaderValue::from_str(&value).map_err(|_| invalid_file())?;
            headers.insert(name, value);
        }
        let mut template = NativeKeepaliveTemplate::from_request(&stored.body, &headers);
        template.succeeded = stored.succeeded;
        template.failed = stored.failed;
        Ok(Some(template))
    }
}

fn invalid_file() -> io::Error {
    // 解析错误不附带原始 JSON，防止请求内容进入运行日志。
    io::Error::new(
        io::ErrorKind::InvalidData,
        "模板文件格式、版本、渠道或大小无效",
    )
}

pub(super) async fn persist_native_template(state: &AppState, provider: &ProviderConfig) -> bool {
    let state = state.clone();
    let provider = provider.clone();
    let name = provider.name.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = state.keepalive_template_store.io_lock.lock();
        // 获取写锁后再取最新快照；迟到的保存任务不能回写旧模板或旧失败状态。
        let template = state
            .keepalive_templates
            .lock()
            .get(&template_key(&provider))
            .cloned();
        match template {
            Some(template) => state.keepalive_template_store.save(&provider, &template),
            None => Ok(false),
        }
    })
    .await;
    match result {
        Ok(Ok(saved)) => saved,
        result => {
            let reason = match result {
                Ok(Err(err)) => format!("{:?}", err.kind()),
                _ => "文件任务异常".into(),
            };
            crate::runtime_log::record("WARN", format!(
                "渠道 {name}：Responses 保活模板写入文件失败（{reason}），本次更新仅在内存中可用，重启后可能恢复旧模板"
            ));
            false
        }
    }
}

pub(crate) async fn restore_native_templates(state: &AppState) {
    let state = state.clone();
    if tokio::task::spawn_blocking(move || {
        let _guard = state.keepalive_template_store.io_lock.lock();
        for provider in &state.snapshot().providers {
            if provider.auth_account_id.is_some() { continue; }
            match state.keepalive_template_store.load(provider) {
                Ok(Some(template)) => {
                    let status = template.skip_reason().unwrap_or("可直接复用");
                    crate::runtime_log::record("INFO", format!(
                        "渠道 {}：已从本地文件恢复 Responses 保活模板（{status}）", provider.name
                    ));
                    state.keepalive_templates.lock().insert(template_key(provider), template);
                }
                Ok(None) => {},
                Err(err) => crate::runtime_log::record("WARN", format!(
                    "渠道 {}：读取 Responses 保活模板文件失败（{:?}），保留原文件，等待成功的真实 Responses 请求",
                    provider.name, err.kind(),
                )),
            }
        }
    }).await.is_err() {
        crate::runtime_log::record("WARN", "恢复 Responses 保活模板的文件任务异常，等待成功的真实 Responses 请求");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("routehub-template-test-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn store(&self) -> TemplateStore {
            TemplateStore::new(self.0.join("keepalive-templates"))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn provider() -> ProviderConfig {
        serde_json::from_value(json!({
            "name": "synthetic-provider", "base_url": "https://example.test/v1",
            "api_key": "synthetic-upstream-secret", "models": ["test-model"],
            "responses_mode": "auto", "request_timeout": 2
        }))
        .unwrap()
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("synthetic-client/1.0"),
        );
        headers.insert("originator", HeaderValue::from_static("synthetic-client"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic-local-secret"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("synthetic-cookie-secret"),
        );
        headers.insert(
            "x-request-id",
            HeaderValue::from_static("synthetic-request-id"),
        );
        headers
    }

    fn state(directory: &TestDirectory, provider: &ProviderConfig) -> AppState {
        let mut state = crate::proxy::tests::test_state();
        state.keepalive_template_store = Arc::new(directory.store());
        let mut cfg = (*state.snapshot()).clone();
        cfg.providers = vec![provider.clone()];
        cfg.auth_accounts.clear();
        *state.config.write() = Arc::new(cfg);
        state
    }

    #[test]
    fn files_roundtrip_protocol_and_status_without_credentials() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let provider = provider();
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/python_keepalive.json")).unwrap();
        let mut template =
            NativeKeepaliveTemplate::from_request(&fixture["source_request"], &headers());
        template.succeeded = true;
        assert!(store.save(&provider, &template).unwrap());
        let path = store.path(&provider).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        for secret in [
            "synthetic-upstream-secret",
            "synthetic-local-secret",
            "synthetic-cookie-secret",
            "synthetic-request-id",
        ] {
            assert!(!raw.contains(secret));
        }
        let loaded = store.load(&provider).unwrap().unwrap();
        assert_eq!(loaded.body, template.body);
        assert_eq!(loaded.headers, template.headers);
        assert!(loaded.succeeded);
        assert!(!loaded.failed);
        assert_ne!(loaded.generation, template.generation);
        template.failed = true;
        store.save(&provider, &template).unwrap();
        assert!(store.load(&provider).unwrap().unwrap().failed);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[tokio::test]
    async fn captured_template_is_reused_after_restart_without_another_client_request() {
        let directory = TestDirectory::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route("/v1/responses", post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let number = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(headers["user-agent"], "synthetic-client/1.0");
                if number == 0 {
                    assert_eq!(body["input"], "synthetic-real-request");
                } else {
                    assert_eq!(body["input"], "Hi");
                    assert_eq!(body["model"], "test-model");
                    assert_eq!(body["max_output_tokens"], 1);
                    assert_eq!(headers["authorization"], "Bearer synthetic-rotated-secret");
                    assert!(!headers.contains_key("cookie"));
                    assert!(Uuid::parse_str(headers["x-request-id"].to_str().unwrap()).is_ok());
                }
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(concat!(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"output\":[]}}\n\n"
                    ))).unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider = provider();
        provider.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let first = state(&directory, &provider);
        let result = send_to_provider(
            &first,
            &provider,
            "/responses",
            &headers(),
            json!({"model": "test-model", "input": "synthetic-real-request", "stream": true}),
            true,
            Instant::now(),
            &RequestContext::detached(),
        )
        .await
        .unwrap();
        to_bytes(result.response.into_body(), 4096).await.unwrap();
        drop(first);

        provider.api_key = "synthetic-rotated-secret".into();
        let restarted = state(&directory, &provider);
        assert!(restarted.keepalive_templates.lock().is_empty());
        restore_native_templates(&restarted).await;
        assert_eq!(heartbeat_path(&restarted, &provider), "/responses");
        let result = send_idle_keepalive(&restarted, &provider, &RequestContext::detached()).await;
        server.abort();
        assert!(matches!(result.unwrap(), IdleKeepaliveOutcome::Sent));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            directory
                .store()
                .load(&provider)
                .unwrap()
                .unwrap()
                .succeeded
        );
    }

    #[tokio::test]
    async fn rejected_template_remains_paused_after_restart_and_new_capture_replaces_it() {
        let directory = TestDirectory::new();
        let app = Router::new().route(
            "/v1/responses",
            post(|| async { (StatusCode::BAD_REQUEST, "invalid codex request") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider = provider();
        provider.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let first = state(&directory, &provider);
        remember_native_template(
            &first,
            &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "synthetic"}), &headers()),
        )
        .await;
        assert_eq!(
            send_idle_keepalive(&first, &provider, &RequestContext::detached())
                .await
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        server.abort();
        drop(first);
        let restarted = state(&directory, &provider);
        restore_native_templates(&restarted).await;
        assert!(native_template_skip_reason(&restarted, &provider)
            .unwrap()
            .contains("失败"));
        assert!(matches!(
            send_idle_keepalive(&restarted, &provider, &RequestContext::detached())
                .await
                .unwrap(),
            IdleKeepaliveOutcome::Skipped(_)
        ));
        remember_native_template(
            &restarted,
            &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "new-synthetic"}), &headers()),
        )
        .await;
        let again = state(&directory, &provider);
        restore_native_templates(&again).await;
        assert!(native_template_skip_reason(&again, &provider).is_none());
    }

    #[tokio::test]
    async fn initial_500_retries_without_another_client_request_and_keeps_file_usable() {
        let directory = TestDirectory::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route("/v1/responses", post(move || {
            let number = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                if number == 0 {
                    (StatusCode::INTERNAL_SERVER_ERROR, "model temporarily unavailable").into_response()
                } else {
                    Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"output\":[]}}\n\n"))
                        .unwrap()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider = provider();
        provider.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        provider.heartbeat_enabled = true;
        provider.heartbeat_interval_secs = 1;
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let first = state(&directory, &provider);
        remember_native_template(
            &first,
            &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "synthetic"}), &headers()),
        )
        .await;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            recover_idle_channel(&first, &provider.name, &RequestContext::detached()),
        )
        .await;
        server.abort();
        result.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let saved = directory.store().load(&provider).unwrap().unwrap();
        assert!(saved.succeeded);
        assert!(!saved.failed);
        let restarted = state(&directory, &provider);
        restore_native_templates(&restarted).await;
        assert!(native_template_skip_reason(&restarted, &provider).is_none());
    }

    #[tokio::test]
    async fn corrupt_or_mismatched_files_are_preserved_and_do_not_block_startup() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let provider = provider();
        assert!(store.load(&provider).unwrap().is_none());
        store
            .save(
                &provider,
                &NativeKeepaliveTemplate::from_request(&json!({"input": "synthetic"}), &headers()),
            )
            .unwrap();
        let mut other = provider.clone();
        other.base_url = "https://other.example.test/v1".into();
        assert!(store.load(&other).unwrap().is_none());
        fs::copy(store.path(&provider).unwrap(), store.path(&other).unwrap()).unwrap();
        assert!(store.load(&other).is_err());
        let path = store.path(&provider).unwrap();
        fs::write(&path, b"broken-json").unwrap();
        let restarted = state(&directory, &provider);
        restore_native_templates(&restarted).await;
        assert!(restarted.keepalive_templates.lock().is_empty());
        assert_eq!(fs::read(path).unwrap(), b"broken-json");
    }

    #[tokio::test]
    async fn failed_save_keeps_memory_and_does_not_overwrite_unrelated_files() {
        let directory = TestDirectory::new();
        let provider = provider();
        let state = state(&directory, &provider);
        let blocker = directory.0.join("keepalive-templates");
        fs::write(&blocker, b"preserve-this-file").unwrap();
        remember_native_template(
            &state,
            &provider,
            NativeKeepaliveTemplate::from_request(&json!({"input": "synthetic"}), &headers()),
        )
        .await;
        assert!(native_template_skip_reason(&state, &provider).is_none());
        assert_eq!(fs::read(blocker).unwrap(), b"preserve-this-file");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_captures_leave_the_latest_memory_template_on_disk() {
        let directory = TestDirectory::new();
        let provider = provider();
        let state = state(&directory, &provider);
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..12 {
            let state = state.clone();
            let provider = provider.clone();
            tasks.spawn(async move {
                remember_native_template(
                    &state,
                    &provider,
                    NativeKeepaliveTemplate::from_request(
                        &json!({"input": "synthetic", "metadata": {"index": index}}),
                        &headers(),
                    ),
                )
                .await;
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert_eq!(
            directory.store().load(&provider).unwrap().unwrap().body,
            state.keepalive_templates.lock()[&template_key(&provider)].body
        );
    }
}
