# Changelog

## v0.3.1 - 2026-08-30

### Fixed

- 修复未登录官方 ChatGPT 时无法配置或切换 API 中转站的问题。Relay 目标现在允许 `auth.json` 不存在或为可解析的非官方状态，并根据冻结的登录态设置 `requires_openai_auth`；切换过程不创建、替换或伪造官方登录文件，Account 目标仍明确要求有效官方登录。
- 修复 fresh Codex Home 缺少 `config.toml` 或 `state_5.sqlite` 时 Relay 保存/切换失败的问题。缺失配置可从最小受管模板原子创建并在失败时恢复为不存在；空 Relay 会话视图使用带所有权证明的 bootstrap 状态，不伪造官方数据库，首次产生 Relay 数据后可安全物化 Account 视图。
- 修复后台自动 GC 轮询占有全局 mutation lock、导致前台切换偶发 `mutationBusy` 的竞争。writer 存在或 Shadow 待稳定时先无锁等待，真正恢复/写入前才加锁并二次校验；请求路由成功后只排队 coalesced Shadow。
- 修复切换失败只能依赖字符串判断的问题。认证、配置、会话视图、独立 writer、真实并发、进程关闭、路由验证和启动目标现在以稳定 camelCase reason 从 Rust/Tauri 传到 UI，并显示对应恢复建议；路由成功与 ChatGPT 启动失败继续作为两个独立事实。
- 修复多个可信 ChatGPT AppUserModelID 同时注册时重启目标偶发歧义的问题。应用只持久化最近验证且仍在可信白名单/注册表中的 AUMID；没有可信证据时保持 fail-closed，不猜 EXE 或 PATH。

### Safety and compatibility

- 已有官方登录、Relay Key DPAPI 槽位、Account/Relay 两槽合同和 `supports_websockets = true` 规则保持兼容；已有官方登录在 Relay 切换前后仍按字节保持不变。
- 配置回滚新增外部替换保护：只有 live bytes 仍等于本次 operation 写入值时才恢复旧快照，避免覆盖并发外部写入；独立 Account/Relay bootstrap 数据库冲突继续 fail closed。
- 本版本的本地/CI/公开资产与 updater 证据必须绑定精确 `v0.3.1` 提交；历史 `v0.3.0` 结果不作为本版本发布证明。最终文件大小、SHA-256 和公开更新结果以 [GitHub Release v0.3.1](https://github.com/mingisrookie/codex-switch/releases/tag/v0.3.1) 为准。

## v0.3.0 - 2026-08-15

> 本版本只有在精确旧版运行时、最终真实库只读预检、完整质量门、tag-CI、公开资产回下载、updater 首跳与回滚全部闭环后才构成正式发布。

### Canonical 会话存储

- 新增统一 `SessionReferenceGraph`，覆盖 OpenAI 主目录、Account、Relay、Shared、legacy/relocated 与备份库存；逐文件记录逻辑 thread、全部数据库引用、canonical/provider-slot 来源、写入状态、大小/hash 和与保留版本的 Equal / EqualExceptProvider / Prefix / Divergent / Unknown 关系。备份引用不再被误当成运行时保留证明。
- 新增语义 JSONL 解析和回合 provenance：按有效消息序列、工具调用/结果关系和内容 hash 判定，不使用 Switch 文件名时间、mtime、大小或 provider 字段单独决定新旧。非法 JSONL、工具链断裂、顺序变化、真实双尾或无法判断的历史都 fail closed。
- Account、Relay 与 Shared 改为数据库视图和请求路由；canonical 就绪后继续同一逻辑 threadId，不再按 provider 永久物化完整正文。每轮实际 provider/model/account 来源由 route epoch/provenance 记录，不改写首轮来源伪装 provider。
- 删除旧“完全同步”语义，改为“会话合并与修复”：只处理缺失、完全相同、仅 provider 元数据不同、严格完整延续和数据库视图/canonical 路径修复。本地合并与 Relay 发布保持独立流程。
- 新增合并冲突清单与原子主版本切换。真实分叉默认“不覆盖”；候选时间可靠时才提供较新版本提示。显式覆盖后的旧主版本进入 7 天冲突回收，到期仍需重新证明零引用后才能删除。
- v0.3 生产版移除会话 hard-delete command 与 UI 永久删除入口；旧 v0.2.x hard-delete 实现仅作为 test fixture 保留，不能从日常命令到达。恢复点显式删除和经全局引用证明的 provider 副本 GC 不等同于删除逻辑会话。

### 迁移、备份、GC 与降级

- 新增一次性前台迁移状态机：只读 Preflight → 完整备份 → 真实 Codex 隔离恢复验证 → 原子计划 → Apply/Validate/Commit；取消、失败和启动恢复均由持久操作账本驱动。未迁移时普通账号切换继续可用，但合并修复、历史清理和离线 GC 禁用。
- 迁移要求所有 Codex writer 退出、SQLite/WAL 一致快照、容量与稳定性预检。完整备份由用户选择位置、未加密且不上传；必须在隔离目录恢复，并由真实 Codex 运行时验证列表、读取和恢复后才标记 `runtimeVerified`。
- 新增 fail-closed 安全窗口 GC：在线阶段固定只扫描，成功切换、迁移、冲突处理、待恢复导入和会话合并只合并排队新 Shadow。自动清理开启时，只有迁移已提交、scan identity 新鲜、零未完成操作且实时确认全部 writer 关闭，后台才调用与手工入口相同的离线执行器；窗口未形成时零删除并等待后续安全触发校准。候选仍必须同时具有有效 Switch marker、确定 canonical peer、Equal/Prefix、连续两次未变化、全部运行时数据库零引用且无活动句柄/写入。
- 新增旧备份整理与“待恢复会话”：重复内容删除、主库缺失会话提取待用户决定、完整延续允许恢复、真实分叉进入冲突；无法读取或验证的旧备份不会写入 canonical。诊断事件、已终结操作/迁移审计、恢复包及其 manifest、冲突回收均按 7 天隐私生命周期管理；未终结或仍被恢复载荷引用的账本先保留。自动清理开关只控制 provider 副本 GC，关闭后继续扫描/报告，也不暂停这些隐私 TTL。
- 新增 v0.2.0–v0.2.7 显式隔离降级导出和再次升级导入。旧版只使用自描述隔离存储，canonical 保持不动；相同/完整延续自动合并，旧版期间新增会话导入，分叉走统一冲突流程。v0.2.x 数据库/索引保存包内绝对路径，因此 UI 与包内 README 明确要求直接选择最终目录，生成后不移动；换盘必须从 canonical 重新生成。
- 新增本地“交给 Codex 排查”任务生成：只包含问题分类、脱敏路径、schema/version、引用/完整性摘要和只读检查建议，不包含会话正文。

### UI、隐私与发布工程

- 按产品决策移除独立中转站连接验证、切换前 `/models` 探测及 Validate/Direct 选择；切换不再因外部网络探测阻塞，仍保留本地 URL、凭据、官方登录态和写后请求路由校验。历史 `verifyRelay` 日志/枚举字段仅保留为旧数据兼容，不再有生产命令入口。
- 顶部新增“存储”页，集中展示存储状态、canonical 会话、安全副本、冲突、已回收字节和安全窗口状态；提供迁移进度、冲突、待恢复、自动清理、本地排查和降级导出入口。在线始终只扫描；日常安全扫描/离线清理不弹窗，阻断问题才提示。
- 会话存储日志和回执只记录脱敏路径、阶段、大小/hash、引用数、删除/保留计数与失败分类；禁止用户消息、模型回复、工具输出、Prompt、附件、凭据和冲突正文。无云端遥测或远程功能开关。
- Tauri 关闭默认 runtime Brotli asset feature，仅保留 `wry` 与 Windows `common-controls-v6`；Release staging 使用固定官方 UPX 5.2.0 `--ultra-brute --lzma`，raw 不原地修改，packed 仍受 3,000,000 bytes、PE32+ x64、双版本、release contract 和 `upx -t` 硬门约束。
- Windows-only 网络客户端改用系统 Schannel（`reqwest` `native-tls`），保留 HTTPS、超时、重定向白名单和证书校验边界，同时避免在单文件 EXE 中静态携带第二套 TLS 实现。

### 发布验证边界

- 任何旧的测试输出、runtime/UI 截图、raw/packed hash 或本地证据都不能继承到变化后的工作树；所有质量门必须针对精确发布工作树重新执行并绑定源输入。
- 真实主库只允许只读 preflight；迁移、回滚、离线 GC 和故障注入只在本机隔离副本执行，不能以历史样本或静态检查替代运行态结果。
- v0.2.0–v0.2.7 旧版运行时和 updater live gate 必须验证精确旧版 EXE 的 list/resume/continue/graceful close、公开资产更新成功与替换锁回滚。
- 独立审查、tag-CI、公开 Release 回下载和 updater 成功/回滚是本版本发布记录的一部分；任一门失败即不得把对应资产声明为稳定 Latest。

## v0.2.7 - 2026-08-10

> `v0.2.7` 已正式发布为 GitHub latest stable。`v0.2.6` annotated tag 在正式 Release 前被最终独立隐私审查拦截，未创建 GitHub Release/Latest；旧 tag 未移动，门禁修复统一进入 `v0.2.7`。

### Added

- 新增独立于 durable `logs/operations.jsonl` 的诊断事件管线：所有用户主动 mutation 都在 lock/spawn/早退前从 command boundary 创建随机 attempt，记录 start/真实 phase/关键安全 branch/typed terminal；拿到 durable operation ID 后只绑定关联，等命令后置步骤完成再按 cleanup/mobile/launch/block/rollback 等真实语义封闭 terminal。operation ledger 持久化失败会记录安全 branch/typed 结果，但诊断始终 fail-open，不改变原业务返回。无 durable record 的 update/process/continuity/exit 也覆盖完整 lifecycle；普通后台成功刷新不写持久诊断，只有失败进入事件管线，前端未处理 `error` / `unhandledrejection` 只提交固定分类和安全文案。
- 新增 `%APPDATA%\codex-switch\logs\diagnostics\events-*.jsonl` 有界 store。事件在落盘前执行字段 allowlist、长度/深度/数量限制和集中 sanitizer；Windows 同一诊断根的 append/read/status/prune/clear 使用 root-scoped named mutex 跨进程协调，默认总量上限 10 MiB、单段 512 KiB。segment 按文件名中的创建时间轮转/prune，读取再按 event `timestamp` 精确过滤最近 7 天；容量可让实际窗口更短。dirty tail 会封存并换段，内部损坏、未知 schema、祖先 reparse 或非 regular segment fail closed；事件已 `sync_data` 后的 post-prune 失败不否定 durable append，也不改变应用启动或业务 mutation 结果。
- 新增启动生命周期与 panic best-effort 记录：Windows session/attempt ID 使用 OS CSPRNG、`CoCreateGuid`、进程局部 fallback 的降级链；尽早写入带 PID/timestamp unit 和 best-effort process creation stamp 的 `sessionStarted`，在 Tauri Ready/正常 Exit 记录 `appReady` / `sessionEnded`，下一次启动在 PID + creation stamp 可用时排除仍存活实例和 PID reuse 后再判断 `previousSessionUnclean`。operation terminal 真正持久化后封闭关联状态，拒绝迟到 phase/branch/重复 terminal；release `panic = "abort"` 保持不变，不引入 watchdog 或崩溃恢复。
- 顶部工具栏新增页面内“诊断与支持”面板，显示可用性、事件数、占用和保留上限，并提供“导出最近诊断”“打开日志目录”和只清理诊断事件的确认流程。已接入且具有真实关联 ID 的失败面可直接“导出本次诊断”；两个入口共用同一后端选择、脱敏与打包逻辑。
- 新增固定五文件诊断 ZIP：`README.txt`、`manifest.json`、`diagnostics.jsonl`、脱敏后的 `operations.jsonl` 子集和 `health.json`。manifest 记录版本、时区化导出时间、选择窗口、脱敏策略、负载 bytes/SHA-256 与 unavailable/warning，并以 `timestampUnit = unixEpochMilliseconds` 明确事件 `timestamp` 的 Unix epoch 毫秒单位；ZIP 在发布前后执行 allowlist、hash 和敏感形态自检。
- 默认导出使用 Windows `FOLDERID_Downloads` Known Folder 解析真实下载目录，以 `ChatGPT-Switch-Diagnostics-<local-timestamp>.zip` 保存。staging 与目标位于同一目录，最终使用 write-through、no-clobber 原子发布；同名时追加数字后缀且不覆盖旧包，成功后可打开所在位置。下载目录失败后，用户可显式重试或主动改存固定的 `%APPDATA%\codex-switch\diagnostic-exports`；不会静默 fallback，也不接受任意路径。
- `health.json` 新增只读 best-effort collector：记录应用/平台、存储摘要、当前 route/auth mode 结构、受管 ChatGPT/独立 Codex 进程计数，以及四个受管 SQLite 的 present/readable/bytes/`schema_version`；采集失败逐项标记 unavailable。

### Security and boundaries

- 诊断事件落盘前脱敏，导出时再次删除禁用字段、替换受管路径/身份字面值并扫描秘密形态和未知绝对路径。自由文本中的 canonical 业务 UUID 固定脱敏，诊断结构化关联 ID 保留；正斜杠 UNC、设备路径、通用未知绝对路径和已知根边界均有双层回归。默认包不包含凭据、token、Authorization、聊天/会话正文、原始 JSONL/SQLite、`auth.json`、完整 `config.toml`、请求/响应正文、全量环境变量、进程命令行、Windows WER、机器名、用户名或稳定设备 ID。
- `operations.jsonl` 的 schema、严格解析、原子整文件发布和检查点清理证据保持不变；诊断导出只读并生成去掉备份路径的相关子集。诊断轮转、导出和清除不取得 mutation guard，也不删除操作历史、备份、会话、配置或凭据。
- Known Folder 解析或写入失败时先返回页面内导出错误；只有用户点击“改存应用诊断目录”才写固定备用目录，不会自动改存 APPDATA、桌面或其他位置。默认与备用导出都不覆盖原业务失败/成功终态。
- health collector 只输出结构化摘要与计数，不输出进程命令行、完整配置/认证内容或 SQLite 数据；无法读取的 Windows 版本、route 状态、进程 inventory 或单库 schema 信息必须逐项 unavailable，不伪造空健康状态。
- store 可按 session causal sequence 返回事件，但 status 和 retained-window selection 使用真实 `timestamp` 最小/最大值；operation overlap 先规范化 started/completed 边界，系统墙钟回拨时不会因数组首尾顺序漏选相关终态。
- diagnostics named mutex 使用 Windows `Local\` 命名空间和词法规范化 root 哈希：同一登录会话/同一路径标识可协调，跨登录会话或同目录不同路径别名不保证共享锁；7 天窗口依赖系统墙钟，大幅回拨会影响保留判断。非 Windows 只提供进程内锁和非稳定 ID fallback，不属于当前正式交付目标。
- exporter 在正常返回/错误 unwind 时尽力删除同目录 staging；若进程在最终发布前硬崩溃，Downloads 或固定备用目录仍可能留下 `.chatgpt-switch-diagnostics.<pid>.<sequence>.tmp`。“清除诊断日志”只删 event segments，不负责清理这些 staging 或已导出 ZIP。
- panic hook 只能尽力保留进程仍有机会执行时的最小事件；访问冲突、强制结束、断电、硬卡死和其他来不及落盘的硬崩溃不在捕获保证内。`previousSessionUnclean` 只表示缺少 clean terminal，不能单独证明根因。

### Validation and release status

- 最终本地门禁通过：前端 `139 passed`，TypeScript typecheck 与 production build 通过；Rust `499 passed`、`4 ignored`，fmt、clippy `-D warnings`、完整测试和 release contract 通过。
- 隔离诊断 E2E 覆盖失败关联、强杀重启、`previousSessionUnclean`、固定五文件 ZIP、外部 UUID/路径/秘密形态扫描、Downloads no-clobber/清理和 390×844 七项 UIA hit-test；独立复核结果为 PASS。
- PR CI `31328121903` / `31328135186`、main CI `31328794302`、tag CI `31329458760` 全部通过；annotated tag `v0.2.7` 指向合并提交 `74832037710520e8ecb0d43c0bb163f32d46ca20`。
- [GitHub Release v0.2.7](https://github.com/mingisrookie/codex-switch/releases/tag/v0.2.7) 为 latest stable、非 draft，唯一资产来自 tag CI：`2,411,008` bytes，SHA-256 `E8905135EAB0C5D76117BAA25770371E3D8BFDAF07495805463C96A823A5B78E`。公开回下载与 tag-CI artifact 完全一致，并再次通过 PE32+ x64、双版本、UPX 5.2.0 与两个 live GitHub Release 合同测试。
- 正式 `v0.2.5 -> v0.2.7` 一键更新 smoke 通过：旧版同路径退出并自动替换/重启为公开资产，`CODEX_HOME`、SQLite Home、Relay runtime 和 mobile continuity 均 byte-exact，真实 WebView2 UDF 元数据不变（未读取内容），受保护 ChatGPT/Codex 进程身份不变，staging/install 残留为 `0`。

## v0.2.5 - 2026-08-01

### Fixed

- 修复 Relay 槽位和请求路由把 `supports_websockets` 保存为 `false` 的问题。新旧 Relay 槽位在切换时都会向 live `config.toml` 的受管 provider 写入 `supports_websockets = true`；该字段为 `false` 或缺失时不再误报为当前精确匹配。

## v0.2.4 - 2026-07-29

### Added

- 新增默认开启的“手机连续性”cutover：升级后首次运行只记录现有 thread ID，不扫描或上传旧会话；以后只自动处理 cutover 后新建、未归档且由受管 Relay 创建的会话。
- 新增 `%APPDATA%\codex-switch\mobile-continuity-v1.json` 持久队列，记录 thread、source fingerprint、typed 状态、尝试次数和脱敏失败分类，不保存会话正文、路径内容或凭据。
- 新增隔离的 Relay SQLite 会话视图：切入 Relay 前用 SQLite Online Backup 从 Account 四库生成 `%APPDATA%\codex-switch\relay-sqlite`，只在副本中把 thread provider 归一为 `openai_custom`；Account 原库和共享 `sessions/` 正文保持不变，因此切换请求端后历史会话不会因 provider 过滤而消失。
- 会话页新增 Remote 状态和旧 Relay 会话“同步此会话”入口；首页新增待发布、已发布、部分可见、冲突/需处理总览以及手机连续性开关。
- Relay 首次切换或地址/凭据变化后新增页面内选择：“验证连接后切换”或“直接切换”；选择随当前 Relay 配置保存。

### Changed

- live `auth.json` 从 preflight 到写后校验始终 byte-exact 不变。Relay provider 显式写入 `requires_openai_auth = true`，让 Desktop 继续识别官方 ChatGPT 账户；实际 Relay 请求仍使用该 provider 的 `experimental_bearer_token`。
- 切回 Account 前在 ChatGPT 仍关闭的窗口内处理手机连续性队列，最多 4 批、每批 8 个且 8 MiB，并受 30 秒总预算约束。若仍有排队会话则停止切换并保留 Relay 请求端，避免用户切回后看到会话暂时消失；旧历史与旧会话追加仍由单会话或“完全同步”手动处理。
- 手机连续性发布使用 immutable/no-clobber `openai` provider successor、SQLite transaction/`PRAGMA quick_check`、受管路径与首条 metadata 复核。兼容内容保持原样；本地文件/附件字段在 Remote successor 中替换为“部分内容仅本机”，原始 Relay JSONL bytes 不变并返回 `partial`。
- 分叉不再被静默视为成功：既有分支与新 no-clobber successor 均保留，并返回 `conflict` 状态。
- Relay 验证缩窄为 URL/TLS/HTTP/鉴权检查：仍请求 `<base>/models`，但不读取、推荐或判断模型列表。直接切换路径不会发起 Relay 网络验证，成功后显示“未验证”和“一键返回 Account”。
- 连续性发布期间关闭 Switch 会显示“继续等待 / 仍然退出”；确认退出会等待当前原子 mutation 结束、持久化队列并真正退出。
- 删除未使用的 `core:window:allow-destroy` 权限；安全退出调度若进程未退出会在 2 秒后释放 shutdown reservation，避免后续 mutation 永久 busy。

### Validation

- 发布前必须通过 Rust/前端单元测试、TypeScript、fmt、clippy、production build、Windows portable EXE 与 release contract；真实 Account ↔ Relay 切换还需确认历史会话和官方登录态均保持可见。

## v0.2.3 - 2026-07-28

### 关闭、切换耗时与任务体验

- 修复窗口关闭无反应：所有 close event 先由前端接管，再调用零参数 `request_app_exit`。后端复用全局 mutation lock；空闲时把锁保留并调度 `app.exit(0)`，若 2 秒后进程仍存活则释放 shutdown reservation，前端在 2.5 秒后重新请求，避免一次失效 exit 把后续 mutation 永久锁成 busy。写操作繁忙时返回 pending，任务到达可靠终态后自动退出。删除没有调用方的 `core:window:allow-destroy` 权限，不靠前端直接 destroy 强拆窗口。
- 按用户确认的 `1A / 2A / 3A` 重构为“官方登录态不变、只切请求端”：live `auth.json` 必须是 `auth_mode = chatgpt`，缺失/损坏/非官方模式在 mutation 前 fail closed；Account ↔ Relay 切换从不写入或恢复认证文件，写后还会做 byte-exact 复核。
- Relay Key 在槽位中继续使用 Windows DPAPI 加密；激活 Relay 时只投影到 live `config.toml` 的受管 `model_providers.openai_custom.experimental_bearer_token`，并拒绝受管合同之外的 Relay provider 名。切回 Account 时从同一受管 provider 常量删除整个表与明文 token，避免不可见密钥残留。TOML 解析错误、日志、回执和 UI 都不回显该值。
- Account overlay 对 `model` / `service_tier` 采用权威语义：保存值存在时恢复；保存值缺失时删除 live 中遗留的 Relay-only 字段，避免切回官方账号后继续沿用中转站模型或 tier。这是有意的路由清理行为，不是配置丢失。
- 请求路由 mutation 彻底移除会话全量扫描、双向同步、容量规划、checkpoint 和 provider slot GC；基线成功样本 `704.3s` 中三次扫描 `466.4s`、两次同步 `223.1s`，合计占 `97.9%`，现已从路由主事务删除。
- 请求路由持久终态之后新增独立有界增量收口：只有有效 `session-sync-state-v1.json` 才做 current/shared 轻量 metadata inventory，最多 32 个变化线程、32 MiB 预计写入，inventory 上限 750 ms、整体目标预算 2 秒；只同步变化 ID并使用双 `StateOnly`。索引缺失/损坏、删除/归档、双边同 ID 变化、分叉、超时或超限都标记“需要完全同步”，绝不自动回退全量，也不回滚已成功请求端；若增量终态无法持久化或 SQLite 补偿恢复失败，返回 typed `blocked` 启动态、保留检查点并禁止 UI 直接重试打开 ChatGPT。
- 原“立即同步”改为手动“完全同步”：由后端先关闭并复核 ChatGPT，移除前端 dry-run 的重复全扫描，只完整对账 current ↔ shared 活跃会话；归档行与 `archived_sessions/` 保持不动。成功后写入增量基线、记录真实阶段并受控重启 ChatGPT；失败可恢复两边 SQLite 状态，immutable JSONL 新增只可能完整保留供重试。
- 正式 Tauri release 实测 Account reapply 后端耗时 `10.770s`、UI `10.9s`，相较 `704.3s` 成功基线约快 `65x`；其中安全关闭 ChatGPT 为 `10.7s`，其余请求配置、认证复核和终态阶段均为 `0.0s`。标准 `WM_CLOSE` 实测应用在 `87ms` 内退出。
- 后端真实 phase/timestamp 继续完整保留，前端聚合成最多 7 个用户步骤：准备、可选 Relay 验证、关闭 ChatGPT、应用请求端、验证并记录、增量会话、启动 ChatGPT；逐步显示真实耗时和终态最慢步骤，不再展示 11 步长列表或“0 会话已同步”。
- 删除 `RuntimeSwitchResult` 中恒空的 `backups / toShared / fromShared / checkpointCleanup` 字段与无用 dry-run wire contract；生产前置关闭 helper 也移除恒为 false 的参数和旧“switching runtimes”文案。关闭前 preflight、关闭后权威 replan 与关键进程复检仍保留为 TOCTOU 安全边界，不为减少调用次数而合并。
- Relay `/models` 验证位于进程检测与关闭之前；无效地址、Key 或模型只消耗一次有界网络探测，不扫描会话池或触碰 ChatGPT。
- 整理仓库分支：把本地未合并的 session schema drift 修复合入 `main`，移除被后续实现取代的本地工作树/分支，并删除已合并或已废弃的对应远端分支；保留 `main` 作为唯一分支。

## v0.2.2 - 2026-07-26

### 切换体验与 ChatGPT 启动

- 运行态切换改为单一页面内 task overlay：切换开始后成为唯一任务焦点，持续展示真实后端 phase、当前说明、成功/写入前失败/已回滚/回滚失败终态和可执行操作；不使用 `alert`、`confirm`、`prompt`、原生 `<dialog>` 或额外系统确认框。
- 每次成功切换（包括运行态已 exact 的 no-op）都会在持久终态之后受控打开 ChatGPT：changed switch 使用关闭前从受管进程捕获并验证的 Windows AppUserModelID，并严格排在临时点清理与 provider GC 尝试之后；no-op best-effort 捕获当前运行实例，已运行时返回 `alreadyRunning`。两者都不从 `PATH` 查找同名 EXE、不显示控制台窗口；启动失败只返回 typed warning，不回滚已经成功的运行态，并在同一完成态提供“重新打开 ChatGPT”。
- 前端继续重构任务层级、运行态选择、状态说明和结果回执；图标统一使用 Lucide，不使用 emoji，并覆盖 reduced-motion、键盘焦点、screen-reader 名称及 1200×820、900×640、390×844 三档布局合同。

### Remote 会话可见性与磁盘边界

- Remote provider 归一现在严格发生在 `SelectMostComplete` 选出活动历史之后：较短 shared source 不会替换更完整 current，内容分叉也不会静默改指活动分支。SQLite `threads.model_provider` 与活动 JSONL 首条 `session_meta.payload.model_provider` 同步指向目标 provider。
- provider 不匹配或活动文件仍是 legacy `-imported` 形态时，任何既有 JSONL 都不覆盖；在受管 `sessions` 根内最多检查 32 个稳定候选，遇到已经完整覆盖所选历史的 Remote-compatible 槽位就复用，同时只记住首个空位；只有有界扫描结束仍无完整槽位时才在该空位发布新的 immutable successor。每个新槽位原子创建不超过 16 KiB 的 provenance marker，以 `createdBytes` 和创建前缀 SHA-256 证明工具所有权；文件创建后的合法 append 可继续识别，创建前缀改变则 fail closed。首次发布或合法增长只写最终 provider 文件与 marker，不额外生成 raw/provider 双份；真正 Divergent 为避免数据丢失可单独保留 raw 分支。
- session id 必须是严格 UUID 单文件名组件；选中来源和目标父目录都必须通过 canonical containment。路径分隔符、`..`、绝对/驱动器片段、越界目录或 ID 不匹配在发布前 fail closed。
- 运行态 planning 先在 ChatGPT 仍运行时计算 provider rollout、普通 rollout、最多 16 KiB marker 预留、SQLite workspace/index 输出并做第一轮只读容量 fail-fast；关闭完成后重建 closed-session 权威写集并再次按真实卷聚合窄 checkpoint 峰值与 session 输出，每卷保留 `max(2 GiB, 15%)` 余量。容量是保守上界，精确相等约束作用于权威写集：post-close replan/capacity 失败或第一份 checkpoint 前发现 plan 漂移时不创建 checkpoint/output；checkpoint 后发现漂移时不做 live session/runtime 写入，已有 typed prewrite checkpoint 按终态证据清理。
- hot shared→current 的 existing-thread `PreserveExisting` 快路径保持不变：在 target candidate 查询/复制前直接跳过，既有 SQLite provider/path/title 不变，`copied_session_files = 0`。
- provider predecessor 只在 durable terminal 成功落盘、current/shared 两库均不再引用、successor 再次证明完整包含旧槽位且 provenance/source 未漂移时才回收；任何证据不足都保留旧槽位并给 warning。typed result/UI 分开披露持久会话 added、reclaimed、net 与 transient checkpoint reclaimed，避免把临时释放误报成会话净变化。
- changed switch 在两根检查点和最后一次 ChatGPT 进程门禁之后校验固定的 `process_manager/chat_processes.json`。current `RuntimeState` 的 BackupManifest v4 会保存其 exact bytes；只有明确的空/全 NUL 损坏才原子修复为 `[]`，任何合法 JSON 都按原字节保留，缺失文件不创建；无效但非空/非全 NUL 的未知格式、超过 16 MiB、link/containment 或锁定异常均保留原文件并 fail closed。后续步骤失败会从检查点恢复原字节；真实 `repairingAppState` phase、`chatProcessStateRepaired` receipt、操作计数和完成面共同披露结果。

### C 盘增长审计与 PR 决策

- 已确认 v0.2.0 每次 changed switch 都永久创建 current `Runtime` 与 shared `Sessions` 两份全会话 checkpoint，是用户观察到 C 盘每切一次就减少的直接原因。本机最新四个 v0.2.0 switch 目录合计约 2.48 GiB；受控清理曾从 21 个目录、`6,327,089,609` bytes 降至 17 个目录、`2,693,977,957` bytes，但剩余项不是“已全部清空”。
- 当前 writer 使用 BackupManifest v4；changed switch 的 current `RuntimeState` 额外声明 `trackedProcessState=true`，shared 仍为 `StateOnly`，普通同步仍为双 `StateOnly`。自动 checkpoint 必须携带非空 `operationId` 与精确 `role`，且只有 bound v4 与唯一 terminal record 的 ID/role/action/status/phase、reason/scope、路径、时间窗及完整 payload hash 全部匹配时才自动释放。未绑定 v2/v3 自动点一律 fail closed 保留；经 managed-full 强校验的 v2/v3/v4 Full 仍可显式列出、恢复和删除，但不参与自动 cleanup。
- 普通切换不会创建 updater EXE；updater staging 只在用户主动执行更新时出现。正常切换的持久增长仅可能来自已通过容量预检的 ordinary/new-session rollout、真正 Divergent 的 raw 分支、Remote provider successor/marker 和窄 operation metadata/log。
- PR #4 是审查时唯一 open PR；其 Remote/provider 与全 NUL process registry 诊断可取，但原实现会在 checkpoint 前破坏性修复，legacy Remote 文件名 fast-path 还会把后续必需的 Create 冻结为 `Deny`，并存在 session id 路径逃逸、provider 完整性绕过、输出容量遗漏、候选复用缺口和重复全量副本。本版本不直接合并该 PR，而是在 latest `main` 上用上述受检查点保护的链路完整替代。

### 已知边界

- 会话同步不是持久备份；手工 Full、硬删除前和恢复覆盖前的安全备份仍有独立用途，不参与自动清理。
- provider rollout 首次生成仍可能增加一份完整 JSONL 和 provenance marker，但它有明确 Remote 可见性用途，并进入 pre-close 初筛与 post-close 权威重算的保守容量检查。活动/可复用槽位不会删除；只有工具可证明所有权、已有完整 successor 且两库均不再引用的 predecessor 才在 durable terminal 后回收，无法证明时继续占用磁盘。
- `archived_sessions/` 不属于普通 active-session provider 归一范围；持续双向同步仍有 source TOCTOU 与跨 SQLite/JSONL/index 非单一 durable transaction 的残余风险，继续依赖稳定性复检、no-clobber 发布、SQLite transaction 和 typed failure 收敛。
- exact no-op 不关闭 ChatGPT，也不检查或修复 process registry；`Full` 继续表示用户数据恢复点而不包含这个 transient registry。Windows 独占读取与同目录原子替换之间无法形成单一 OS 事务，因此修复前后都执行 exact revalidation，无法证明时终止并由现有补偿恢复。

## v0.2.1 - 2026-07-26

### 备份与数据安全

- 新增页面内“创建完整备份”入口：在 blocking worker 中串行执行容量预检、关闭态复检、current/shared 两根 full snapshot 和强校验，并返回操作 ID、备份路径与 typed receipt；全程不使用系统弹窗。
- changed switch 固定创建 current `RuntimeState` + shared `StateOnly`，普通同步固定创建双 `StateOnly`，不再为了临时回滚复制 GiB 级 `sessions/` payload。切换失败补偿 current/shared config/SQLite；热同步失败只补偿 shared SQLite。实际进入 Create/Import 的路径在失败或崩溃时最多留下未被 SQLite 引用的完整 imported JSONL，不会留下半截文件尾；hot shared→current 的 existing + `PreserveExisting` 在文件处理前直接跳过，不会产生这类残留；不宣称跨介质 bit-exact 回滚。
- 临时点只在终态日志成功落盘后释放：成功、切换完整回滚、typed `Failed + Backup` 写入前失败，以及恢复可见成功；Apply/回滚/日志失败、无法复核或清理失败时保留。删除前重新验证完整 manifest、路径、文件集合、大小与 payload SHA-256。
- 备份区新增占用审计与页面内安全释放任务流，显示目录总占用、可证明回收的字节/数量、安全保留数量、最近结果和警告；显式 cleanup 还支持严格 v3 写入前失败的一根/两根临时点和恢复可见单根 `StateOnly`，legacy v2 只兼容旧成功/完整回滚双根。cleanup 在 plan 与 execute 两阶段都重新验证完整 payload SHA-256，计划后发生任何漂移都保留目录并 fail closed；全程不按年龄、数量、mtime 或空间阈值猜测删除。
- cleanup Summary、Receipt 与操作记录新增 `attemptedCount` / `failedCount`。只有计划内目录在执行期 revalidate 或 remove 失败才计入失败、触发页面“部分完成”和 Failed 日志；Full、孤儿、unclassified 等安全保留 warning 仅作说明，`failedCount = 0` 时仍为成功。对应 Rust 与前端测试同时覆盖“有保留说明但成功”和“无 warning 但真实删除失败”。
- 真实受控 UI 清理已将 `%APPDATA%\codex-switch\backups` 从 `21` 个目录、`6,327,089,609` bytes 降到 `17` 个目录、`2,693,977,957` bytes，计划项 `4/4` 删除并回收 `3,633,111,652` bytes，紧邻操作的 C 盘空闲实测增加 `3,637,547,008` bytes。首次执行使用旧候选 UI，因把安全保留 warning 当作 partial 而留下历史 Failed 记录；该历史不篡改，语义 bug 已由上述字段与测试修复。发布闭环后的最终 cleanup 为 `0 attempted / 0 failed / Succeeded / Complete`，剩余 17 项继续安全保留，不自动删除。
- 已验证 Full 与兼容 legacy v2 恢复点新增页面内“删除恢复点”：显式二次确认、受管根直接子目录限制、删除前强校验、回收字节回执和 `deleteBackup` 操作审计。手工 full backup、硬删除和恢复 safety backup 仍不自动删除。
- Full/Sessions 备份和会话硬删除覆盖 `archived_sessions/`；runtime/state scope 保持原有窄边界。备份创建失败时如果 partial 目录清理也失败，错误不再吞掉该残留。
- 容量 fail-fast 从运行态切换扩展到会话热同步、会话硬删除、恢复可见、full restore safety 和手工 full backup。估算按每个 source 的真实 `CodexPaths + BackupScope` 累加 payload、DPAPI 与 manifest 开销，使用最大的 SQLite workspace，并保留至少 2 GiB 或 15% 的安全余量。
- 恢复点列表改为用户按需加载、按时间遍历 full 候选，最多返回 256 个已强校验快照供逐份恢复或删除。列表与删除共用 managed-full 强校验；未声明额外文件、payload 大小/hash 漂移、路径或 manifest 异常的候选不会显示为 verified，也不会自动删除；持久 Full 仍只接受用户显式管理。
- 关闭态 mutation 会检测独立 `codex.exe` CLI 并 fail closed，同时继续只关闭受管 ChatGPT 进程树；CLI 不会被强制结束。备份完成后、第一笔 live 写入前再次复检，缩小进程重新启动竞态。
- 会话 JSONL 发布补齐执行期 TOCTOU 防护并移除生产路径原地修改：source 对完整尾行、session ID、长度和 SHA-256 做前后稳定性校验，只允许有界重试；Create 使用 atomic hard-link no-clobber，允许替换时的严格扩展与 Divergent 都按稳定 source hash 发布完整 imported JSONL，旧目标 bytes/hash/mtime 不变。current→shared 与 ChatGPT 已关闭的同步使用 `SelectMostComplete`，完整文件发布后才可推进既有 SQLite `rollout_path`；只有 hot shared→current 使用 `PreserveExisting`，既有 current thread 在 `existing_thread_rollout_path` / `copy_rollout_file` 前直接 duplicate+continue，target candidate 不读取、不写入、不发布，`copied_session_files = 0`，原 `rollout_path`、provider、title 和旧 writer 可见性全部保留。hot 新 thread 仍事务插入，复用 source row 字段并归一 current provider；live current index 使用 `Skip`，关闭态或工具独占的 shared index 用完整 merged bytes 同目录 `atomic_write`，`Deny` 零写入，`Unchanged` 返回前复检。
- `inspect_checkpoint_storage` 改为 blocking worker + mutation guard；备份 Dashboard 先完成列表读取/legacy 迁移和操作记录，再扫描检查点空间，避免并发 guard 冲突和迁移竞态。

### 性能、Relay 与交互

- 运行态切换在扫描大会话池前先发送真实 `planningSessions` 阶段，再双向规划 JSONL 与 `session_index.jsonl` 写集；检查点始终保持 current `RuntimeState` + shared `StateOnly`。会话文件不变时冻结 `Deny`，需要合并时使用零 in-place、完整文件原子发布的 `Allow`，不再把检查点扩大为 Runtime/Sessions。
- 切换任务轨道新增真实 `cleaningCheckpoints` 阶段；用户能看到终态记录完成后的临时空间释放，不会在“校验完成”和最终回执之间再次出现无反馈等待。
- 普通会话同步改为 async Tauri command + `spawn_blocking`，容量扫描、备份、SQLite/JSONL 合并和终态清理不再占用异步 IPC 线程。备份域已有扫描进行中时，后续 mutation 的刷新请求会排队再跑一次，避免长期展示 mutation 前的占用和操作记录。
- hot shared→current 对既有 thread 现在零 target candidate I/O：不再重复读取、hash、写入或发布异名 imported 文件，也不会制造 orphan；重复热同步的该分支 `copied_session_files = 0`，直接减少同步耗时、ChatGPT 文件观察/索引压力和 C 盘会话目录增长。
- 关闭请求监听改为应用挂载时预注册；注册未完成或失败时切换 fail closed，切换 invoke 前同步激活门禁且不弹确认框。切换完成后进入独立 runtime refresh pending 状态，确认当前运行态前禁用再次切换；mutation 错误、刷新错误和 unknown 终态使用单一且保守的页面反馈。
- Full 删除、检查点释放、配置和所有确认继续使用页面内流程；生产 UI 统一 Lucide、禁止 emoji，新增 `public/favicon.ico`。三种目标尺寸的视觉 QA 未发现页面级 overflow、native dialog 或 emoji；正式 Release 资产随后通过真实自动更新并重启进入 ChatGPT Switch。
- Relay `/models` 验证现在拒绝空模型列表，并要求配置的 model ID 与 `data[].id` 精确匹配；错误不会回显 API Key 或响应正文，同一次失败只进入一个页面错误面，不再重复显示。
- Relay 当前态精确匹配纳入 provider `base_url`。修改已激活 Relay 的地址后会显示待重新应用，不再把旧地址误判为当前配置。
- 正式 Tauri 窗口最小宽度降到 390px，使 820px/520px 响应式布局在 EXE 中真实可达；会话表是可聚焦、可命名的横向滚动区域。
- 缺少 `auth.json` 或 `config.toml` 时，运行态页面会直接提示先打开 ChatGPT 完成登录并刷新；切换按钮旁持续说明会关闭 ChatGPT，不增加确认弹窗。
- updater 下载连接超时为 30 秒、总超时为 10 分钟；helper readiness 超时执行 kill + wait，staging 清理使用有界重试。正式 v0.2.0（SHA-256 `42012…A65A`）已在隔离 UIA 中真实点击“立即更新”：约 `2.086s` 出现并点击，旧进程约 `89.103s` 后 exit `0`，`105.1s` 内无手工替换地安装并重启 v0.2.1；目标为 `2,214,400` bytes、SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`、版本 `0.2.1`，staging/install leftovers 均为 `0`。
- Windows release profile 固定为 `opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"` 与 strip symbols。新增 `scripts/pack-windows-release.ps1`：验证固定官方 UPX 5.2.0/EXE SHA-256，只压缩 raw 副本，依次执行 raw contract、`--best --lzma`、`upx -t`、packed contract、PE32+ x64/双版本和 3,000,000 bytes 硬门禁；CI 固定官方 ZIP/EXE 两级 SHA-256，并只上传 packed 的裸 `codex-switch.exe`。

### 回归证据

- 新增隔离合成大 Home 回归：64 个等价 JSONL 在 provider 切换后保持文件数、bytes、SHA-256 与 mtime 不变，零复制、零 imported 副本，只更新对应 SQLite provider。可选 ignored benchmark 支持环境变量放大规模。
- 增加容量边界、archived sessions 覆盖、损坏候选回填、list/delete 同强校验、额外文件/hash 漂移排除、256 个显式 Full 管理上限、独立 CLI、Relay 空列表/模型匹配与单一错误面、手工 full backup、source 稳定性、完整 hash-named import、旧目标 bytes/hash/mtime 不变、atomic no-clobber create、hot `PreserveExisting` 的既有 thread 零 candidate I/O/零 orphan/`copied_session_files = 0`、closed `SelectMostComplete`、旧 writer 继续可见、既有 rollout/provider/title 保留、新 thread 插入、index hot `Skip`/完整原子替换/`Deny` 零写、`Unchanged` 终前复检、cleanup plan/execute 完整 SHA、`planningSessions` / `cleaningCheckpoints` 阶段、typed prewrite/恢复可见临时点释放、legacy v2 严格双根证明、备份刷新排队、关闭门禁、runtime refresh pending、updater 超时/清理和页面内任务流测试。
- 发布打包本地临时候选从 raw `5,955,584` bytes 得到 packed `2,228,224` bytes（SHA-256 `4DDC…CED7A`），同一 raw 重复打包 hash 一致；它只作为历史候选，不是正式 Release。PR #5 已从工作提交 `702dc37` 合并为 `3b4f440`；PR CI `30194264349` / `30194276794`、main CI `30194772843`、annotated tag `v0.2.1` 指向的 tag CI `30195207004` 均通过。[正式 v0.2.1 Release](https://github.com/mingisrookie/codex-switch/releases/tag/v0.2.1) 是 latest stable、非 draft，且只有一个 `codex-switch.exe`：`2,214,400` bytes、SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`、PE32+ x64、`FileVersion/ProductVersion = 0.2.1`、`upx -t` 通过；Release 回下载与 tag-CI artifact 的 hash/bytes 完全一致，两个 ignored live GitHub 合同测试各 `1 passed`。

### 兼容性与已知边界

- 继续只发布 Windows x64 `codex-switch.exe`；当前不按年龄、数量、mtime 或容量阈值自动 prune。持久 Full 可由用户逐个确认删除，但失败、孤儿和无证据目录仍会占用磁盘。
- 热同步仍不是跨 SQLite、JSONL 和 `session_index.jsonl` 的单一 durable transaction；实际进入 Create/Import 的路径会先发布完整 JSONL 且不会产生半尾。hot shared→current 对既有 thread 在文件处理前直接跳过，保持零 target candidate I/O、零 orphan 和 `copied_session_files = 0`；closed `SelectMostComplete` 才会在发布后推进既有引用。合成回归也不等于真实 ChatGPT 重索引 wall-clock 已量化归零。
- 本机 Microsoft Defender Product/Feature disabled，扫描命令未能形成通过证据；上述 release contract、hash、`upx -t` 与真实更新闭环不能替代或冒充 Defender 扫描通过。

## v0.2.0 - 2026-07-26

### 体验与品牌

- 公开 UI 与窗口标题统一为 **ChatGPT Switch**；仓库名、`.codex`、`CODEX_HOME`、`plus` 槽位和既有 wire contract 继续保留兼容命名。Release 资产仍唯一命名 `codex-switch.exe`，兼容 v0.1.9 updater 的固定资产校验。
- 前端重构为更清晰的运行态工作台，生产 UI 统一使用 Lucide 图标，不使用 emoji、`window.confirm`、`window.prompt` 或原生 `<dialog>`；配置、覆盖、同步和删除确认都在页面内完成。
- 窄屏主导航改为顶部两行 sticky header，不再用固定底栏遮挡会话表和运行态内容；会话表在窄容器内保持可横向访问，不产生页面级横向溢出。
- 点击切换后立即显示页面内任务执行器，通过 Tauri `Channel` 展示 relay 验证、ChatGPT 检测/关闭、两根备份、双向同步、运行态应用、校验和回滚等真实后端阶段，不显示伪造百分比。
- 切换失败明确区分“写入前失败”“已自动回滚”和“回滚失败”；切换成功后提示会话域将在进入会话页时刷新。

### 性能与可靠性

- 首屏只加载运行态、已保存槽位和操作记录；会话扫描在进入会话页时按需执行，备份哈希校验在用户主动加载时执行。切换后只刷新运行态域，并把会话/备份域标记为 stale，避免切换完成后再次同步阻塞 UI。
- `switch_runtime` 的 relay 网络验证、scoped 备份、SQLite/JSONL 同步和后置校验整体进入 Tauri blocking worker，避免大型会话池占用异步事件线程。
- Windows 进程控制改用 ToolHelp 快照建立 PID/PPID 进程树，只管理已识别的 `ChatGPT.exe` / `OpenAI.Codex.exe` 根及其后代，不误杀独立 `codex.exe` CLI；先温和关闭并等待，约 8 秒后才对仍可证明身份的进程使用强制兜底。
- 切换在关闭 ChatGPT 前完成 current/shared/backup/外置 SQLite roots 冲突检查和磁盘容量预检；估算按本次 scope 的实际写集覆盖 payload、SQLite workspace、加密/manifest 开销，以及至少 2 GiB 或 15% 的安全余量。
- 会话同步不再只为 provider 变化重写内容等价的既有 JSONL；未变化文件保持原 bytes 和 mtime，provider 可只更新到 SQLite。仅新建、复制或增长替换的 JSONL 才归一运行态元数据。

### 安全与数据完整性

- 备份 manifest 升级为 v3，并显式记录 `scope` / `trackedDatabases`。runtime / sessions / state-only 快照只覆盖实际写集；hard delete 和 restore safety 通过 SQLite Online Backup API 覆盖四个受管数据库。局部快照不进入 UI 可恢复列表，既有 v2 快照继续可恢复。手工 full backup 的公开入口在 v0.2.1 补齐。
- Relay 只有在 URL origin 不变时才允许留空 API Key 并保留旧密文；改变 scheme、host 或 port 必须输入对应来源的新 Key。
- updater staging 名改用 Windows CSPRNG；目录按当前 token 是否 elevated 设置受限 DACL，并在准备与 helper 执行期间持有目录句柄。
- EXE replacement 新增持久化阶段 journal、旧/新文件 hash 复核和中断恢复；Tauri Ready ACK 前的早退、超时或不一致状态会走受控回滚。
- elevated 无参数启动不再扫描 `%TEMP%` 自动续跑中断更新；正常 helper 的显式完成/回滚路径保持不变。
- 构建链将 `postcss` 升级到 `8.5.23`、`nanoid` 升级到 `3.3.16`，消除旧版 source map 自动加载的路径遍历公告；生产和完整依赖审计均为零漏洞。

### 兼容性与已知边界

- 当前仍只发布 Windows x64 `codex-switch.exe`，技术数据路径仍使用 Codex Home 命名；品牌更名不迁移或重命名用户现有目录，也不能把 Release 资产改名为 `chatgpt-switch.exe`。
- 当前没有自动 backup retention/prune；历史加密备份仍会累积。容量预检只阻止新的不安全切换，不会静默删除旧备份。
- GitHub Release digest 与元数据仍来自同一控制面，当前 EXE 也没有 Authenticode 独立信任根；发布说明不得把这两项描述为已解决。
- 热同步仍不是横跨 SQLite、JSONL 与 `session_index.jsonl` 的单一 durable transaction；失败时继续只补偿 shared-sessions，不用旧快照覆盖可能正在变化的 live current Home。

### 发布验证

- 发布 commit 必须执行 `npm test -- --run`、`npm run typecheck`、`npm run build`、`cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`、`cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings` 和 `cargo test --locked --manifest-path src-tauri/Cargo.toml`。
- 完整 release 还必须执行 `npm run tauri -- build` 与 `npm run check:release`，并留存版本/PE/hash、临时 `CODEX_HOME`、updater 成功/回滚、乱码和敏感形态检查证据；最终测试计数以发布 commit 的实际输出为准。

## v0.1.9 - 2026-07-19

### Fixed

- Windows 原子替换在深层加密备份目录中兼容超过传统 `MAX_PATH` 的临时文件；调用 `MoveFileExW` 前规范化已存在的父目录，同时保留目录联接迁移后的路径兼容性。
- 新增长路径备份形态的回归测试，覆盖临时文件超过 260 字符时的原子写入。
- 会话同步在同一会话 ID 存在多份 JSONL 时优先采用 SQLite 当前 `rollout_path`，只允许沿严格前缀关系选择更完整的独立版本；较短或内容分叉的来源不再把目标数据库改指向不完整副本。
- 会话文件比较忽略 `session_meta.payload.model_provider` 的运行态差异，避免单纯切换 provider 生成伪冲突副本，并阻止 `-imported-*` 后缀反复嵌套。
- 热同步只改写本轮新复制且实际选中的文件；未选中的候选不再改写 live JSONL 或活动 thread provider。
- 目标 SQLite 中的既有 `rollout_path` 必须通过受管 `sessions` 根、路径穿越、canonical containment 和 session ID 校验；越界、失配或陈旧路径按缺失路径修复。
- SQLite 当前候选按完整 `sessions/...` 相对路径匹配；多个互相分叉的候选保持原活动文件，不再按文件名顺序静默提升分支。

## v0.1.8 - 2026-07-18

### 可靠性与安全加固

- 自更新与运行态/会话/Skill mutation 共用后端互斥；进入退出阶段后继续持有跨进程锁，前端也双向禁用重叠操作。
- 新 EXE 不再以“存活固定时间”判定成功：只有进入 Tauri `RunEvent::Ready` 并写入绑定受控 plan、状态和 SHA-256 的 ACK 后，helper 才删除旧 EXE；早退或 15 秒超时会恢复旧版本。
- 固定运行态槽位的 metadata 缺失、损坏或 ID 不匹配改为 fail-closed；`auth.enc`、`config.toml`、`runtime.json` 任一步写失败会恢复并验证全部旧文件。
- Relay 覆盖前归档完整旧槽位；同毫秒历史目录避免碰撞。远程 Relay 强制 HTTPS，HTTP 只允许 loopback；`/models` 成功响应限制为 4 MiB。
- Skill 崩溃 journal 增加阶段状态，修复备份尚未创建时恢复流程误删用户原 Skill 的窗口。
- `taskkill` 后重新枚举 Codex 进程，只有确认全部退出才报告成功；关闭态 mutation 在备份后、写入前再次检查进程状态。

### 架构与发布工程

- 删除当前产品链路没有调用的 legacy `profile_store`、`switcher`、`redaction` 模块，减少 556 行旧攻击面和维护分支。
- Windows CI 固定 Actions 与 Rust toolchain，使用 lockfile 执行 Rust 门禁，并真实构建 Tauri release EXE。
- 新增 release contract：校验五处版本、tag、PE 格式/大小、ProductVersion、构建机路径和常见 GitHub token marker；CI 留存对应 commit 的 verified artifact。
- README、跨平台审计、DXM 长期文档和 Trellis 可执行规范与真实实现同步；当前仍只发布 Windows x64 单文件 EXE。

### 兼容性与已知边界

- `v0.1.7` 用户可直接通过应用内“一键更新”升级到本版本；受管运行态、备份和 Skill 配置路径保持兼容。
- 原先配置为非 loopback 明文 HTTP 的 Relay 将被拒绝，必须改用 HTTPS；这是防止 Bearer Key 明文传输的有意收紧。
- GitHub Release digest 仍不是独立签名信任根；热同步也尚非跨 SQLite/JSONL/index 的单一事务。发布说明不把这些残余描述为已解决。

### 验证

- `npm test -- --run`（53 项）、`npm run typecheck`、`npm run build`。
- `cargo fmt -- --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked`（107 项单元测试 + 7 项 Skill 合同测试；2 项 live GitHub 测试默认 ignored）。
- updater replacement 成功、启动失败回滚、ACK 篡改、早退、超时和 mutation 双向互斥对抗测试通过。
- 完整 Tauri release build、版本/PE/路径/敏感 marker 合同和临时 `CODEX_HOME` / `APPDATA` 真实 EXE 启动冒烟通过。

## v0.1.7 - 2026-07-14

### 新增

- 更新提示改为“一键更新”：自动下载仓库最新稳定 Release 的固定 `codex-switch.exe` 资产，校验大小与 GitHub SHA-256 digest 后自动重启安装。
- 当前 EXE 会复制自身作为临时 updater helper；helper 完成旧进程等待、同目录 replacement、旧 EXE 备份、原子切换和新版本启动确认。
- 新版本启动失败时自动恢复并重启旧 EXE；成功或回滚状态会在重启后的界面中明确显示。

### 安全与兼容性

- 下载地址由固定仓库和已验证稳定版 tag 推导，只允许 HTTPS GitHub 与受控 GitHub Release 资产重定向。
- 拒绝缺失/重复资产、非法大小、缺失或非法 digest、URL 漂移、超限下载、SHA-256 不匹配和并发安装。
- debug 构建不执行真实自更新；Windows 文件替换封装在独立平台模块，当前仍只发布 Windows x64 便携 EXE。
- Tauri 构建入口会重映射工作区/用户目录并剥离 release 符号，避免发布 EXE 携带构建机绝对路径。
- v0.1.6 不包含 updater，升级到 v0.1.7 需要最后一次手动替换；v0.1.7 之后支持应用内一键更新。

### 验证

- `npm test -- --run`（51 项）以及 `npm run typecheck`、`npm run build`。
- `cargo fmt -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（93 项单元测试 + 6 项 Skill 合同测试）。
- 显式运行 live GitHub Release EXE 下载与 SHA-256 校验测试。
- 在隔离临时目录执行真实 helper 演练：成功覆盖/重启/清理通过；无效新 EXE 触发恢复旧 EXE、重启和清理通过。
- 完整 Tauri release 构建和临时 `CODEX_HOME` / `APPDATA` EXE 启动冒烟通过，窗口标题和产品版本为 `Codex Switch` / `0.1.7`。

## v0.1.6 - 2026-07-14

### Added

- 每次启动后台检查本仓库最新正式 GitHub Release，按 SemVer 判断是否存在新版；提供手动“检查更新”和非阻塞新版横幅。
- 新版横幅显示版本与限长更新说明，可关闭本次提示，并通过固定后端命令打开本仓库 Releases 下载页。
- 新增更新元数据异常、超大响应、恶意外部 URL、draft/prerelease、非法版本、并发检查、离线和前端 XSS 文本渲染测试。
- 新增跨平台准备度审计，明确 macOS/Linux 的路径、密钥库、进程、锁、Skill runtime 和 CI 迁移边界。

### Security

- GitHub 请求设置 8 秒超时、禁止重定向并限制 release metadata 为 256 KiB；错误不回显响应正文。
- 下载入口不采用 GitHub 响应中的任意 URL，只允许后端打开固定仓库 Releases 页面。
- 非 Windows 凭据保护不再使用可逆开发占位；缺少平台密钥库时明确拒绝，避免未来误构建形成假安全。

### Changed

- `windows-sys` 收敛为 Windows target 依赖，进程控制与 release GUI subsystem 显式标注 Windows 边界；当前发布目标仍只有 Windows x64 便携 EXE。

### Verified

- `npm test -- --run`（49 项）
- `npm run typecheck`
- `npm run build`
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`（83 项单元测试 + 6 项 Skill 合同测试；1 项 live 网络测试默认 ignored）
- `cargo test live_github_release_contract_is_compatible --manifest-path src-tauri/Cargo.toml -- --ignored --nocapture`
- `npm run tauri -- build`
- 临时 `CODEX_HOME` / `APPDATA` 启动 release EXE，确认进程存活、窗口标题为 `Codex Switch`、产品版本为 `0.1.6`，随后清理临时目录。

## v0.1.5 - 2026-07-14

### Changed

- 当前产品合同收敛为固定的一个 Codex 账号槽位和一个 API 中转站槽位；运行态分别展示已保存、精确激活/模式匹配和最近验证状态。
- 运行态配置改为基于 live `config.toml` 应用 overlay，只修改模型/service tier/provider 绑定，保护 `model_instructions_file`、MCP、项目等全局配置；只有精确匹配才视为无需切换。
- Dashboard 改为七个数据域独立加载/报错（含操作历史）；每个动作只依赖自身必要域，Codex Home 损坏时仍可验证已保存 relay 和恢复已验证备份，session 扫描失败时不误禁用仅依赖 managed inventory 的删除/恢复可见。
- 操作完成后的 Dashboard 刷新改为后台 best-effort；命令结束即释放 busy，刷新失败不会反转已完成操作或持续锁住按钮。
- 后端应用状态从已退役的 `MVP scaffold` 标记更新为 `hardened-mvp`，避免诊断接口继续误报脚手架阶段。
- 会话同步增加双向 dry-run 和 typed receipt；热同步不重写已存在的 live JSONL，关闭 Codex 的切换流程才允许流式原子修正 provider 元数据。
- 会话管理增加搜索、排序、每页 50 条、跨页选择和部分选中状态；所有硬删除统一要求确认，成功后清理已处理选择。
- 顶部新增独立“技能”页；技能状态只在首次进入时懒加载，不并入现有 Dashboard 七域。

### Security

- 所有工具备份载荷统一使用 Windows DPAPI 加密并记录 SHA-256/大小；SQLite 改用 Online Backup API，不再直接复制 WAL/SHM。
- 切换和删除具备 current/shared 双根快照、后置校验和失败补偿；热同步失败只恢复 shared-sessions，并保留 live current Home 及其安全备份，避免覆盖并发变化；完整恢复限制为受管来源且先创建目标安全快照。
- mutation guard 增加 Windows 独占 lock-file 句柄，在进程内 try-lock 之外阻止第二个 Codex Switch 进程并发写同一受管状态。
- 文件写入/复制/JSONL 重写统一走同目录临时文件 + sync + 原子替换；Windows 使用 write-through replace。
- API 中转站增加 Base URL 严格校验、原生可访问 `<dialog>`、password 输入、空 Key 保留已存凭据和 `/models` 连接验证；保存失败保留本次 Key 便于重试，成功/取消后销毁；10 秒超时、禁止重定向且错误不回显 Key/响应正文。
- 新增结构化脱敏操作记录 `%APPDATA%\codex-switch\logs\operations.jsonl`，记录操作 ID、动作、阶段、终态、备份引用和计数；Dashboard 可查看最近操作及关联备份路径。
- Tauri 生产 CSP 收敛到 self/Tauri IPC；开发态仅为本机 Vite HMR 与开发样式开放额外权限。
- Image2 / Grok 的用户 Key 通过受控 password 表单进入 Rust 后端，只以 Windows DPAPI 密文保存；空 Key 更新保留旧密文，明文不进入 Skill、配置、UI 状态、操作记录或回执。
- Skill 安装限定两个固定 ID 和编译期 allowlist；要求绝对 `CODEX_HOME`、Codex 关闭和全局 mutation guard，拒绝 link/junction/reparse path、未确认的未知目录与本地漂移，覆盖前保留完整目录备份，并用原子 transaction journal 在进程中断后恢复目录 swap。

### Added

- 新增最近已验证备份列表和按 `sourceRoot` 恢复入口；初版列表使用有限候选窗口做 payload 大小/SHA-256 强校验，恢复时再次强校验，并对 SQLite 执行 `PRAGMA quick_check`。该上限与校验合同已在 v0.2.1 改为按需最多 256 项、list/delete 同一 managed-full 强校验。
- 新增统一操作回执面板，展示操作 ID、备份数量、计数、回滚终态和警告。
- 新增 Windows GitHub Actions 质量门禁：前端测试/类型检查/构建，以及 Rust fmt、clippy `-D warnings` 和测试。
- 内置 `newapi-image2-client`：来源锁定到用户提供 ZIP 的 SHA-256 基线，默认使用 `https://api.lcming951.com/v1`、`gpt-image-2` 和 Images API，并增加 DPAPI 配置读取的 PowerShell generate/edit helper。
- 内置可分发 `grok-search`：移除本机路径、endpoint 和私有配置，由最终用户填写 URL/Key，默认模型为 `grok-4.5`，支持 Web/X 搜索。
- 新增 Skill 安装/更新/配置 typed receipt、受管 manifest/hash 漂移检测和 Windows PowerShell DPAPI 跨运行时契约测试。

### Verified

- `npm test -- --run`
- `npm run typecheck`
- `npm run build`
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`
- `npm run tauri -- build`

### Known limitations

- 当前没有自动 backup retention/prune；受管加密备份会持续累积，后续需要独立设计保留周期、容量上限和安全清理入口。

## v0.1.4 - 2026-06-30

### Changed

- 会话管理表格的批量操作下拉移到工具条右侧，选择入口更明显。
- 会话列表改为紧凑行高：只展示单行标题并省略超长文本，不再把长会话 ID 作为第二行显示。

### Verified

- `npm test -- --run`
- `npm run typecheck`
- `npm run build`
- `cargo test --manifest-path src-tauri/Cargo.toml`
- `npm run tauri -- build`

## v0.1.3 - 2026-06-30

### Added

- 新增顶部“会话管理”页，与“运行态”页分离，保持现有浅色 Codex Switch UI 风格。
- 会话管理默认合并展示当前 Codex Home 与 `%APPDATA%\codex-switch\shared-sessions`，来源标记为“本机 / 共享池 / 两边都有”。
- 会话表格左上角新增全选框、批量选择下拉和全选 / 反选按钮，修正表格列宽导致的更新时间截断问题。
- 支持删除所选会话：删除前备份当前 Codex Home 与 shared-sessions，随后硬删除两边的 SQLite thread、相关边表、JSONL 正文和 `session_index.jsonl`。
- 支持恢复可见：只更新当前 Codex Home 的归档字段，不立即强制同步。

### Changed

- 已归档会话默认跳过同步，不自动删除、不清理 shared-sessions，也不会从共享池复活回当前 Codex Home。
- 同一会话 ID 同时存在于当前 Codex Home 和 shared-sessions 时，当前 Codex Home 的标题、更新时间、provider 和归档状态优先。
- 删除未归档会话必须二次确认；删除已归档会话走备份后的安全硬删除，不额外弹二次确认。

### Verified

- `npm test -- --run`
- `npm run typecheck`
- `npm run build`
- `cargo test --manifest-path src-tauri/Cargo.toml`
- `npm run tauri -- build`
- 临时 adversarial Rust 集成测试：4 passed

## v0.1.2 - 2026-06-23

### Fixed

- 会话同步改为 JSONL-first：只同步存在 `sessions/**/*.jsonl` 正文的会话，跳过只有 SQLite 行但缺少正文的孤儿记录，避免同步后出现不可打开的空会话。
- 自动识别 Codex `config.toml` 中的 `sqlite_home` 和环境变量 `CODEX_SQLITE_HOME`，不再假设 `state_5.sqlite` 一定在默认 Codex home 下。
- 共享会话池与当前 Codex home 同步时同时合并 `session_index.jsonl`。
- 运行态切换和热同步继续只归一化 `threads.model_provider` 与 JSONL `session_meta.payload.model_provider`，不改用户/助手正文。

### Verified

- `cargo test --manifest-path src-tauri/Cargo.toml`
- `npm run typecheck`
- `npm test -- --run`
- `npm run build`
- `npm run tauri -- build`

## v0.1.1 - 2026-06-23

### Changed

- 首页 UI 调整为 Codex 账号态 / API 中转站态 / 会话热同步的运行态工作台。
- Release 版 Windows 子系统改为 GUI，启动时不再弹出终端窗口。
- README 更新为面向 GitHub 发布的公开说明。

## v0.1.0 - 2026-06-23

### Added

- 初始 MVP：Codex 账号态保存、单个 API 中转站配置、运行态切换、备份、会话扫描与基础同步。
