# Provider 与运行时兼容合同

## 1. 核心边界

ChatGPT Switch 的核心职责是安全切换本机请求路由。核心事务只处理受管 Provider 配置、官方 `auth.json` 原样保持、必要的会话数据库视图、终态账本和受控应用启动；不在切换事务中探测网络、扫描全部会话正文或执行 GC。

“高级存储”是独立能力域，包含迁移、冲突处理、恢复导入、降级和离线清理。它依赖更严格的数据库结构门禁，不应扩大普通切换的可信代码路径。

## 2. Relay Provider 合同

受管 Relay 固定使用：

- 无内嵌凭据、query 或 fragment 的 HTTP(S) Base URL；远程地址必须 HTTPS，loopback 可 HTTP；
- Bearer API Key；
- `wire_api = "responses"`；
- 用户配置的模型 ID；
- `supports_websockets = false` 的 HTTP Responses 默认值，或用户明确选择的 WebSocket。

Base URL 没有路径时补 `/v1`；已有显式路径时不擅自追加。传输能力属于 Provider 配置，不从“中转站”名称、HTTPS scheme 或反向代理品牌推断。

## 3. 成功语义

“本地请求路由已应用”只证明 `config.toml`、认证快照、会话视图和本地运行态满足写后合同。它不证明：DNS/TLS、API Key、模型、额度、计费、HTTP Responses 或 WebSocket 远端可用。实际连通性由用户切换后的真实请求验证。

## 4. 运行时兼容门禁

应用只读采集可信 AUMID 对应的 ChatGPT/Codex Windows 包名称、family 和版本，作为诊断信息；不按包版本建立脆弱白名单。实际能力由当前 `state_5.sqlite` 判断：

- regular/non-reparse 文件身份前后一致；
- SQLite 只读打开并通过 `PRAGMA quick_check`；
- 记录 `schema_version` 和按列定义计算的 SHA-256 指纹；
- 会话视图要求 `threads.id` 与 `threads.model_provider`；
- 高级存储还要求 `threads.rollout_path`。

数据库缺失时允许 fresh-home 请求配置和 Relay bootstrap。数据库损坏、路径不可证明、文件持续变化或关键列缺失时，对应写能力 fail closed，并返回稳定错误码和恢复策略。

## 5. SQLite sidecar 不变量

共享全局数据库只允许相同 file identity 的 hard link。已建立视图时，只在 active 路径执行 checkpoint；inactive 路径的 `-wal`、`-shm`、`-journal` 必须在 checkpoint 前后均不存在。未建立视图的 Relay 目录只允许主数据库文件，任何 sidecar 或未知条目都阻止自动接管。

## 6. 错误与恢复

Provider/兼容性边界使用稳定错误码和恢复分类：重新配置、重试、关闭 writer 后重试、人工排查或不支持的客户端。UI 不根据自由文本反解析控制流。

## 7. 发布证明

CI 固定 Node/Rust/direct dependencies，执行前端和 Rust 全量质量门、npm 生产依赖审计、cargo-deny advisories/source/license 检查、CycloneDX SBOM、raw/packed EXE 合同，并对最终 EXE生成 GitHub/Sigstore 构建来源与 SBOM attestation。Windows Authenticode 仍是独立未满足的外部证书边界。

## v0.3.5 主线整合补充

兼容性检查在 blocking worker 中执行，SQLite 完整性、schema version 与列信息在同一个只读快照中采集。线程 ID 必须是独立 TEXT 主键，不接受复合主键或含 INT 亲和性的伪 TEXT 声明。高级存储在 writer 关闭后、进入执行器的文件屏障前重新检查结构；持锁后使用执行器已有的冻结字节／schema／文件身份验证和 writer 复查，不再通过 SQLite 重开受保护的数据库。该次结构检查不重复扫描全库，原有完整性和回滚证明仍独立执行。恢复／回滚入口保留原有 writer 检查，不因受损 schema 无法通过新功能检查而阻断数据恢复。

共享数据库在取得既有 write-exclusion handle 后复核 source/target 的 WAL、SHM、journal；创建硬链接在句柄移交、发布前后复检。它不把“主文件 identity 未变化”单独当作无日志证明。错误信封对消息、phase 和编码后总大小分别限长，不因 Unicode 或 JSON 转义退化成无界消息。

组件清单只将当前工作区包的本地身份替换为稳定 cargo PURL，并同步图引用；未知依赖路径仍拒绝。文档检查覆盖 Windows/POSIX 路径包含关系以及首页 HTML 图片，生成 SBOM 不进入 Git。main 的 0.4.0 是开发版本；公开发布、签名证明和更新器验收在后续发版时另行执行。
