# 一、必修:高危功能缺陷(会导致静默错误/兼容性破坏)
## 1. [已修复] Responses /responses → chat 回退丢失 model_mapping
handlers.rs:275 — 回退分支用 with_model(body.clone(), ...) 重建,却没重新 apply_model_mapping。凡是配了 model_mapping 的 provider,回退时会把未映射的模型名发给上游,直接 "model not found"。这是确定性 bug。
修复: fallback 复用已应用 model_mapping 的 upstream_body,并新增回归测试 upstream_body_for_provider_applies_model_mapping。验证: cargo test 通过。

## 2. [已修复] include_usage 探测把任意 4xx 当作 "不支持"
upstream.rs:174-180 — probing && status.is_client_error() 会让 401/403/404/422(坏 key、过期 token、错模型)永久污染该 provider 的 usage_injection,整个进程生命周期内静默关闭用量统计。应只在响应体确实提到 include_usage/stream_options 时才标记。
修复: 仅当 4xx 响应体明确包含 include_usage 或 stream_options 时才关闭探测,普通鉴权/模型错误不再污染 provider。验证: cargo test 通过。

## 3. Anthropic thinking 内容/事件全部丢失(两个 agent 独立确认)
anthropic.rs:406-428 非流式 + anthropic.rs:676-793 流式 — thinking/redacted_thinking/thinking_delta/signature_delta 全部命中 _ => {} 被吞。所有 Claude 4/4.5 客户端的推理链输出丢失。同时请求侧不映射 reasoning_effort → thinking.budget_tokens,无法触发 Extended Thinking。

## 4. Gemini 流式违反 OpenAI SSE 契约(两个 agent 确认)
google_ai.rs:430-442 — 每个 chunk 用新 chatcmpl-<uuid> id 和新 created,同一 response 的 chunk id 应相同,客户端去重/合并会失败。同时终局 chunk 不带 usage,stream_options.include_usage 拿不到 token 数。functionCall 的 index/id 每 chunk 重新编号,跨 chunk tool_call 拼装错位。

## 5. Gemini 缺 safety_settings(两个 agent 确认,标为高)
google_ai.rs:96-116 — 从不设置 safetySettings,用户无法调整默认安全阈值,大量正常请求被 SAFETY 拦截且内容已丢失。

## 6. [已修复] Responses json_schema 结构映射错误(两个 agent 确认)
responses_api.rs:183-191 — 应展开为 text.format: {type, name, schema, strict},现在把整个 json_schema 对象塞进 schema 键,structured outputs 静默失效。
修复: Chat response_format.json_schema 已展开为 Responses text.format 的 type/name/schema/strict。验证: cargo test 通过。

## 7. [已修复] 裸 error 事件被误报为成功
responses_api.rs:914-930 — error 事件设 finished=true,主循环当作成功补发 [DONE],该请求的 usage 被记为成功、不触发 failover。对比 response.failed 分支才是对的。
修复: 裸 error 事件不再标记 finished,流翻译器会按 responses_stream_error_message 返回失败 outcome。验证: cargo test 通过。

## 8. [已修复] chat→responses 流客户端断开后不清理
streaming.rs:396-649 — 全部 let _ = send(...) 忽略错误。客户端断开后 spawned task 仍消费上游到完,浪费上游 token/配额,还占着熔断器 inflight 计数。
修复: chat_sse_stream_to_responses 使用 tx.closed() 与上游 stream.next() 并行等待,客户端断开后无需等待下一个上游 chunk 即可停止消费并返回 StreamOutcome::failed("client disconnected");终局 SSE 发送失败也按断连处理。验证: 新增普通断连和上游 pending 断连回归测试,cargo test 109 项全部通过。

## 9. Chat/Responses 参数兼容缺口会静默降级
responses_api.rs:160-191 + google_ai.rs:30-116 + anthropic.rs:96-116 — Chat→Responses 只映射少量字段,reasoning_effort、metadata、parallel_tool_calls、tool_choice 细节、response_format json_schema 的 name/strict 等兼容字段没有完整保留。Google/Anthropic 翻译层也缺 seed、logprobs、top_logprobs、frequency_penalty/presence_penalty 等 OpenAI 常见参数的明确处理或拒绝,客户端以为参数生效,实际被静默丢弃。

## 10. daily_token_limit 并发下可被突破
handlers.rs:63-69/88-94/113-118 — 每个请求在转发前调用 enforce_daily_token_limit,usage_log.rs:433-449 只按已落库用量求和,没有为 inflight 请求预留 token。多个并发请求会同时通过检查,最终总量可能远超 daily_token_limit。该限制目前只能算事后近似限额,不应作为硬配额承诺。

# 二、必修:工程/CI(当前最大的系统性漏洞)

## 11. CI 从不跑测试
.github/workflows/build-release.yml — 唯一 workflow 只在 push tag v* 时打包,PR/push 不触发任何检查,1725 行测试从不进 CI,无 clippy、无 fmt。建议加独立 ci.yml,在 pull_request/push main 跑 cargo fmt --check + clippy -D warnings + test。

## 12. UI 同步 HTTP/SQL 冻结主线程

providers.rs:644-723 sync_upstream_models 用 reqwest::blocking 在 egui 主线程,最长冻结 60s。
logs.rs:337-390 每 1s、analytics.rs:247-267 每 2s 在 UI 线程跑多条聚合 SQL,大表明显卡顿。应改 spawn_blocking + 回填。

## 13. 配置每帧全量深克隆写锁
desktop.rs:354-358 — sync_config_to_runtime 每帧无条件 *handle.write() = Arc::new(cfg.clone()),60Hz 持续抢 proxy 的读锁。加版本号/hash 变更检测即可。

# 三、应修:数据安全与文档脱节

## 14. 文档与实现严重脱节(三个 agent 确认)

README.md:10 说"不再提供 /admin 或 /chat WebUI",但 web/admin.rs + proxy.rs:796 实际挂载了 /admin /user。
docs/doc.md:139 说 provider_type 只有 openai/anthropic,实际还有 google_ai_studio、codex_only。
doc.md 的 max_concurrency 默认写 5,代码是 None(运行时兜底 5)。

## 15. config 无语义校验
config.rs:207-215 — session_ttl_hours(文档 1-720)、port 等都不校验。且 #[serde(flatten)] extra 会静默吞拼写错的字段名。建议加 validate() + 未知字段 warn。

## 16. Web 登录面缺少速率限制/失败审计
web/auth.rs:32-67 + web/admin.rs:116-146 — 用户 API Key 登录和 admin_key 登录都是无限次同步比较,没有 IP/会话维度限速、失败计数、冷却或失败日志。若 server.host 配成 0.0.0.0 或被反代暴露,API Key/admin_key 可被在线枚举。

## 17. Web 会话 Cookie 作用域偏宽且未按 HTTPS 设置 Secure
web/auth.rs:59-65 — 用户端 routehub_user_session 使用 Path=/,会随 /v1/* 代理请求一起发送,扩大无关接口的 Cookie 暴露面;web/admin.rs:138-144 管理端限定 Path=/admin 相对更收敛。两类 Cookie 都没有在 HTTPS 部署场景下设置 Secure,需要结合 host/scheme 或配置项收敛。

## 18. serde_yaml 已停止维护 — 建议迁移 serde_yaml_ng。

## 19. Windows/Linux 无系统托盘、CJK 字体仅 macOS(两个 agent 确认)
— common.rs:366 字体候选只有 macOS 路径,Windows/Linux 中文渲染成豆腐块;托盘 #[cfg(macos)] 硬门控。

# 四、缺失的测试(载重路径无回归网)
- SE 跨 provider 续接(README 核心卖点)无端到端测试。
- Gemini 只有 2 个测试(流式稳定 id/created、tool_call 跨 chunk 拼装、usage、错误映射全缺)。
- /responses→chat 回退(即上面 bug #1)、include_usage 4xx 回退、model-fallback 端到端遍历都没测。
- daily_token_limit 并发突破、Web 登录限速、Cookie Path/Secure 属性没有回归测试。
- SQLite read_api_key_today_tokens 只有 ts 单列索引,缺 (api_key_id, ts) 复合索引;usage_log 表无保留清理,无限增长。
