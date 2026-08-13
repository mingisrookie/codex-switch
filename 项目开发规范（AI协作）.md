# 项目开发规范（AI协作）

> 项目：`codex-switch`
> 根目录：`<repo-root>`
> 初始化日期：`2026-06-23`

本文档是面向 AI 与开发者的大项目开发规范。目标是让项目更清晰、更可维护、更可测试，而不是让 AI 只追求“眼前能跑”。

<!-- DXM-DOC-RULES:START -->

<!-- DXM-CONTRACT:2 -->

## DXM 文档维护规则

- 本块由 DXM 管理；`--refresh-blocks` 只刷新本块，保留下方项目专属规范和人工补充。
- 完整流程细则集中维护在本文；`AGENTS.md` 只保留触发约定、红线摘要和本文件指针，避免多处长期事实逐字漂移。
- 修改开发流程、测试要求、架构边界、协作约束后，必须同步更新本文并检查中文乱码。
- 不得把临时方案文件、聊天结论或一次性清单当成长期规范替代品。

<!-- DXM-DOC-RULES:END -->

## 0. AI 协作执行协议

### 0.1 开发前必须实际阅读

任何涉及代码、流程、UI、配置、测试、文档、提交、合并、发布的任务，开始前必须实际阅读或重新核对：

1. `AGENTS.md`
2. `项目文件结构说明.md`
3. `项目完整链路说明.md`
4. 当前文件

不能只凭历史记忆、上次会话摘要或“看起来知道项目”直接开发。

### 0.2 意图边界

- 用户指出“先分析”“只分析”“暂时不改”时，只能输出分析，不得擅自改代码或提交。
- 用户要求“开始开发”“按照清单开发”“提交代码”时，必须按阶段推进到完成，不能停在方案层。
- 如果目标、边界或成功标准不清楚，先澄清；如果本地可以查清，优先查证。

### 0.3 首次建档与 project-grill

首次 `/dxm` 是项目建档入口，不是单纯模板生成命令。默认先做 `project-grill`，再生成或更新长期文档。

`new-project-grill` 和 `lightweight-grill` 是 DXM 模式标签，不要求存在同名 skill；实际执行时可用 `grill-with-docs`、`grill-me` 或简短内联问答完成。

| 场景 | 默认处理 |
| --- | --- |
| 空目录 / 新项目 | `new-project-grill`：问清用户、交付形态、技术栈、核心范围、非目标、外部依赖、验收标准和维护周期 |
| 已有代码 / 文档 | `grill-with-docs`：先读文件与文档，再澄清需求、架构边界、风险和验收 |
| 临时脚本 / demo | `lightweight-grill`：只问阻塞执行的关键问题 |
| 已有完整 DXM | 不重复 grill，除非用户要求重梳理 |
| `scaffold only` / `先别问` | 只补模板，不 grill |
| `只分析` / `先看看` | 只读，不初始化、不改文件 |

### 0.4 Trellis 使用边界

Trellis 是中大型任务记忆层，不替代 DXM。

- 小修、只读排查、单点 bug、轻量文档调整：默认 DXM inline，不建 Trellis task。
- 新功能、多模块、架构变化、跨文件重构、长周期任务、需求不清楚：先 project-grill，再建议或创建 Trellis task。
- Trellis PRD 必须写入 `.trellis/tasks/<task>/prd.md`；不能只依赖聊天上下文。
- Trellis 不能自动 stage、commit、push 或创建/合并 PR；Git 操作必须得到用户明确授权。

### 0.5 开发方案与开发清单

用户要求写开发方案时，方案至少包含：

1. 需求理解与真实目标。
2. 现有源码链路分析。
3. 是否符合要求、是否完善、是否完整、是否正确。
4. 是否符合本开发规范、现有架构和命名。
5. 方案自身是否有缺陷、边界遗漏或上下游设计冲突。
6. 与本功能无直接 UI 关系但有逻辑关联的模块检查。
7. 分阶段开发清单。
8. 每个阶段完成后的自检项。
9. 最终全量审查项。

### 0.6 阶段化开发硬要求

进入开发后，必须按开发清单一个阶段一个阶段完成。每完成一个阶段，必须自检：

- 相关代码是否都改到。
- 状态流、日志流、数据流、UI/API 流是否一致。
- 是否有设计缺漏、逻辑缺陷和边界问题。
- 是否引入乱码、错码、异常替换字符。
- 定向测试或必要的静态检查是否通过。

阶段自检未通过时，不能进入下一阶段。最终提交或回执前必须再做一次全局审查。

## 1. 架构原则

### 1.1 真实运行态优先

当源码、文档、注释、旧方案与运行态冲突时，优先级为：

1. 实际运行行为、测试结果、日志和命令输出。
2. 当前入口、配置、启动路径和依赖关系。
3. 根目录长期文档。
4. 历史方案、注释、旧截图和生成物。

如果发现长期文档落后于真实实现，本次任务影响相关事实时必须同步修正文档。

### 1.2 模块边界

- 新功能优先沿现有分层接入，不能把逻辑重新堆回主入口、大文件或无关模块。
- 横切能力（日志、观察、导出、审计、缓存、重试、配置）应放在清晰的公共层或独立模块中，不能借某个业务开关隐式控制。
- 兼容型薄包装允许存在，但必须有明确目的；没有意义的旧分支应在后续重构中清理。
- 不为“看起来整洁”做无关重写、全文件格式化或大范围重命名。

### 1.3 完整链路原则

新增或调整功能时，必须同步检查：

1. 默认值、归一化、保存、恢复、导入导出。
2. UI / CLI / API / 配置入口。
3. 核心执行链路、状态流、日志流、错误传播。
4. 手动操作、自动流程、失败恢复、清理路径。
5. 测试和长期文档。

不能只改一个调用点就声称完成。

### 1.4 本地高风险写操作合同

涉及 live Codex Home、shared-sessions、运行态凭据或工具备份的 mutation，默认按以下顺序设计和审查：

```text
preflight -> plan -> capacity -> backup -> apply -> verify -> persist terminal -> cleanup -> typed receipt
                                                   \-> rollback -> verify rollback -> persist terminal
```

- 请求路由切换是上面快照合同的显式窄例外：`validate official auth + local Relay config -> close -> replan config/session view -> atomic config apply -> verify auth bytes + route -> persist provenance/terminal`。路由 mutation 不得执行 Relay 链接状态或 `/models` 网络探测，不得扫描/复制会话正文、创建 checkpoint、执行 provider slot GC 或写 `auth.json`；配置写后的失败只允许回滚原始 `config.toml`。成功后只在 terminal 外排队 coalesced Shadow，不得阻塞用户切换。
- 请求端切换、一次性迁移、冲突切换、离线 GC、待恢复导入、显式降级、恢复可见、手工 Full 和完整恢复必须在最后 live 写入前确认全部 ChatGPT/Codex writer 已关闭；独立 `codex.exe` CLI 是 fail-closed 阻断信号，不得被工具结束。v0.3 不暴露会话硬删除命令或 UI。
- 所有会创建安全快照的 mutation 必须在第一份新快照前完成受管 root 冲突检查与容量预检。手工 Full 与 restore safety 使用既有加密 scope；v0.3 迁移使用用户选择位置的完整未加密备份，并必须实际恢复验证。旧 v0.2.x hard-delete 快照合同只允许存在于 test fixture，不得据此注册生产命令或恢复 UI 入口。路径/卷解析、算术溢出或容量不足始终零目标写入，禁止边删边腾空间。
- Windows 关闭态操作必须基于 ToolHelp PID/PPID 关系证明受管进程树，只把 `ChatGPT.exe` / `OpenAI.Codex.exe` 作为受管根，不得按模糊进程名误杀独立 `codex.exe` CLI。需要自动重启时，关闭前对所有受管根尝试捕获且统一验证唯一 AppUserModelID；捕获缺失/冲突不得猜测其他目标，也不得阻断运行态切换，而是在成功终态返回 typed launch warning。切换完成后只允许通过 Windows 原生应用激活接口启动并重新验证同一身份，不得执行 PATH 中同名程序或前端传入的路径/命令。
- 请求路由 mutation 禁止创建任何 checkpoint 或永久正文适配文件。旧 v0.2 增量/完全同步只允许作为升级迁移、显式降级和契约测试兼容实现存在，不能从 v0.3 日常 command 到达；旧 hard-delete 实现同样只能由测试 fixture 触达。手工 Full 和 restore safety 继续使用既有 scope，恢复可见使用单根 `StateOnly`。
- 旧“完全同步”wire 必须保持删除；日常“会话合并与修复”在 canonical 未就绪时 typed blocked，迁移后只处理 Missing/Equal/EqualExceptProvider/Prefix 与数据库视图修复。Divergent/Unknown、索引异常、缺失正文、工具链损坏和来源不明文件只进入冲突或脱敏排查，不 fallback 到 legacy 全量物化。
- canonical JSONL 生产路径必须零 in-place，不得出现半截尾。每次 Create/Import/主版本切换先在同目录 staging 完整写入、sync 和 no-clobber/原子发布，再切数据库引用；失败先回滚数据库，只删除本次创建且 hash 未变/全局零引用的文件，无法证明的残留保留并报告。
- 需要整文件替换的配置、备份、可写 index、`operations.jsonl` 和 `session-storage-v1` 状态统一走 `file_ops` 同目录完整临时文件 + sync + 原子替换；逻辑 append-only 文件必须锁内严格解析旧历史、拒绝损坏/重复 ID。会话 source 在执行期校验语义尾、session ID、长度、SHA-256、file identity 和前后稳定，只做有界重试。v0.2 provider successor/`PreserveExisting` 规则仅可被兼容迁移、显式降级或单会话适配调用；v0.3 日常 route 禁止使用。历史 marker 只证明候选归属，不能单独授权删除。
- 所有会修改运行态、凭据、current/shared 或备份目标的 command 必须共用 mutation guard：进程内 try-lock + Windows 独占 `mutation.lock` 文件句柄；新增入口不得绕过同进程/跨进程串行化。
- 一键更新安装也必须取得同一 mutation guard；helper readiness 成功后父进程进入 shutdown-pending，退出前不得释放跨进程锁或接受新 mutation。前端禁用只能改善体验，不能替代后端互斥。
- 临时点只有在 terminal operation record 原子持久化后才允许释放。自动 checkpoint 必须是带非空 `operationId + role` 的 BackupManifest v4，并与唯一 terminal record 的 ID、角色、action/status/phase、目录、reason/scope、时间窗及完整 payload hash 全量匹配；恢复可见只允许绑定 v4 单根 `StateOnly`。未绑定 v2/v3 自动点、Apply/回滚/日志失败必须保留。cleanup 在 plan 与 execute 两阶段重新强校验；Summary、Receipt 和日志显式携带 `attemptedCount` / `failedCount`，warning 本身不得作为 partial/Failed 判据。
- 显式旧检查点清理必须严格解析完整 `operations.jsonl`；普通历史展示可以容忍旧的最后一条截断，但 strict cleanup reader 和后续 terminal publication 均拒绝损坏日志。未绑定 v2/v3 自动点不能由后来的日志认领；经 managed-full 强校验的 v2 legacy Full、v3 Full、v4 Full 仍可显式管理。重复 ID/引用、路径逃逸、无效时间窗、Apply/回滚/日志失败、孤儿或无法分类目录都保留。
- v0.2.0 风格的 current `Runtime` + shared `Sessions` 全会话自动 checkpoint 禁止重新引入；请求路由和手机连续性都不得创建全池 snapshot 或永久 provider rollout。显式单会话兼容若原生 runtime 必须读取 provider header，只能生成生命周期明确的临时适配文件，操作结束即回收；无法安全回收时 fail closed 并报告。
- 手工 Full 和 restore safety 不参与自动清理。Full 列表必须由用户按需加载，最多返回 256 个通过 managed-full 强校验的恢复点；列表和显式删除必须共用同一验证器，覆盖 backup root 直接子目录、Full scope、manifest、精确文件集合/大小与完整 payload SHA-256。extra file、hash drift、路径或 manifest 异常的目录不得显示为 verified，也不得自动删除。删除还必须再次复检，回报 reclaimed bytes 并写独立 `deleteBackup` 审计；禁止按年龄、目录数量、mtime 或容量阈值猜测删除，也禁止普通列表扫描静默 prune。
- 既有 BackupManifest writer 必须继续生成 v4：继承 `scope` 与 `trackedDatabases`，自动点绑定 `operationId + role`；请求路由不调用 backup writer。v0.3 迁移备份使用独立 manifest 和 marker，未加密、继承当前 Windows 用户 ACL，外部磁盘必须提示包含完整会话正文。两类备份不能混淆或由同一 cleanup 规则删除。
- 纳入 scope 的 SQLite 必须使用 Online Backup API，不得直接复制 WAL/SHM。既有 Full/State payload 继续 DPAPI 加密并记录大小/SHA-256；v0.3 迁移备份按原始 bytes/hash 保存并实际恢复验证。容量估算必须包含全部 payload、SQLite workspace、manifest/staging 与至少 2 GiB 或 15% 安全余量；overflow、根重叠、查询失败或空间不足都在任何目标写入前 fail closed。
- `SessionFileWritePolicy::Deny/Allow` 和 provider-aware plan 仅属于 legacy 兼容迁移/降级/显式单会话适配，不得接入请求路由 apply。v0.3 migration plan 不能作为执行授权：关闭后必须按 semantic ID/order/tool relation/len/hash/file identity 重冻 source，并复核完整引用图和授权写集；late drift 或抢先目标必须重算或 fail closed。
- v0.3 `SessionReferenceGraph` 的在线结果只能用于 Shadow 分类和报告：每个 runtime/backup SQLite 必须分别使用 Online Backup + `quick_check`，runtime 引用与备份库存必须分开计数；跨库快照天然非原子，禁止把“当前扫描零引用”当成删除授权。在线 GC 在 v0.3.0 固定为 scan-only。成功切换、迁移、冲突处理、待恢复导入和会话合并只向 single-flight/coalesced 调度器排队新 Shadow；自动清理开启时，只有迁移已提交、scan identity 新鲜、零未完成操作且实时 writer inventory 全部关闭，调度器才可自动调用与手工入口相同的离线执行器。窗口未形成时必须零删除并保留最终校准请求；候选证明失败必须停止并报告。关闭自动清理后继续扫描/报告但不执行 provider 副本删除。任何正文删除、canonical 切换或数据库迁移在最终写入前都必须重新验证全局 runtime 引用、完整 semantic containment、完整文件 hash/marker、文件身份/句柄和未变化状态。
- Shadow hash cache 只能在文件身份、大小、创建/修改时间和首尾指纹都一致时复用解析结果，且不得持久化完整路径或正文；marker 的缺失/有效/非法状态也必须独立复核，非法或链接 marker 不能命中先前的“缺失”缓存。Shadow cache/report/marker/session-view state/Account config 的 persisted-state 读取必须绑定已打开 regular-file handle、硬字节上限、前后 metadata 和 file identity；reparse、路径替换、增长、截断或空文件均 fail closed，禁止先看 metadata 再无界 `fs::read`。cache hit、mtime、文件名或单独大小/hash 都不能单独证明可删除；缓存损坏必须退回完整解析并报告，未来删除路径必须重新读取完整文件。Shadow staging 回收只允许固定 managed root 的直属合法 scan 目录，必须跳过存活或无法证明已退出的 owner、未过期目录、非规范名称、目录/未知条目及任何 symlink/reparse；只逐个删除 allowlist regular snapshot 文件再删除空目录，禁止递归删除。
- 会话关系判断必须以同一 threadId 的有效有序记录、工具调用/结果关系和内容 hash 为准；只允许从比较视图移除 `session_meta.payload.model_provider`。完全相同、仅 provider 不同和严格完整前缀可列为高置信候选；消息乱序、共同前缀后的不同尾部、工具结果先于/缺少调用、非法或不完整 JSONL、marker 缺失/伪造/hash 不符必须保守标记 Divergent/Unknown，不能进入自动删除。
- 每次迁移/GC/冲突/恢复/降级操作必须在任何写入前创建 durable ledger，记录 operation kind、canonical root、创建文件、数据库副本、源大小/hash、预期 canonical、阶段和回滚步骤。阶段只能沿声明状态机前进；Applying/Validating 失败必须进入 RollingBack，启动恢复不得靠内存状态。提交前可取消，提交/验证期间 UI 阻止误关，整次失败后不后台无限重试。
- 迁移 backup 未经实际隔离恢复和真实 Codex runtime 列表/读取/恢复验证不得标 `runtimeVerified`，不得继续 PlanReady/Apply，也不得整理旧备份。恢复验证必须使用隔离 `CODEX_HOME`/SQLite home，禁止污染原备份或真实主库。
- 冲突默认 `defer`；“较新”只按最后有效消息时间推荐，时间不可靠时不提供推荐。显式切换主版本先发布新 canonical/数据库副本并验证，旧版本进入 7 天回收；到期仍需全局零引用复检。旧备份缺失会话只能进入待恢复区，由用户决定；不可读或无法验证的备份不得写入 canonical。
- 离线 GC 只有在有效 Switch marker、marker/大小/hash 完全吻合、确定 canonical peer、Equal/Prefix、连续两次稳定、全部 runtime 数据库零引用、无活动句柄/写入且不属于冲突/恢复/降级/未知来源时才能删除。任何一个证明失败都保留并报告；重复执行必须幂等，不能继续误删或重复计入回收字节。
- diagnostics 事件、已终结的 operation/迁移审计、恢复包及其 manifest、冲突回收统一采用 7 天隐私生命周期；未终结或仍被回滚/恢复载荷引用的账本必须先保留，删除前仍做引用和完整性复核。该 retention 是独立隐私合同：provider 副本自动清理开关只控制离线 GC，关闭后不得停掉 7 天 TTL，也不得把 retention 删除计入 provider GC 回收量。用户主动导出的诊断 ZIP 和显式降级包寿命跟随用户文件，不纳入此自动 TTL。
- v0.3 日常 request-route 只允许复用 OpenAI 主目录唯一 canonical `sessions/`；不得改写首条 provider、创建永久 provider successor、写 Shared 正文镜像或调用旧 full/incremental provider materializer。Relay/Account 只能切换数据库视图：`state_5.sqlite` 可在所有 writer 关闭后用 Online Backup 生成 provider-normalized sibling view；`logs_2.sqlite`、`goals_1.sqlite`、`memories_1.sqlite` 只能在同卷 hard-link 能力、相同 file identity、WAL checkpoint 和 quick_check 全部证明后共享，不能在普通切换中复制数百 MiB 全局库。hard link 不支持、目标独立、busy、reparse 或 root 漂移必须 fail closed。
- route view state 必须保存 Account configured/effective SQLite home、严格受管的 Relay sibling path 和 provider-normalized logical state digest。覆盖 inactive `state_5.sqlite` 前必须证明它仍等于上次共同基线；缺失、外部改写、崩溃残留或 digest drift 都不得用“active 看起来较新”猜测覆盖。普通切换不做数据修复，交给迁移/合并账本流程。
- 每次有效 route source 变化必须先于受控启动持久化 append-only provenance epoch，字段只允许 operation/time/runtime/provider/runtime generation/configured model，不得包含正文、完整路径、账号标识明文或 credential。原生 JSONL 的 `turn_context.timestamp/turn_id/model` 与最新不晚于该回合的 epoch 组合为每轮来源；首行 provider 只表示初始来源。账本重复 operation、时间倒序、截断、非法字段或持久化复核失败必须阻止启动；连续 no-op 不得重复增长相同 epoch。
- 备份创建失败必须尝试移除 partial 目录；如果清理同样失败，错误必须同时报告残留路径/原因并写入审计上下文，不得吞错或留下看似完整的无记录目录。
- 完整备份列表固定按需最多返回 256 个强校验通过的 full snapshot，不是只检查 256 个新候选。损坏、extra/hash drift 候选必须跳过且不得标 verified；局部 scope 始终排除，前端不得再次裁成 5 项。删除必须重新执行同一 managed-full 强校验；恢复也必须在 mutation 时重新校验所选 manifest/payload，不能复用列表时结果。
- 成功回执必须包含可关联的 operation ID 和 typed ChatGPT launch result。请求端切换还必须披露 route changed、官方 auth preserved、`chatProcessStateRepaired`、session-view status 与 `routeProvenance` typed terminal；为旧客户端兼容保留的 `relayValidation` 只能是 `skipped` / `notApplicable`，不得驱动网络请求或产品状态。不得保留恒空 backup/toShared/fromShared/checkpoint 字段或渲染“0 会话已同步”。高风险长流程必须通过后端 `Channel` 上报真实阶段和 typed 终态；后端原始 phase/timestamp 不得丢失，UI 可聚合为不超过 6 个用户步骤。ChatGPT launch 必须排在 route exact、provenance ready 与 durable terminal 之后；provenance failed/unknown 不得提供普通启动重试。

### 1.5 诊断日志与支持包合同

- 支持诊断是统一横切能力，必须复用同一 event schema、sanitizer、bounded store、operation correlation 和 exporter，不能在各业务 command 中临时拼接自由文本日志。已有 durable operation ID 必须复用；外层 command 尚未拿到 durable ID 时先生成随机 attempt ID，拿到后显式绑定。Windows ID 优先使用 OS CSPRNG，失败后用 `CoCreateGuid`，最终 fail-open fallback 也必须是进程局部且不可稳定关联。阶段来自 typed backend phase，不得从 UI 文案反解析；operation state 只在 terminal 事件确实持久化后封闭，封闭后必须拒绝再次 bind/phase/branch/terminal。所有用户主动 mutation 在发布前必须审计 start、真实 phase、关键安全分支、typed terminal/failed 覆盖；普通成功 query/render/后台刷新不持久化，后台失败和前端未处理异常才记录。
- `%APPDATA%\codex-switch\logs\operations.jsonl` 继续是检查点清理依赖的严格 durable 终态账本；诊断事件必须分库存放在 `%APPDATA%\codex-switch\logs\diagnostics\events-*.jsonl`。诊断层不得改变 operation-log schema、严格解析、原子整文件发布或 cleanup 证明。导出只读严格 operation 记录并生成去掉备份路径等字段的脱敏子集；诊断面板的清除、轮转和导出不得截断或删除账本，只有独立的 7 天隐私 retention 能在确认记录已终结且无恢复引用后回收旧审计记录。
- diagnostics store 不取得 mutation guard、不参与业务事务，但 Windows 同一登录会话、同一词法规范化诊断根的 append/read/status/prune/clear 必须先取得 root-scoped `Local\` named mutex，实现跨进程协调；command/lifecycle recorder 与 panic 都必须使用零等待 best-effort，锁忙立即放弃，只有低层管理 API 允许有界等待。不同登录会话或同目录的不同路径别名不保证共享锁。事件先结构化、限长、限制深度/字段/数组数量并集中脱敏后才 append + `sync_data`。默认保留窗口 7 天、总量上限 10 MiB、单 segment 512 KiB；年龄 prune 必须逐条解析，只能删除 clean 且全部事件过期的段，不能用 segment 创建时间替代 event Unix epoch 毫秒 `timestamp`；容量上限仍可淘汰结构有效、clean 的最旧段，dirty/corrupt/schema drift 不得借 prune 删除。
- store 读取时每个 segment 只允许忽略唯一的未完成尾记录，内部损坏必须明确失败；current segment 只有为空或以换行结尾时才可复用，dirty/partial tail 必须封存并创建新段。append/read 都必须拒绝非当前 schema；创建根前后逐级拒绝 symlink/Windows reparse/非目录 ancestor，segment 必须是受管 regular non-reparse file。open/write/sync 失败必须放弃 current 复用；事件已 `sync_data` 成功后，post-prune 仅 best-effort，其失败不得把 durable append 反报为失败。任何诊断初始化、锁、写入、轮转或清理失败都不得阻止应用启动、改变原 mutation 结果、触发回滚或伪造 terminal。
- 应用必须尽早创建每次启动随机、不可跨安装稳定关联的 session ID，记录含 PID、`timestampUnit` 以及系统允许时 best-effort 进程 creation stamp 的 `sessionStarted`；在 Tauri Ready/正常 Exit 记录 `appReady` / `sessionEnded`。下次启动只有在上一 session 无 clean terminal，且可用的 PID + creation stamp 不能证明原进程仍存活时才记录 `previousSessionUnclean`；liveness 证据可得时不得因另一个仍运行实例或 PID reuse 误报。panic hook 只做 root lock/state try-lock 的最小 best-effort 记录并继续调用既有 hook，release `panic = "abort"` 保持不变；前端 `error` / `unhandledrejection` 只允许固定分类和限长安全文案，不传原始 Error、stack、文件名、Promise reason 或业务 payload。
- 生命周期诊断不等于崩溃捕获保证。不得引入 watchdog、常驻服务、自动 WER 或 panic 恢复；访问冲突、强制结束、断电、硬卡死及其他来不及执行落盘代码的硬崩溃可以没有 panic/session terminal。`previousSessionUnclean` 只能证明上一诊断 session 缺少 clean terminal，不能单独归因为 panic 或具体业务步骤。
- 导出层必须在落盘 sanitizer 之外，对 diagnostics、operation 子集、health 与 metadata 再执行更严格的禁用字段删除、受管路径映射、未知绝对路径/身份字面值/秘密形态拒绝，再构建固定五文件 ZIP：`README.txt`、`manifest.json`、`diagnostics.jsonl`、`operations.jsonl`、`health.json`。manifest 记录 schema、应用/构建版本、时区化导出时间、平台/架构、选择窗口、脱敏策略、负载 bytes/SHA-256 以及 unavailable/warning；必须用 `timestampUnit = unixEpochMilliseconds` 明确 event `timestamp` 为 Unix epoch 毫秒，且不递归记录自身 hash。health 只允许只读结构化摘要：应用/平台、存储、route/auth mode、受管/standalone 进程计数及四个受管 SQLite 的 present/readable/bytes/`schema_version`；collector 缺失或失败必须逐项 unavailable，禁止伪造空列表、绿色健康或成功 quick-check。
- 默认诊断包不得包含凭据、API Key、token、Authorization、cookie、聊天/会话正文、原始 session JSONL、SQLite、`auth.json`、完整 `config.toml`、请求/响应正文、全量环境变量、进程命令行、Windows WER、机器名、用户名或稳定设备 ID。已知根只允许映射为 `%CODEX_HOME%` / `%APPDATA%` / `%USERPROFILE%` 等逻辑 token；未知绝对路径不得原样导出。诊断包仍是用户主动、私下发送的支持材料，应用不得自动上传。
- exporter 默认使用 Windows `SHGetKnownFolderPath(FOLDERID_Downloads)` 解析真实下载目录，不得拼接 `%USERPROFILE%\Downloads`。固定五文件必须先在受控内存完成脱敏、逐项 hash、ZIP 与 manifest 自检，再在目标目录写同目录 staging 并用 write-through、no-clobber 原子发布。只有 destination resolve/publish 失败才返回有界、短期 opaque retry ID；Downloads 重试和用户显式选择的固定 `%APPDATA%\codex-switch\diagnostic-exports` 必须复用同一 prepared bytes/hash/selection，不能重新采集，也不能把 preparation failure 误报为下载失败。成功后只通过后端登记的 opaque export ID 重新验证 containment 并打开所在位置。
- 顶部工具栏提供页面内可访问“诊断”面板，不新增主导航页签、原生 `<dialog>` 或额外窗口；必须覆盖 390px 可达、键盘/Escape、焦点进入/恢复、busy/error/success 单一状态面，busy 时外层 toolbar 也不得卸载面板或重置去重状态。所有 mutation rejection 通过固定、严格限长的信封携带真实 durable operation ID 或本次 attempt ID，API 统一解包并兼容裸字符串；业务失败入口只有拿到真实 ID 时才导出该 attempt 的完整事件及开始前 10 分钟上下文，不猜最新操作。
- “清除诊断日志”必须页面内确认，只删除受管 diagnostic event segments；不得删除 `operations.jsonl`、备份、会话、配置、凭据、用户已经导出的 ZIP（包括 `%APPDATA%\codex-switch\diagnostic-exports` 中的用户主动备用包）或导出硬中断遗留的同目录 staging。exporter 正常返回/错误 unwind 必须尽力删除 `.chatgpt-switch-diagnostics.<pid>.<sequence>.tmp`，但强杀/断电前来不及执行 Drop 时可以残留，测试与支持文档必须保留该边界。`health.json` 的 Windows 版本、route/auth、进程计数或单库只读 schema collector 失败时必须逐项 unavailable；即使采集成功，也不得把这些摘要宣传成完整配置/认证、进程命令行、SQLite `quick_check`、磁盘取证或“已覆盖所有首轮归因证据”。

## 2. 新增功能接入规范

### 2.1 新增配置项

必须同步检查：默认值、读取、归一化、保存、恢复、导入导出、UI/CLI/API 暴露、文档、测试。

### 2.2 新增流程节点或运行模式

必须先说明它是持久配置、当前轮冻结状态、运行态 UI 模式、单次执行参数，还是能力开关。不能把这些概念混在一起。

### 2.3 新增 provider / 外部集成

优先使用稳定协议接口。只有目标来源没有协议能力、必须依赖页面操作时，才新增浏览器或 UI 自动化分支。新增 provider 必须补齐配置、调度、错误处理、状态落盘、文档和测试。

本项目当前运行态是固定的 `plus`（ChatGPT 账号内部兼容 ID）和 `relay` 两槽位。扩展为任意账号池属于产品范围变化，必须先更新 PRD，不能仅通过循环 UI 或复用 legacy profile command 偷渡。

Relay 连接必须同时在前端做即时体验校验、在后端做权威校验；只接受无内嵌凭据/query/fragment 的 HTTPS Base URL，HTTP 仅允许 loopback。API Key 只能通过 password 表单进入，首次必填；后续只有规范化 URL 的 origin 不变时才允许空值保留旧密文，scheme/host/port 改变必须输入新 Key。Key 不得回填或回显。槽位中只存 DPAPI 密文；受管 Relay provider ID 必须由单一常量定义，当前只接受 `openai_custom`。激活 Relay 时允许且仅允许把解密值写入 live `config.toml` 的受管 `experimental_bearer_token`，并必须把同一表中的 `supports_websockets` 权威设为 `true`；切回 Account 必须按同一来源删除整个受管 provider 表。任何 TOML 解析错误、日志、回执或 UI 都不得包含 Key。Relay 首次切换或地址/凭据变化后必须让用户选择验证或直接切换；直接路径不得联网。连通性验证不得跟随重定向、不得输出响应正文或 Key，只以 2xx 判断 URL/TLS/HTTP/鉴权，不读取、推荐或判断模型列表。运行态 `exact` 判定必须比较所有影响请求路由的 provider 字段，包括 Base URL、bearer 与 WebSocket 开关；`supports_websockets` 为 `false` 或缺失时不得报告 exact。

手机连续性必须使用独立版本化 cutover/queue；首次初始化只读取 SQLite thread ID，不扫描或上传旧 JSONL。队列只保存 thread ID、source fingerprint、typed 状态和脱敏失败分类，不保存正文、路径内容或凭据。自动领取只允许 cutover 后全新、未归档、受管 Relay 会话；单批最多 8 thread/8 MiB，切回 Account 最多 4 批且总预算 30 秒，仍 deferred 必须保持 Relay route、不得先切回再隐藏会话。canonical 就绪后发布只更新 Account/Remote 数据库视图并继续同一正文；若原生兼容必须生成 provider-specific header，只允许操作期临时适配且结束归零。分叉必须保留多个 branch 并返回 conflict，禁止 last-write-wins。没有手机侧证据时 UI 只能写“本机 Remote 已发布”或“已提交到手机同步”。

Relay 会话可见性必须通过独立 SQLite 视图保证：`state_5.sqlite` 使用 Online Backup 生成 provider-normalized sibling view，三个全局 SQLite 只在 WAL checkpoint、同卷 hard-link、相同 file identity 和 `quick_check` 通过时共享；不得批量改写 Account 原库、复制全局库或原 JSONL。Relay route 必须同时写入受管 `sqlite_home`、`experimental_bearer_token`、`requires_openai_auth = true` 与 `supports_websockets = true`；`requires_openai_auth` 只维持 Desktop 官方账户识别，不替代 Relay 请求凭据。

Relay 切换不得发送 `/models` 或其他独立网络探测，也不得以外部链接状态阻断本地请求路由切换；只执行本地 URL/凭据/官方登录态 preflight、受管配置写入和写后 route exact 校验。请求路由 mutation 禁止进入大会话 planning/capacity/checkpoint/GC 链路；成功 terminal 后只允许后台排队 coalesced Shadow，不得调用 legacy 增量/完全同步正文物化。

内置 Skill 接入必须使用编译期固定 ID、固定文件 allowlist、来源/版本/hash manifest 和后端推导目标；不得让前端传任意下载 URL、源路径、目标路径或文件名。Skill 状态必须区分 missing/current/update available/local drift/unmanaged/invalid，未知目录和本地修改不得静默覆盖。安装/更新属于关闭态 mutation，必须共用 mutation guard、同卷 stage、完整旧目录备份、原子激活、后置 hash 验证、崩溃 journal 恢复和 typed receipt。

Skill 的服务 URL 与 Key 属于用户配置而不是包内容。URL 在前后端校验，非 loopback HTTP 拒绝；Key 使用 password 输入、首次必填、后续空值保留，只能以 Windows DPAPI 密文保存。安装包、Skill 文件、非敏感 config、日志、回执、错误和测试产物均不得包含真实 Key；PowerShell helper 的 DPAPI 格式必须用跨 Rust/Windows PowerShell 契约测试验证。

### 2.4 前后端状态与回执

- Dashboard 数据必须按领域建模为 `loading | ready | error`；某个 Tauri command 失败时保留该域错误，禁止替换成空数组、零计数或绿色安全状态。
- 应用首屏只加载 runtime 必需域；会话扫描、managed inventory 和备份 payload 哈希等昂贵域必须按需加载。请求路由结果始终刷新 runtime 域；只有 typed incremental `applied` 才标记 session stale，apply `failed/deferred` 可标记 backup stale。禁止为了“看起来同步”触发全量扫描。
- v0.3 存储卡首屏只读取最近一次 Shadow 报告；显式扫描使用 blocking worker，切换成功后的扫描必须在 durable terminal 之外后台排队。进程内请求必须 single-flight/coalescing，多个 Switch 进程还必须竞争固定根 Windows 独占 scan lease，不得并发写 cache/report，也不得增加普通账号切换延迟。UI 必须明确展示在线仅扫描、不删除，不能把 potential reclaim bytes 写成已回收空间。
- “存储”页必须把 control state 与 Shadow report 分开：显示 canonical 会话、安全副本、冲突、真实已回收 bytes 和安全窗口状态，并明确在线始终只扫描；未迁移时迁移入口可用而合并/GC/冲突/降级 apply 按合同禁用。迁移 UI 显示只读预检、完整备份、真实恢复验证、原子计划、提交验证五步；高风险提交必须要求 writer-closed 明示，冲突默认 defer，时间不可靠时不显示“较新”推荐。
- 备份域按需加载时应把可恢复 full backup 与检查点空间状态作为独立 `DomainState` 返回；先完成会执行 legacy 迁移的备份列表读取，再调用持 mutation guard 的检查点 inspect，禁止两个 guard 入口并发。空间状态必须区分总占用、严格证据可回收项、安全保留项、警告和最近清理结果。手工清理没有后端 typed 子阶段时，只能通过当前页面展示单一 indeterminate 运行态，并按 `attemptedCount` / `failedCount` 区分成功、成功但有保留说明和真实 partial；不使用确认弹窗、伪造分步进度、伪造百分比或模糊“已优化”文案。
- 备份域刷新若已有 Promise 执行中，新的 mutation 后刷新请求必须标记 queued，并在旧请求 settle 后至少补跑一次最新扫描；不得永久复用 mutation 前快照或在成功回执旁继续显示旧可回收数值。
- 写操作门禁必须依赖真实文件/SQLite/运行态域，而不是“页面加载完成”或“文件路径存在”。
- 窗口关闭监听必须在应用挂载时预注册；注册未 ready 或失败时切换必须 fail closed。前端必须阻止所有原生 close event，再调用零参数 typed `request_app_exit`；后端取得同一 mutation guard 后才允许 `app.exit(0)`，但调度后进程仍存活不得永久泄漏 shutdown reservation：必须有有界 watchdog 释放，前端在 watchdog 之后重试。mutation 繁忙时返回 pending，前端必须显示已排队，不得丢失关闭请求或直接 destroy 窗口；不得为未使用的 `destroy()` 扩大 capability。切换完成后的 runtime refresh pending 必须继续禁用两个切换入口，直到最新请求 settle。
- mutation 失败、后台刷新失败与 unknown/缺失终态不得在多个全局/局部 banner 重复显示。unknown 终态必须保守提示用户先不要重新打开 ChatGPT，不能在没有 typed 证据时承诺“数据未变化”。
- 运行态只展示“已保存”和“当前激活”；只有 `confidence = exact` 可标记当前并跳过切换，`mode` 只能提示重新应用。历史 `lastVerifiedAtMs` 可被兼容读取，但不构成产品状态或网络健康证明。
- 跨层 mutation 返回 typed receipt；新增/修改字段时必须同步 Rust serde、`src/types.ts`、`src/api.ts`、UI 展示和契约测试。
- `switch_runtime` 包含本地目标校验、进程等待、SQLite 会话视图准备和配置 I/O，必须放入 Tauri blocking worker；不得包含 Relay 网络探测。请求路由不得混入 scoped backup、全量 JSONL 扫描、provider materializer 或 GC。会话视图准备必须先于 config write：Relay/Account 都验证 provider-normalized state view 与共享全局库 identity；准备失败属于写入前失败并保留当前 route。进度通过 `Channel<RuntimeSwitchProgress>` 从真实后端阶段产生，并明确区分写入前失败、配置已回滚和回滚失败。
- 切换 task overlay 是唯一运行态焦点层：invoke 前打开，真实 phase/终态/incremental/launch warning 全部在同一实例展示；后端细 phase 对 Account 与 Relay 都聚合为最多 6 步，逐步展示由相邻时间戳计算的耗时并在终态标出最慢步骤，不展示伪造百分比。重复点击不得创建第二层，运行中不得因 Escape、遮罩或关闭按钮隐藏；若收到应用退出请求，同一层必须显示安全排队回执。
- 旧 `sync_all_sessions` wire 必须保持不可达；当前 `merge_and_repair_sessions` 只在 canonical 迁移已提交后执行 Missing/Equal/EqualExceptProvider/Prefix 和数据库视图修复，迁移前 typed blocked，Divergent/Unknown 不覆盖。它与检查点 inspect/cleanup 等包含容量扫描、目录遍历、SQLite/JSONL 或严格日志解析的命令都必须使用 async wrapper + blocking worker；合并修复用独立进度 Channel 展示关闭、完整备份、对账、记录和受控启动阶段。

### 2.5 前端交互与视觉合同

- 生产 UI 禁止使用 `window.alert`、`window.confirm`、`window.prompt` 和原生 `<dialog>` 触发浏览器/系统模态弹窗。配置、冲突覆盖、会话合并、恢复点/Full 删除确认、错误与恢复提示都必须在当前页面内完成；v0.3 不得出现会话永久删除确认。运行态切换只允许一个 portal/modal task overlay，并保留可访问名称、焦点捕获/恢复和 `role="status"` / `role="alert"` 等语义。
- 图标统一使用 `lucide-react`；界面文案和装饰禁止 emoji，存在对应 Lucide 图标时禁止手写 SVG。图标按钮必须有可访问名称，不熟悉的纯图标操作必须提供 tooltip。
- 视觉调整必须延续 ChatGPT Switch 的统一响应式系统，避免卡片嵌套、无意义装饰和信息遮挡；至少实际检查 1200×820、900×640、390×844，页面不得横向 overflow，overlay 内容可独立滚动且底部操作可达。
- 长流程必须让用户看到当前真实阶段、当前步骤耗时、已完成步骤各自耗时、最慢步骤和终态；耗时按事件顺序取下一条后端时间戳，同毫秒事件不得共享后一阶段耗时。按钮 busy、页面任务执行器和后端终态必须来自同一操作，不得出现命令已结束但 UI 仍永久 loading 的分叉状态。
- 诊断面板和失败导出属于同一页面内支持 UI：必须使用 Lucide、保持隐私说明常驻、只展示状态/占用/导出回执而不展示原始事件；导出、打开位置和清理各自保持真实 busy/error/success，关闭或切页不得泄漏局部任务状态，也不得让诊断错误替代原 mutation 反馈。

### 2.6 平台边界与外部只读集成

- 当前唯一发布目标是 Windows x64 便携 EXE；没有对应目标编译和运行证据前，不得宣称支持 macOS/Linux。
- Cargo release profile 必须保持 `opt-level = "z"`、`lto = true`、`codegen-units = 1`、`panic = "abort"`、`strip = "symbols"`。最终资产只能通过 `scripts/pack-windows-release.ps1` 对 raw 副本打包：固定官方 UPX 5.2.0 与 EXE SHA-256 `F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D`，禁止从未验证 PATH 取工具或原地压缩 raw。
- CI 下载 UPX 的官方 ZIP 必须固定 SHA-256 `B471EBF1B7F20F4A89150264ED9A008A2A5BFD247F3C6D1184A75BB59CA08F5D`，解包后仍复核 EXE hash。raw 与 packed 都必须通过 release contract；packed 还必须通过 `upx -t`、PE32+ x64、双版本与 3,000,000 bytes 硬门禁，artifact 只能包含 packed 的裸 `codex-switch.exe`。
- 与平台无关的新模块默认不得直接读取 `APPDATA`、调用 Windows shell 或拼接反斜杠路径；平台能力必须收敛到独立模块、target dependency 或 `cfg` 边界。
- 凭据保护、进程控制、跨进程锁和 Skill runtime 属于平台能力。缺少安全实现时必须明确拒绝，禁止用可逆占位伪装可用。
- 固定仓库更新检查属于外部只读集成：后端固定 endpoint，设置超时/响应上限、禁止元数据重定向、验证稳定 SemVer，错误不得回显响应正文；前端不得传任意仓库或下载 URL。
- 启动更新检查必须与 runtime/session/backup 数据域解耦并保持非阻塞；应用不是常驻工具，不新增后台服务或运行中轮询。
- Windows 单文件自更新必须只接受唯一固定名称的 Release EXE，要求 GitHub SHA-256 digest，按元数据大小和全量流式 hash 双重验证；下载 URL 从固定仓库和已验证 tag 推导，只允许 HTTPS GitHub Release 资产重定向。v0.2.1 下载连接超时为 30 秒、总下载超时为 10 分钟；helper readiness 超时必须 kill + wait，staging 清理必须有界重试。当前 EXE 复制为同版本 helper，父进程只能在 helper 完成计划/路径/hash/进程句柄预检并写入 readiness 后退出。
- 公开 UI 和窗口标题使用 ChatGPT Switch，但 GitHub Release 资产必须继续唯一命名为 `codex-switch.exe`；v0.1.9 updater 固定校验该名称，未经兼容迁移不得改为 `chatgpt-switch.exe`。
- EXE 替换必须在目标目录同卷完成：先写 replacement 并复核 hash，再备份旧 EXE、激活 replacement。新进程必须在 Tauri `RunEvent::Ready` 后写入绑定受控 plan、状态与目标 hash 的 ACK；helper 在 ACK 前保留 backup，早退/超时必须终止新进程并恢复旧 EXE。staging 名必须来自 Windows CSPRNG，目录按当前 token 是否 elevated 施加只允许 SYSTEM/Administrators 或 SYSTEM/owner 的受限 DACL，并在准备和 helper 执行期间持有目录句柄。replacement 的每个不可逆阶段必须先后持久化并校验 journal，重入时按 journal 和旧/新 hash 决定继续或回滚。debug 和非 Windows 构建必须明确拒绝真实安装。
- Release 型 Trellis 任务必须遵守不可倒置的收口顺序：最终 `check.md` 先满足 PASS 门并保持任务 active；随后提交、推送、创建 tag，完成 tag-CI、公开 Release 回下载和 updater 成功/回滚证据；只有这些公开入口证据都绑定到精确发布 commit 后，才允许依次执行 `task.py finish`、`task.py archive <task> --no-commit` 并生成/校验 schema v2 completion receipt。不得在 public/updater 证据之前归档任务或写一个宣称完成的 receipt。

## 3. 测试规范

### 3.1 原则

任何结构性改动都必须伴随测试迁移或新增。优先测试：模块是否接入、核心纯函数是否仍可验证、回退/停止/异常传播是否仍正确。

### 3.2 最低要求

- 改 JavaScript / TypeScript：至少运行该项目真实可用的语法、类型或测试入口；只有纯 JS 文件且无项目命令时，才退回 `node --check <file>`。
- 改 Python：至少对修改过的 Python 文件运行 `python -m py_compile <file>`，若项目有 `pytest`、`unittest`、`ruff`、`mypy` 或 CI 入口，以项目真实命令为准。
- 改 Go / Rust / Java / 其他语言：运行该语言和本项目真实使用的最小语法、类型、测试或构建检查，例如 `go test ./...`、`cargo test`、`mvn test`；不可硬套 Node/JS 规则。
- 改核心逻辑：运行项目当前真实回归命令；如果清单、README、长期文档、CI 配置和实际可运行命令冲突，以当前实际可运行命令为准，并说明证据。
- 改文档-only：至少做文档内容、链接和乱码检查。
- 测试失败时不得提交；除非用户明确要求保留失败状态用于排查，否则必须先修复。

### 3.3 本项目发布门禁

当前 Windows release 至少执行：

```powershell
npm test -- --run
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path src-tauri/Cargo.toml
npm run tauri -- build --no-bundle
npm run tauri -- build
npm run check:release
.\scripts\pack-windows-release.ps1 -UpxPath "<verified-upx.exe>"
```

- 桌面 UI/原生交互实测必须使用 Tauri CLI 产物；`npm run tauri -- build --no-bundle` 是不生成安装包的快速候选，仍执行前端构建并绑定生产 custom protocol。裸 `cargo build --release` 只属于 Rust 编译证据，可能继续访问 `devUrl`，不得把其页面错误误判为产品回归或当作 UI E2E 通过。
- `.github/workflows/ci.yml` 必须在 `windows-latest` 上覆盖前端测试/类型/构建、Rust fmt/clippy/test、raw Tauri release 编译/合同、固定官方 UPX ZIP/EXE 双 hash、copy-only packing、`upx -t`、packed 合同/3,000,000 bytes 上限，以及只留存 packed artifact；CI 文件存在不等于本轮已通过。
- 备份、切换、迁移、冲突、离线 GC、降级、删除和恢复等高风险变化必须有临时目录或临时 `CODEX_HOME` 测试，至少覆盖幂等、故障注入、并发 TOCTOU 和回滚终态。真实主库破坏性测试只能使用本机隔离副本，最终真实主库只做只读 preflight。
- 既有备份测试必须覆盖 v4 scope/binding/role、Full/Sessions active+archived、manual Full/restore safety、损坏候选和局部点排除；旧 hard-delete scope 只能作为 v0.2.x test fixture 覆盖，并必须另行断言 v0.3 production command registry 与 UI 均无会话 hard-delete。请求端切换不会创建 backup/checkpoint。v0.3 迁移备份另测未加密 manifest、SQLite Online Backup、空间/根冲突、实际隔离恢复、真实 Codex runtime verify、损坏/sidecar/长路径与无 `runtimeVerified` 禁止 Apply；不得把两套备份合同混为一谈。
- 请求端切换测试必须覆盖真实 phase（含 `validatingOfficialAuth`、`syncingIncrementalSessions`、`repairingAppState`、`launchingApp`）、缺失/非官方 auth 写前失败、Account↔Relay 前后 auth bytes/mtime 不变、Relay bearer/`requires_openai_auth = true`/`supports_websockets = true`/隔离 `sqlite_home` 写入与 Account 清除恢复、旧槽位 WebSocket 缺失或 `false` 的纠正、live WebSocket 漂移降级为 mode、Relay 副本 provider 归一且 Account 原库不变、Account 发布 deferred 时 route 不写、配置写后失败只回滚 config、进程重启阻断恢复、零全量 JSONL scan/checkpoint/GC、runtime refresh pending、单一错误面和按需域不失效；所有成功切换只在 durable terminal 后启动 ChatGPT，launch failed 不回滚且可重试。
- legacy 会话同步测试继续覆盖 source 尾/session ID/len/hash 漂移、抢先 Create、完整 import、provider marker/slot 幂等、divergence 与旧文件 byte/hash/mtime；同时必须有 reachability 测试证明这些 provider materializer 不从 v0.3 普通 route/merge command 调用。连续切换 100 次必须保持永久 provider 正文新增为 0。
- v0.3 canonical 对抗矩阵必须覆盖：Equal/EqualExceptProvider/双向 Prefix/双尾、消息乱序、缺失工具结果、非法 JSONL、marker 缺失/伪造/hash 漂移、三份以上同 ID、文件名时间反向；主库/Account/Relay/Shared/legacy/backup/WAL 引用；扫描后新增引用、删除前引用变化；Desktop/CLI append、双进程分叉、扫描中再次切换、hash 间变化、数据库提交竞争；每个迁移阶段崩溃、强杀/重启、磁盘满、占用、权限/SQLite 写失败、恢复包损坏、回滚中断；迁移/GC/降级重试幂等。任一有效消息丢失、工具关系损坏、引用中文件删除、分叉误合并或并发误删都阻断发布。
- 备份前端测试必须覆盖旧扫描 pending 时 queued rerun 的最终状态、按需列表不再裁成 5 项、Full 删除二次确认/回执，以及恢复点删除后刷新显示更旧候选；不得只验证 invoke 次数。
- Windows 进程控制测试必须覆盖 PID reuse、受管根/后代、独立 CLI 阻断、最后写前复检、温和关闭与强制兜底；ChatGPT launch 还必须覆盖 AUMID 缺失/冲突、已运行、原生激活失败、验证超时和成功后同身份根出现，禁止 PATH/任意 EXE fallback。updater 测试必须覆盖 30 秒/10 分钟超时合同、helper kill + wait、cleanup retry、DACL、目录句柄、journal 中断、hash 篡改和 Ready ACK。
- 容量测试必须覆盖迁移完整备份 + SQLite workspace + staging 的同卷/跨卷 reserve、`u64` overflow、缺失子目录 ancestor 查询，以及空间不足时在 ChatGPT close/第一份备份/任何 JSONL 发布前零写入；禁止边删除旧副本边腾迁移空间。
- 会话性能回归必须证明普通切换不等待全库 scan，连续切换不增加正文文件；Shadow 缓存命中只作为性能证据，不能作为删除证据。合成大 Home 不得被误述为真实 ChatGPT 重索引 wall-clock。
- 前端生产验证必须覆盖 1200×820、900×640、390×844，逐档检查无横向溢出、主导航/关键按钮 hit-test、存储页 `aria-current` 往返、reduced-motion、中文与 console error；packed EXE 必须在隔离 `APPDATA`/`CODEX_HOME` 且无 Vite listener 时真实启动并 graceful close。
- v0.2.0–v0.2.7 精确旧版 EXE/CLI 属于下载或本地构建可执行文件；每次真正执行前必须获得行动时确认，只能使用隔离目录并验证列表、恢复和继续会话。当前新版 runtime 验证不能替代目标旧版验证。
- Skill 安装测试必须使用临时 `CODEX_HOME` / `APPDATA`，覆盖 clean install、幂等 current、未知目录/漂移确认、旧目录备份、URL 拒绝、Key 密文与空 Key 保留；vendored PowerShell/Python 至少做语法解析，并验证 Rust DPAPI 密文可被 Windows PowerShell 读取。
- 诊断专项测试必须覆盖 event schema/ID/operation binding、所有用户主动 mutation 的 begin-before-lock/spawn/早退、真实 phase/安全 branch、attempt 到 durable operation 的只绑定关联、worker join/后置步骤和唯一 typed terminal、无 durable record 的完整 lifecycle、后台成功不落盘/失败落盘，以及 operation-log 写入失败时仍保持 diagnostics fail-open 和原业务 `Result`。还必须覆盖字段与文本脱敏、受管路径 tokenization/未知绝对路径拒绝、敏感 key/secret shape、长度/深度/数量边界、每段唯一截断尾/dirty tail 封存与内部损坏、segment 创建时间轮转、event timestamp 最近 7 天精确过滤、10 MiB/512 KiB 容量、独立 store 与 Windows root named mutex 并发、panic 零等待、post-prune 终态、schema/ancestor reparse/clear。必须证明诊断 append/rotate/export/clear 前后 `operations.jsonl` byte-exact 不变。
- 导出专项测试必须覆盖固定五文件 allowlist、每项 bytes/SHA-256、manifest unavailable/warning、health 只读字段与逐项降级、ZIP 前后自检、包大小上限、Known Folder 解析/重定向/权限失败、用户显式备用诊断目录、同名冲突数字后缀、same-directory staging、正常失败的 staging Drop 清理与 clear 排除边界、write-through no-clobber、opaque export ID containment 和打开目录失败。前端必须覆盖顶部面板、失败页有/无真实 ID、下载失败后的重试/主动改存、隐私说明、busy/成功/失败、原业务错误保留、清理确认、焦点恢复和 390×844 可达。
- 诊断系统正式发布必须在隔离 `APPDATA` / `CODEX_HOME` 做失败注入并从生成 ZIP 外部扫描凭据、正文、用户名/机器名、业务 ID 和原始绝对路径，再用真实 packed EXE 完成导出 UI/E2E。PR/main/tag CI、annotated tag、唯一 tag-CI packed 资产、GitHub Release/Latest、公开回下载 bytes/hash/PE/双版本/UPX 合同和正式旧版到目标版 updater smoke 任一未完成时，只能标记目标/草稿/pending，不能写成已发布或已验证。
- 发布回执必须区分已运行、未运行和被环境阻断的检查；不得把局部测试写成“全量通过”。
- 本地临时候选大小/hash 不能写成最终 Release。只有 tag-CI packed artifact 的大小/hash、Release 重下载合同和真实 `v0.2.0 -> v0.2.1` 一键更新全部完成后，才能宣称首跳交付闭环。
- 敏感扫描和中文乱码检查是发布门禁，不得被普通单元测试替代。

## 4. 文档更新规范

必须更新长期文档的场景：

- 文件新增、删除、重命名或职责变化：更新 `项目文件结构说明.md`。
- 功能链路、运行模式、状态流、输出文件、故障归因变化：更新 `项目完整链路说明.md`。
- 开发流程、架构边界、测试要求、协作约束变化：更新当前文件。

不能只改代码不改文档；不能让链路文档落后于真实实现；不能让临时方案文件替代根目录长期文档。

## 5. 乱码要求

所有中文文档、中文注释、中文日志、UI 文案、错误提示文案都必须避免乱码。修改任何包含中文的文件时，必须把乱码检查视为与功能正确同级的必做项。

## 6. AI 自检清单

每次修改后至少自问：

1. 我开发前是否重新核对了 `AGENTS.md` 和三份根目录长期文档？
2. 我这次新增逻辑是不是应该下沉到模块？
3. 我有没有漏掉配置、状态、日志、错误处理、测试或文档中的一环？
4. 我有没有补或迁移测试？
5. 我有没有更新对应长期文档？
6. 我修改的中文内容是否无可见乱码？
7. 我有没有新增不必要的兼容分支、兜底分支或旧逻辑？
8. 我有没有保护敏感文件，不回显真实 token、密码、API Key 或账号明细？
9. 我有没有把 scan failure 误当成空数据，或把 mode-only 误当成 exact？
10. mutation 是否完整覆盖备份、后置校验、补偿恢复、typed receipt 和操作记录？
11. 长流程状态是否来自真实后端阶段，昂贵 session/backup 域是否保持按需加载？
12. 生产 UI 是否仍然存在 emoji、手写 SVG、浏览器/系统模态弹窗或不可访问的纯图标按钮？

## 7. 完成标准

满足以下条件时，才可以视为一次合格开发完成：

- 代码职责边界清晰。
- 新旧功能链路完整。
- 开发清单各阶段已逐项完成并自检。
- 关键路径测试或检查已通过。
- 根目录长期文档已同步。
- 没有可见乱码。
- 工作区范围已确认，没有误提交草稿、密钥、运行态数据或无关文件。

## 8. AI 最终回执要求

完成开发或审查后，最终回复必须简明说明：

1. 改了什么。
2. 是否遵守本规范，尤其是架构边界、文档同步、测试和乱码检查。
3. 跑了哪些测试或检查。
4. 是否提交、提交号是什么。
5. 是否推送、推送到哪个分支。
6. 如果有未完成项、未运行测试或残余风险，必须明确说出。

<!-- DXM-TRELLIS:START -->

## DXM + Trellis 协作规则

Trellis 只用于中大型开发任务的 PRD、任务状态和检查沉淀。默认路由：

| 场景 | 默认处理 |
| --- | --- |
| 只分析 / 先看看 | 只读，不建 task |
| 小修 / 单点 bug / 单文件文档调整 | DXM inline，不建 task |
| 新功能 / 多模块 / 架构 / 跨文件 / 长周期 | project-grill 后建 Trellis task |
| 需求不清楚但会继续开发 | 先 grill-with-docs，再决定是否建 task |
| 用户明确 scaffold only / 先别问 | 只 scaffold，不 grill，不建 task |

启用 Trellis 时必须保持 `session_auto_commit: false`，并遵守本项目 Git/PR 授权规则。

<!-- DXM-TRELLIS:END -->
