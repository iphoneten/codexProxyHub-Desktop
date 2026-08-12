use super::*;

fn provider(provider_type: &str) -> ProviderConfig {
    serde_json::from_value(json!({
        "name": "test",
        "provider_type": provider_type,
        "base_url": "https://example.test/v1",
        "api_key": "sk-test",
        "models": ["test-model"]
    }))
    .unwrap()
}

#[test]
fn anthropic_client_headers_only_pass_to_anthropic_providers() {
    let mut client_headers = HeaderMap::new();
    client_headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("context-1m-2025-08-07"),
    );

    let anthropic = upstream_headers(&provider("anthropic"), &client_headers, false);
    assert_eq!(
        anthropic
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("context-1m-2025-08-07")
    );

    for provider_type in ["openai", "google_ai_studio", "custom"] {
        let headers = upstream_headers(&provider(provider_type), &client_headers, false);
        assert!(
            !headers.contains_key("anthropic-beta"),
            "{provider_type} 渠道不应收到 anthropic-beta"
        );
    }
}

#[test]
fn anthropic_passthrough_keeps_only_safe_diagnostic_response_headers() {
    let mut upstream = reqwest::header::HeaderMap::new();
    upstream.insert("request-id", HeaderValue::from_static("req-1"));
    upstream.insert("x-request-id", HeaderValue::from_static("xreq-1"));
    upstream.insert(
        "anthropic-ratelimit-requests-remaining",
        HeaderValue::from_static("42"),
    );
    upstream.insert("retry-after", HeaderValue::from_static("3"));
    upstream.insert("set-cookie", HeaderValue::from_static("secret=1"));
    upstream.insert(
        reqwest::header::SERVER,
        HeaderValue::from_static("upstream"),
    );

    let headers = anthropic_passthrough_response_headers(&upstream);

    assert_eq!(headers.get("request-id").unwrap(), "req-1");
    assert_eq!(headers.get("x-request-id").unwrap(), "xreq-1");
    assert_eq!(
        headers
            .get("anthropic-ratelimit-requests-remaining")
            .unwrap(),
        "42"
    );
    assert_eq!(headers.get("retry-after").unwrap(), "3");
    assert!(!headers.contains_key("set-cookie"));
    assert!(!headers.contains_key("server"));
}
