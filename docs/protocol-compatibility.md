# 协议兼容范围与本地验证

本文区分原生转发和协议转换，不把“可以转发”视为所有字段都能无损互转。
后续的持续轮询与取消熔断改造见 [路由和故障转移行为](doc.md#路由和故障转移行为)；下文“本轮”指协议兼容修复本身。

## 现有链路

| 客户端接口 | 上游接口 | 处理方式 |
| --- | --- | --- |
| Chat Completions | OpenAI Chat Completions | 原生兼容转发 |
| Chat Completions | OpenAI Responses | 上游 Chat 返回 404/405 后使用现有反向转换路径 |
| Chat Completions | Anthropic Messages / Google GenerateContent | 使用现有渠道类型对应的转换器 |
| Responses | OpenAI Responses | 原生转发；显式 native 不再受旧 supports_responses 标记拦截 |
| Responses | Chat Completions | 显式 chat 或现有 auto 故障回退路径 |
| Responses | Anthropic / Google | 沿用现有经 Chat 中间格式的转换路径 |
| Anthropic Messages / count_tokens | Anthropic | 原生直通；不扩展到 OpenAI / Google 渠道 |

Anthropic / Google 的已有协议特例仍然保留；native 修复不是将所有渠道类型强行改成访问 `/responses`。

## 本轮修复覆盖

### 工具调用

- Responses → Chat：`function_call_arguments.done` 是完整快照，不再把完整 JSON 追加到已经发送的参数分片后面；只补未发送后缀。
- Responses → Chat 请求历史：同一轮相邻的并行 function/custom 调用合并到一个 assistant 消息，保留文本、调用 ID 和后续各自的 tool 结果。
- Chat → Responses 流：即使参数分片早于工具名称到达，后续也会补发此前缓存的参数；各个工具槽位独立积累，不串线。
- 保留已有 custom 工具包装及还原逻辑，不把其描述成所有自定义工具语法完全无损。

### 流式结束与用量

- Chat 的 `finish_reason` 后可能还有独立 usage 块。转换器与用量探针最多等待 500 毫秒，收到 `[DONE]` / EOF 时提前收尾；尾部连接错误不推翻已确认的完成状态。超过等待窗口才到达的用量可能无法收集，不为用量无限挂住客户端。
- 无 `finish_reason`、也无 `[DONE]` 的意外 EOF 不能合成成功的 `response.completed`，改为输出失败并报告失败结果。
- 保留只提供 `[DONE]` 的兼容行为，以及有 `finish_reason` 后正常关闭连接的行为。
- Chat 的 `length` / `content_filter` 映射到 Responses 的 `incomplete` 和对应原因，流式与非流式保持一致，不伪装成完整回答。
- 首帧探针区分有效正文和合法结束：没有正文但提供了合法 `finish_reason` 的结果可交付，不能误判为网络故障反复重试，也不伪造首字耗时。仅有 `[DONE]` 而无正文、工具事件或结束原因的无效流仍按原规则处理。
- `response.failed` 转为 Chat 错误对象，而不是正常的 `finish_reason: stop`。
- 原生 Responses 的 `response.completed` 和 Anthropic 的 `message_stop` 均立即结束下游与对应统计通道，不等待上游长连接关闭。已有客户端取消和服务关停机制不变。
- 首个业务输出前的日志由尝试收尾守卫管理：正常交付后沿用已有结果记录；取消、断开或 future 被丢弃时记录为 `aborted`，SQLite 收尾不阻塞网络取消。只读 `count_tokens` 保留鉴权与模型/渠道权限检查，但不受每日 Token 限额拦截，也不新增用量记录。

### 字段与编码

- Chat 的 developer 消息转为 Responses developer 消息，不再降为 user。
- Chat ↔ Responses 转换保留函数工具的 `strict`、`parallel_tool_calls`、`reasoning_effort` ↔ `reasoning.effort`、JSON Schema 输出格式及 description、图片 detail；不代表其他协议也支持所有这些字段。
- SSE 的 UTF-8 字符可能跨网络分片；共用增量解码函数保留不完整字节，避免中文、emoji 和工具 JSON 内容被替换成乱码。
- 增量解码覆盖 Chat/Responses 转换器、Anthropic/Google 转换器、首输出探针、用量探针和现有 thought 过滤路径；原生透传的字节本身不因解析而重写。

## 已知损耗与边界

- Responses 的 `previous_response_id` 不能仅凭 ID 在 Chat 上游还原历史；应发送完整 input，或使用支持原生 Responses 的上游。
- `reasoning.summary` 与部分托管工具没有当前 Chat 转换器的对应字段。
- 对上述字段及当前不支持转换的工具类型，新增运行日志提示，指出转换方向和字段路径；不记录请求值、工具内容、原始模型名称或凭据。
- 这些提示沿用既有降级/省略行为，不新增硬拒绝，也不是一套覆盖所有未知字段的完整损耗检测器。
- 本轮未实现所有多模态、托管工具、推理加密内容或各家专用扩展的完全互转。未调整重试、熔断、权限、配置存储或数据库结构。
- 上游终止标记、SDK 版本和实现差异仍需结合具体渠道验证；本轮回归使用合成数据和本地模拟，不消耗真实账号额度。

## 开发环境与验证

使用官方 rustup 稳定版工具链，macOS 还需要 Xcode Command Line Tools。工具链安装时若选择不修改 Shell 配置，每个新终端先执行：

```sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

本轮环境为 macOS arm64、Rust/Cargo 1.99.1，已安装 rustfmt 和 Clippy。未修改个人 Shell 启动文件；若 IDE 未继承 PATH，可指向 `$HOME/.cargo/bin` 中的工具。

```sh
# 全部本地测试，包括既有 native 模式回归
cargo test --locked

# 本轮协议相关回归
cargo test --locked protocol_

# 开发构建与静态检查；不启动代理服务
cargo build --locked
cargo clippy --locked --all-targets
```

测试入口为 `src/proxy/protocol_tests.rs`、`src/responses_api.rs` 的内置测试，以及已有 Anthropic、用量、取消和端到端模拟测试。固定样例同时覆盖一次交付与逐字节交付，不依赖在线模型响应。

开发构建输出为 `target/debug/route-hub`（Windows 为 `.exe`）。构建不会替换已安装的应用，也不会自动停止或启动服务；启动开发实例前应检查监听端口，避免与正在使用的实例冲突。

### macOS 窗口初始化兼容

macOS 26 及更新系统可能在旧 `eframe 0.28 → winit 0.29 → icrate 0.0.4 → objc2 0.4.1` 链路中触发 `NSScreen` 枚举的 `q` / `Q` 类型编码校验 panic。这发生在 GUI 初始化阶段，普通构建与协议单元测试无法覆盖。

目前在 `Cargo.toml` 中仅对 `objc2:0.4.1` 设置 `profile.dev.package` 的 `debug-assertions = false`，临时避免该旧库的开发模式校验崩溃；测试 profile 也继承此覆盖。它不关闭 RouteHub 自身或其他版本 objc2 的调试检查，也不改变 release profile，但会减少该旧依赖的运行时校验。未来升级窗口依赖链时应复核并移除这个临时兼容项。

参考：[winit #4299](https://github.com/rust-windowing/winit/issues/4299)。新版的 `relax-sign-encoding` 不存在于当前锁定的 objc2 0.4.1 中，不能只给已有的 objc2 0.6 依赖加开关来修复这条旧链路。

协议字段参考：[OpenAI Chat SDK 类型](https://github.com/openai/openai-node/blob/master/src/resources/chat/completions/completions.ts)、[Responses SDK 类型](https://github.com/openai/openai-node/blob/master/src/resources/responses/responses.ts)。链接内容会随上游版本变化，本项目实际兼容性以固定样例测试和渠道验证为准。
