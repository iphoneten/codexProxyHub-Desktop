# 开源 AI 网关调研与 RouteHub 可借鉴能力

核查日期：2026-10-27。范围按“优秀开源 AI 网关”理解，不专指游戏服务。
后续实现已取消渠道熔断/半开并改为按次数持续轮询；下文基线保留调研时的情况，当前行为见 [配置与路由说明](doc.md#路由和故障转移行为)。

## 结论

优先补齐可解释的路由诊断、错误分类与请求尝试链路、协议兼容回归测试，再考虑价格与预算。建议复用现有 Rust / axum / egui / SQLite 结构，借鉴设计而不是把另一套完整网关嵌入桌面应用。

本文是调研建议，不代表功能已实现；本次未安装外部网关、引入运行时依赖或复制第三方实现。

## RouteHub 当前基线

以源码调用链为准，不把 README 中的历史规划当作当前缺口：

- `src/proxy/routing.rs`：模型与别名匹配、渠道权限筛选、优先级、权重、健康状态及在途负载排序。
- `src/proxy/handlers.rs`、`upstream.rs`、`streaming.rs`：请求转发、协议适配、故障转移及流式处理；已经有重试、取消、熔断与无限重试选项，不需要重新实现整套路由器。
- `src/proxy/auth.rs`、`src/desktop/auth.rs`：API Key 的模型/渠道限制、并发控制、每日 Token 限额。
- `src/proxy/request_layer.rs`、`usage_log.rs`、`src/desktop/logs.rs`：请求 ID、请求用量、首字和总耗时、错误记录。
- `src/desktop/providers.rs`：渠道管理、模型同步、模型映射和显式 Responses 模式。
- 入站 `/v1/messages` 当前只承接 Anthropic 渠道；不能把已有的部分协议转换描述成四种协议完全双向兼容。
- `src/proxy/cache.rs` 和缓存配置结构已经存在，但当前 `src/proxy.rs` 没有声明该模块，不能仅凭文件存在就认定响应缓存已接入运行链路。

## 候选项目

下表核查的是所链接分支与官方文档，不代表这些能力全部包含在某个稳定发行版。接入成本是针对 RouteHub 的工程估计，不是项目方承诺。

| 项目 | 技术与许可证 | 最值得借鉴 | 对 RouteHub 的接入判断 |
| --- | --- | --- | --- |
| [LiteLLM](https://github.com/BerriAI/litellm) | 当前有 Python 与 Rust 代码；根许可证明确非 `enterprise/` 内容为 MIT，企业目录单独授权 | 模型价格/上下文目录、按错误类型配置重试、RPM/TPM 感知路由 | 价格数据结构与小范围策略借鉴成本中等；不建议整体嵌入代理服务 |
| [Bifrost](https://github.com/maximhq/bifrost) | Go；Apache-2.0；本次核查默认 `dev` 分支 | `failure_class`、每次尝试与故障转移的可追溯记录、流式开始阶段的错误处理 | 诊断与错误分类最贴近当前问题；复用 RouteHub 日志结构，成本中等 |
| [Portkey Gateway](https://github.com/Portkey-AI/gateway) | TypeScript；核查 `main` 分支为 MIT | 配置校验的字段路径提示、请求前后处理、可配置故障转移 | 优先借鉴校验结果和展示形式，成本低至中；不引入 Node 服务 |
| [New API / RelayKit](https://github.com/QuantumNous/new-api) | Go；AGPL-3.0，另有署名与原项目链接要求；RelayKit 仍适用 AGPL | 四协议转换矩阵、转换质量/损耗诊断、工具调用与流式 golden tests | 测试方法与行为规范有参考价值；直接复用 Go 模块成本高且需评估许可证义务 |

### 版本和商业边界

- GitHub API 核查时上述仓库均未归档。LiteLLM、Bifrost 的 `pushed_at` 为 2026-10-27，New API 为 2026-10-25，Portkey 为 2026-05-25；这只是仓库推送元数据，不等于稳定版发布日期或质量保证。
- Portkey `main` 的 README 将 Gateway 2.0 标为预发布，并指向 `2.0.0` 分支。不要把 2.0 或托管/企业功能自动当作 `main` 的现成功能。
- Bifrost 文档包含企业自适应路由、集群等内容。本文不把这些能力当作 Apache-2.0 社区版可直接复制的实现。
- LiteLLM 根许可证明确排除 `enterprise/`；不能因为 GitHub 项目公开就忽略目录级许可证。
- New API 的 `NOTICE` 要求修改版保留相关署名和原项目链接；RelayKit README 明确其沿用根目录 AGPL。是否直接复用需要结合 RouteHub 的分发与许可证方案单独评估。
- 未运行这些项目，也未复现其宣传性能数据；不据此比较网关吞吐和延迟。

## 建议落地顺序

### P0：渠道筛选原因与配置生效状态

**收益：** 把笼统的“没有可用渠道”解释为模型不匹配、渠道未启用、密钥无渠道权限、协议不匹配等具体原因。

**最小做法：**

- 在现有筛选条件旁生成诊断信息，在桌面端展示“请求模型 → 映射模型 → 入站接口 → 上游接口 → 候选/排除原因”。
- 显式 `native` / `chat` 与能力提示分别展示，不能让旧能力提示悄悄否决显式模式。本次 native 修复只解决这一处筛选冲突。
- 首阶段提供本地只读诊断，不发送探测请求、不消耗额度、不写虚构用量，也不能因诊断而推进轮询计数或改变熔断状态。
- 未获授权的客户端只返回简要错误和请求 ID，完整渠道清单与权限判断仅在本地管理界面展示。

**涉及位置：** `src/proxy/routing.rs`、`src/proxy/handlers.rs`、`src/desktop/providers.rs`。先使用现有运行日志，无需改用量数据库。

**来源：** Portkey 的字段级配置校验 [S3]、Bifrost 的路由尝试记录 [S1]。这是针对 RouteHub 的设计建议，不宣称两者已经具备完全相同的离线诊断界面。

### P1：错误分类与一条请求的完整尝试链路

**收益：** 区分本地权限拒绝、上游密钥失效、模型无访问权、余额耗尽、临时限流、服务器错误和客户端取消，避免把不同故障都解释成“模型不可用”。

**最小做法：**

- 复用现有请求 ID，增加尝试序号、分类、状态码、等待时间、所用接口和切换原因；将同一请求的多次尝试聚合展示。
- 先记录分类，不立即改变现有重试策略。后续若调整永久错误处理、总重试预算或无限重试语义，需单独确认兼容性和默认行为。
- 优先识别结构化错误码，再使用受控的消息特征；只记录分类，不把可能包含密钥或用户输入的上游原始错误体带入诊断。

**涉及位置：** `src/proxy/upstream.rs`、`src/proxy.rs`、`src/runtime_log.rs`、`src/desktop/logs.rs`。

**来源：** Bifrost 错误分类源码及尝试链路说明 [S1]；LiteLLM 按错误类型配置重试 [S4]。

### P1：协议兼容矩阵、损耗提示和固定样例测试

**收益：** 明确“可以转换”不等于“全部字段无损”，降低工具调用丢失、流式完成事件错误、推理字段不兼容等问题。

**最小做法：**

- 围绕现有转换路径补齐请求、非流式响应、SSE 事件序列的固定输入输出样例，继续使用本地模拟上游。
- 覆盖并行工具调用、分片 arguments、工具结果、usage、完成事件、流内错误、断开取消以及 native/chat/auto 模式与能力标记冲突。
- 对无法表达的字段记录字段路径和原因；不默认删除字段，不自动切换成更有损的协议，也不擅自重放已经输出工具调用的请求。
- 如需扩展入站 Messages 到 OpenAI 等新转换路径，另列需求与兼容性验收，不混入本次 native 修复。

**涉及位置：** `src/responses_api.rs`、`src/anthropic.rs`、`src/google_ai.rs`、`src/proxy/streaming.rs` 及现有测试文件。

**来源：** RelayKit 的转换质量矩阵、`Diagnostics`、`ToolLossPolicy` 与 golden test 方法 [S5]；Bifrost 的流式重试边界 [S1]。借鉴方法不等于直接复制受 AGPL 约束的实现或样例。

### P2：模型价格目录、成本展示与可选预算

**收益：** 在已有 Token 统计之外回答“哪个渠道贵、一次请求花了多少、预算还剩多少”。

**最小做法：**

- 先增加可选的本地模型价格表，区分输入、输出、缓存读取/写入价格，并允许渠道级覆盖；公共目录可作为手动更新的数据来源，不默认联网更新。
- 使用实际上游模型与渠道价格计算，记录价格版本；未知价格显示“未知”，不能按零费用处理或从相似模型名猜测。
- 第一步仅展示估算成本，随后再评估预算拦截；预算实现需要考虑并发预留与结算，而不只是请求前读取一次余额。
- 模型目录中的能力字段仅作提示和辅助验证，不能重新引入“目录旧标记否决 native”的问题。

**涉及位置：** `src/config.rs`、`src/proxy/usage_log.rs`、`src/desktop/analytics.rs`、`src/desktop/auth.rs`。持久化成本属于数据库增量迁移，需要覆盖旧记录和导入导出兼容性。

**来源：** LiteLLM 模型价格与上下文目录 [S6]。RouteHub 的每日 Token 限额与价格预算不是同一种能力。

## 暂不建议直接引入

- 不为本地单实例提前引入 Redis、Kubernetes、分布式集群或另一套管理后台。
- 不把语义缓存作为第一优先级；代码与工具调用场景对精确性、权限隔离、时效性要求较高，应先核实已有缓存代码的接入与隔离边界。
- 不采用默认流量镜像或真实请求回放来测试新网关，以免额外泄露请求内容或产生费用。
- 不照搬云网关的“禁止所有内网地址”策略；RouteHub 需要保留用户授权的本地模型与局域网上游使用方式。

## 已核查来源

- [S1] [Bifrost 重试、错误分类、流式边界及尝试链路](https://docs.getbifrost.ai/features/retries-and-fallbacks)；[分类源码](https://github.com/maximhq/bifrost/blob/dev/core/failureclass.go)；[许可证](https://github.com/maximhq/bifrost/blob/dev/LICENSE)。
- [S2] [Bifrost 插件目录](https://github.com/maximhq/bifrost/tree/dev/plugins)，含 logging、governance、mocker、otel 等；仅核查目录，不代表已对所有插件做完整审计。
- [S3] [Portkey 配置校验源码](https://github.com/Portkey-AI/gateway/blob/main/src/middlewares/requestValidator/index.ts)；[README 与版本边界](https://github.com/Portkey-AI/gateway/blob/main/README.md)；[许可证](https://github.com/Portkey-AI/gateway/blob/main/LICENSE)。
- [S4] [LiteLLM 路由、重试及速率控制文档](https://docs.litellm.ai/docs/routing)；[根许可证与 enterprise 排除条款](https://github.com/BerriAI/litellm/blob/main/LICENSE)。
- [S5] [New API RelayKit 转换模块说明](https://github.com/QuantumNous/new-api/blob/main/relaykit/README.md)；[根许可证](https://github.com/QuantumNous/new-api/blob/main/LICENSE)；[NOTICE](https://github.com/QuantumNous/new-api/blob/main/NOTICE)。
- [S6] [LiteLLM 模型价格及上下文目录](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)。

上述链接指向可变分支或在线文档；真正复用前应固定提交版本、复核对应许可证并执行本项目的离线兼容测试。
