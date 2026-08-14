use super::*;
use axum::body::HttpBody;
use axum::extract::Request;
use axum::middleware::Next;
use tokio_util::sync::DropGuard;

pub(super) const REQUEST_ID_HEADER: &str = "x-request-id";

/// 给每个请求挂上 [`RequestContext`]，并保证「没人要这条链路了」一定会变成取消信号。
///
/// 取消需要盖住两个阶段，因为流式请求的响应体是在 handler 返回**之后**才产出的：
///   1. handler 执行期间（响应头还没发）客户端断开 → axum 直接 drop 这个 future，
///      栈上的 `guard` 随之 drop → 取消；
///   2. handler 返回后流到一半客户端断开 → hyper drop 响应体，
///      被移进 body 闭包里的同一个 `guard` 随之 drop → 取消。
///
/// 响应头统一回写 `x-request-id`，客户端带了就沿用它的值，方便两侧日志对齐。
pub(super) async fn attach_request_context(
    root: CancellationToken,
    mut req: Request,
    next: Next,
) -> Response {
    let incoming = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let ctx = RequestContext::child_of(&root, incoming);
    let guard = ctx.drop_guard();
    req.extensions_mut().insert(ctx.clone());

    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(ctx.request_id()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    hold_guard_until_body_done(response, guard)
}

/// 把取消守卫的生命周期延长到响应体读完（或被 drop）为止。
///
/// 已经完整缓冲好的响应体（JSON、错误体）直接放过：它们的 `size_hint` 是精确值，
/// 包一层 stream 会让 hyper 丢掉 content-length 改用 chunked，纯属没必要的行为变更。
pub(super) fn hold_guard_until_body_done(response: Response, guard: DropGuard) -> Response {
    if response.body().size_hint().exact().is_some() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let guard = Some(guard);
    let stream = body.into_data_stream().map(move |item| {
        let _guard = &guard;
        item
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::sync::atomic::AtomicBool;

    type Cancelled = Arc<AtomicBool>;

    fn app(root: CancellationToken, cancelled: Cancelled) -> Router {
        Router::new()
            .route(
                "/json",
                get(|Extension(ctx): Extension<RequestContext>| async move {
                    Json(json!({"request_id": ctx.request_id()}))
                }),
            )
            .route(
                "/stream",
                get(
                    move |Extension(ctx): Extension<RequestContext>| async move {
                        // 只发一个事件就挂住不结束，模拟推理模型长时间不吐字：
                        // 此时唯一能叫停后台任务的信号就是取消 token
                        let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
                        let cancel = ctx.cancel_token();
                        tokio::spawn(async move {
                            let _ = tx.send(Ok(Bytes::from_static(b"data: hi\n\n"))).await;
                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    cancelled.store(true, Ordering::SeqCst);
                                }
                                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                            }
                        });
                        Response::builder()
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from_stream(ReceiverStream::new(rx)))
                            .unwrap()
                    },
                ),
            )
            .layer(axum::middleware::from_fn(move |req, next| {
                attach_request_context(root.clone(), req, next)
            }))
    }

    async fn serve(root: CancellationToken) -> (String, Cancelled) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancelled: Cancelled = Arc::new(AtomicBool::new(false));
        let app = app(root, Arc::clone(&cancelled));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), cancelled)
    }

    async fn wait_for_cancel(flag: &Cancelled) -> bool {
        for _ in 0..100 {
            if flag.load(Ordering::SeqCst) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn generated_request_id_is_echoed_in_response_header() {
        let (base, _) = serve(CancellationToken::new()).await;
        let resp = reqwest::get(format!("{base}/json")).await.unwrap();
        let request_id = resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        // 缓冲响应仍然带 content-length，没有被包成 chunked
        assert!(resp.headers().get(header::CONTENT_LENGTH).is_some());
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["request_id"], request_id);
        assert!(request_id.starts_with("req_"));
    }

    #[tokio::test]
    async fn client_request_id_is_reused() {
        let (base, _) = serve(CancellationToken::new()).await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/json"))
            .header(REQUEST_ID_HEADER, "trace-abc")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get(REQUEST_ID_HEADER).unwrap(),
            HeaderValue::from_static("trace-abc")
        );
    }

    #[tokio::test]
    async fn dropping_a_streaming_response_cancels_the_request() {
        let (base, cancelled) = serve(CancellationToken::new()).await;
        let resp = reqwest::get(format!("{base}/stream")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(!cancelled.load(Ordering::SeqCst));
        // 只拿到响应头就丢掉响应体，模拟客户端中途退出
        drop(resp);
        assert!(
            wait_for_cancel(&cancelled).await,
            "客户端断开后请求上下文应被取消"
        );
    }

    #[tokio::test]
    async fn root_cancel_stops_in_flight_streams() {
        let root = CancellationToken::new();
        let (base, cancelled) = serve(root.clone()).await;
        let resp = reqwest::get(format!("{base}/stream")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(!cancelled.load(Ordering::SeqCst));
        // 代理停止：根 token 一取消，在飞的流也要跟着结束
        root.cancel();
        assert!(
            wait_for_cancel(&cancelled).await,
            "根 token 取消后在飞请求应被取消"
        );
        drop(resp);
    }

    /// 停止代理时 graceful shutdown 不能被在飞的流式响应拖住。
    /// 流式响应体永远不会自己结束，所以关停顺序必须是「先取消、再收尾」；
    /// 少了那次 `root_cancel.cancel()`，`axum::serve` 会一直等下去，
    /// 桌面端的「停止」按钮看起来就像卡死。
    #[tokio::test]
    async fn graceful_shutdown_does_not_wait_for_in_flight_streams() {
        let root = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancelled: Cancelled = Arc::new(AtomicBool::new(false));
        let app = app(root.clone(), Arc::clone(&cancelled));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                    root.cancel();
                })
                .await;
        });

        // 拿到响应头就把响应体一直握在手里，模拟正在输出的会话
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/stream"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("graceful shutdown 被在飞的流式响应卡住了")
            .unwrap();
        assert!(cancelled.load(Ordering::SeqCst));
        drop(resp);
    }
}
