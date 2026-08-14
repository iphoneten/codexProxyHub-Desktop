//! 单次请求的上下文：请求 ID + 取消信号。
//!
//! 存在的原因是流式请求的实际工作发生在 detached `tokio::spawn` 里：
//! handler 只负责把 `Body::from_stream(...)` 交给 axum 就返回了，之后客户端
//! 断开（Ctrl-C、切换会话、进程被杀）只会让响应体被 drop，spawn 出去的翻译/
//! 探针任务感知不到，会一直 `stream.next().await` 等上游下一个 chunk。
//! 推理模型经常几十秒不吐字，这段时间上游仍在生成，账号额度照扣。
//!
//! 因此每个请求都持有一个 [`CancellationToken`]：
//!   * 中间件把 `DropGuard` 先放在 handler future 里（响应头之前断开就取消），
//!     handler 返回后再把 guard 移进响应体（流中途断开就取消）；
//!   * 所有流式 spawn 统一 `tokio::select!` 监听它，被取消就立刻退出，
//!     顺带 drop 掉 reqwest 的 bytes_stream，上游连接随之关闭；
//!   * 它是 server 根 token 的子 token，代理停止/退出时根 token 一取消，
//!     在飞的流全部结束，graceful shutdown 不再干等。
use std::fmt;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};
use uuid::Uuid;

/// 请求上下文。克隆代价等同于 `Arc`，可以随意传给 spawn 出去的任务。
#[derive(Clone)]
pub(crate) struct RequestContext {
    request_id: String,
    cancel: CancellationToken,
}

impl RequestContext {
    /// 挂在 server 根 token 下：根 token 取消（代理停止）会连带取消本次请求。
    pub(crate) fn child_of(root: &CancellationToken, request_id: Option<String>) -> Self {
        Self {
            request_id: request_id
                .map(|id| sanitize_request_id(&id))
                .filter(|id| !id.is_empty())
                .unwrap_or_else(new_request_id),
            cancel: root.child_token(),
        }
    }

    /// 脱离 server 生命周期的独立上下文：不挂在任何根 token 下，只会被自己的
    /// drop guard 取消。给不经过中间件的调用用（目前只有测试）。
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self {
            request_id: new_request_id(),
            cancel: CancellationToken::new(),
        }
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 拿一个 drop 即取消的守卫。放进 future 或响应体闭包里，
    /// 它被 drop 就说明这条链路没人要了。
    pub(crate) fn drop_guard(&self) -> DropGuard {
        self.cancel.clone().drop_guard()
    }
}

impl fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestContext")
            .field("request_id", &self.request_id)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

/// 向有界 channel 写入时同时监听取消，避免下游背压让关停卡在 `send().await`。
pub(crate) async fn send_with_cancel<T>(
    tx: &mpsc::Sender<T>,
    value: T,
    cancel: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(()),
        result = tx.send(value) => result.map_err(|_| ()),
    }
}

fn new_request_id() -> String {
    format!("req_{}", Uuid::new_v4().simple())
}

/// 客户端传进来的 `x-request-id` 直接复用，方便两侧日志对齐；
/// 但要限长并过滤掉非 header-safe 字符，否则回写响应头会失败。
fn sanitize_request_id(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .take(128)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_request_id_is_prefixed_and_unique() {
        let a = RequestContext::detached();
        let b = RequestContext::detached();
        assert!(a.request_id().starts_with("req_"));
        assert_ne!(a.request_id(), b.request_id());
    }

    #[test]
    fn client_request_id_is_reused_after_sanitizing() {
        let root = CancellationToken::new();
        let ctx = RequestContext::child_of(&root, Some(" trace-1:abc \n".to_string()));
        assert_eq!(ctx.request_id(), "trace-1:abc");
    }

    #[test]
    fn unusable_client_request_id_falls_back_to_generated() {
        let root = CancellationToken::new();
        let ctx = RequestContext::child_of(&root, Some("   ".to_string()));
        assert!(ctx.request_id().starts_with("req_"));

        let ctx = RequestContext::child_of(&root, Some("中文///".to_string()));
        assert!(ctx.request_id().starts_with("req_"));
    }

    #[test]
    fn root_cancel_propagates_to_request() {
        let root = CancellationToken::new();
        let ctx = RequestContext::child_of(&root, None);
        assert!(!ctx.cancel_token().is_cancelled());
        root.cancel();
        assert!(ctx.cancel_token().is_cancelled());
    }

    #[test]
    fn drop_guard_cancels_only_this_request() {
        let root = CancellationToken::new();
        let first = RequestContext::child_of(&root, None);
        let second = RequestContext::child_of(&root, None);
        drop(first.drop_guard());
        assert!(first.cancel_token().is_cancelled());
        assert!(!second.cancel_token().is_cancelled());
        assert!(!root.is_cancelled());
    }

    #[test]
    fn long_client_request_id_is_truncated() {
        let root = CancellationToken::new();
        let ctx = RequestContext::child_of(&root, Some("a".repeat(300)));
        assert_eq!(ctx.request_id().len(), 128);
    }

    #[tokio::test]
    async fn full_channel_send_is_interrupted_by_cancel() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send("first").await.unwrap();
        let cancel = CancellationToken::new();
        let send = send_with_cancel(&tx, "second", &cancel);
        tokio::pin!(send);

        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), &mut send)
            .await
            .expect("取消后发送不应继续等待 channel 容量");
        assert!(result.is_err());
    }
}
