# 项目开发规范（AI协作）

> 项目：`codex-switch`
> 根目录：`<repo-root>`
> 初始化日期：`2026-06-23`

本文档是面向 AI 与开发者的大项目开发规范。目标是让项目更清晰、更可维护、更可测试，而不是让 AI 只追求“眼前能跑”。

<!-- DXM-DOC-RULES:START -->

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

涉及 live Codex Home、shared-sessions、运行态凭据或工具备份的 mutation，必须按以下顺序设计和审查：

```text
preflight -> plan/dry-run -> backup -> apply -> verify -> typed receipt
                                      \-> rollback -> verify rollback -> terminal status
```

- 切换、删除、恢复可见和完整恢复必须在后端写入前再次确认 ChatGPT 受管进程已关闭；只有独立会话同步允许热写入。
- 运行态切换必须在关闭 ChatGPT 前完成 plan、受管 root 冲突检查、备份容量预检和 relay 连通性验证；这些 fail-closed 错误不得先打断正在运行的 ChatGPT。
- Windows 关闭态操作必须基于 ToolHelp PID/PPID 关系证明受管进程树，只把 `ChatGPT.exe` / `OpenAI.Codex.exe` 作为受管根，不得按模糊进程名误杀独立 `codex.exe` CLI。先温和关闭并等待，超时后只能强制结束重新枚举后仍可证明身份的受管进程。
- 关闭态 mutation 可能影响 current/shared 两根时，必须在写入前分别创建覆盖本次实际写集、可验证且可补偿的 scoped 快照；任一根部分成功后另一根失败，必须补偿恢复两根。hard delete 会写四个受管数据库，必须使用 full scope。
- 热同步允许 ChatGPT 运行，是上述规则的显式例外：失败时恢复工具内部 shared 根，但不得用操作前快照覆盖可能已产生并发变化的 live current Home；必须保留并回报 current 安全备份供人工恢复。
- 文件写入、复制和 JSONL 重写统一走 `file_ops` 原子操作；不得直接 `fs::write` 覆盖 live 文件。
- 所有会修改运行态、凭据、current/shared 或备份目标的 command 必须共用 mutation guard：进程内 try-lock + Windows 独占 `mutation.lock` 文件句柄；新增入口不得绕过同进程/跨进程串行化。
- 一键更新安装也必须取得同一 mutation guard；helper readiness 成功后父进程进入 shutdown-pending，退出前不得释放跨进程锁或接受新 mutation。前端禁用只能改善体验，不能替代后端互斥。
- 当前没有备份 retention/prune；任何清理策略必须先定义保留周期、容量上限、在用/安全快照保护与用户确认，不能在列表扫描或其他无关流程中静默删除。
- manifest v3 必须显式记录 `scope` 与 `trackedDatabases`。runtime / sessions / state-only 只覆盖实际写集；只有 hard delete、手工 full backup 和 restore safety 覆盖 `state_5.sqlite`、`goals_1.sqlite`、`memories_1.sqlite`、`logs_2.sqlite` 四库。局部快照只能用于对应操作补偿，不得进入 UI 可恢复列表；恢复 v2 时不得假装其含 v3 新增字段。
- 纳入 scope 的 SQLite 必须使用 Online Backup API，不得直接复制 WAL/SHM。所有 payload 必须 DPAPI 加密并记录大小/SHA-256；容量估算必须按本次 scope 的实际 payload 加上 SQLite workspace、加密/manifest 开销和明确安全余量，恢复后按 `trackedDatabases` 执行适用的 `quick_check`。
- 成功回执必须包含可关联的 operation ID、备份引用、计数、回滚终态和警告。高风险长流程还必须通过后端事件 `Channel` 上报真实阶段和 typed 终态；禁止用前端计时器或伪造百分比代替事实。命令完成后必须释放 UI busy，刷新失败必须与 mutation 失败分开表达。

## 2. 新增功能接入规范

### 2.1 新增配置项

必须同步检查：默认值、读取、归一化、保存、恢复、导入导出、UI/CLI/API 暴露、文档、测试。

### 2.2 新增流程节点或运行模式

必须先说明它是持久配置、当前轮冻结状态、运行态 UI 模式、单次执行参数，还是能力开关。不能把这些概念混在一起。

### 2.3 新增 provider / 外部集成

优先使用稳定协议接口。只有目标来源没有协议能力、必须依赖页面操作时，才新增浏览器或 UI 自动化分支。新增 provider 必须补齐配置、调度、错误处理、状态落盘、文档和测试。

本项目当前运行态是固定的 `plus`（ChatGPT 账号内部兼容 ID）和 `relay` 两槽位。扩展为任意账号池属于产品范围变化，必须先更新 PRD，不能仅通过循环 UI 或复用 legacy profile command 偷渡。

Relay 连接必须同时在前端做即时体验校验、在后端做权威校验；只接受无内嵌凭据/query/fragment 的 HTTPS Base URL，HTTP 仅允许 loopback。API Key 只能通过 password 表单进入，首次必填；后续只有规范化 URL 的 origin 不变时才允许空值保留旧密文，scheme/host/port 改变必须输入新 Key。Key 不得回填或回显。连通性验证不得跟随重定向、不得输出响应正文或 Key，成功响应也必须设置严格字节上限。

内置 Skill 接入必须使用编译期固定 ID、固定文件 allowlist、来源/版本/hash manifest 和后端推导目标；不得让前端传任意下载 URL、源路径、目标路径或文件名。Skill 状态必须区分 missing/current/update available/local drift/unmanaged/invalid，未知目录和本地修改不得静默覆盖。安装/更新属于关闭态 mutation，必须共用 mutation guard、同卷 stage、完整旧目录备份、原子激活、后置 hash 验证、崩溃 journal 恢复和 typed receipt。

Skill 的服务 URL 与 Key 属于用户配置而不是包内容。URL 在前后端校验，非 loopback HTTP 拒绝；Key 使用 password 输入、首次必填、后续空值保留，只能以 Windows DPAPI 密文保存。安装包、Skill 文件、非敏感 config、日志、回执、错误和测试产物均不得包含真实 Key；PowerShell helper 的 DPAPI 格式必须用跨 Rust/Windows PowerShell 契约测试验证。

### 2.4 前后端状态与回执

- Dashboard 数据必须按领域建模为 `loading | ready | error`；某个 Tauri command 失败时保留该域错误，禁止替换成空数组、零计数或绿色安全状态。
- 应用首屏只加载 runtime 必需域；会话扫描、managed inventory 和备份 payload 哈希等昂贵域必须按需加载。切换完成后只刷新 runtime 域，并把 session/backup 标记 stale，禁止为了“看起来同步”立即重复全量扫描。
- 写操作门禁必须依赖真实文件/SQLite/运行态域，而不是“页面加载完成”或“文件路径存在”。
- 运行态的“已保存”“当前激活”“最近验证”是三个独立概念；只有 `confidence = exact` 可标记当前并跳过切换，`mode` 只能提示重新应用。
- 跨层 mutation 返回 typed receipt；新增/修改字段时必须同步 Rust serde、`src/types.ts`、`src/api.ts`、UI 展示和契约测试。
- `switch_runtime` 等包含网络、scoped 备份和大量 SQLite/文件 I/O 的命令必须放入 Tauri blocking worker；进度通过 `Channel<RuntimeSwitchProgress>` 从真实后端阶段产生，并明确区分写入前失败、已回滚和回滚失败。

### 2.5 前端交互与视觉合同

- 生产 UI 禁止使用 `window.alert`、`window.confirm`、`window.prompt` 和原生 `<dialog>` 触发浏览器/系统模态弹窗。配置、覆盖、dry-run、删除确认、进度、错误与恢复提示都必须在当前页面内完成，并保留可访问名称、焦点恢复和 `role="status"` / `role="alert"` 等语义。
- 图标统一使用 `lucide-react`；界面文案和装饰禁止 emoji，存在对应 Lucide 图标时禁止手写 SVG。图标按钮必须有可访问名称，不熟悉的纯图标操作必须提供 tooltip。
- 视觉调整必须延续 ChatGPT Switch 的统一响应式系统，避免卡片嵌套、无意义装饰和信息遮挡；桌面与移动宽度下文本、按钮、时间线、表格和页面内确认区都不能溢出或互相覆盖。
- 长流程必须让用户看到当前真实阶段、已完成阶段、耗时和终态；按钮 busy、页面任务执行器和后端终态必须来自同一操作，不得出现命令已结束但 UI 仍永久 loading 的分叉状态。

### 2.6 平台边界与外部只读集成

- 当前唯一发布目标是 Windows x64 便携 EXE；没有对应目标编译和运行证据前，不得宣称支持 macOS/Linux。
- 与平台无关的新模块默认不得直接读取 `APPDATA`、调用 Windows shell 或拼接反斜杠路径；平台能力必须收敛到独立模块、target dependency 或 `cfg` 边界。
- 凭据保护、进程控制、跨进程锁和 Skill runtime 属于平台能力。缺少安全实现时必须明确拒绝，禁止用可逆占位伪装可用。
- 固定仓库更新检查属于外部只读集成：后端固定 endpoint，设置超时/响应上限、禁止元数据重定向、验证稳定 SemVer，错误不得回显响应正文；前端不得传任意仓库或下载 URL。
- 启动更新检查必须与 runtime/session/backup 数据域解耦并保持非阻塞；应用不是常驻工具，不新增后台服务或运行中轮询。
- Windows 单文件自更新必须只接受唯一固定名称的 Release EXE，要求 GitHub SHA-256 digest，按元数据大小和全量流式 hash 双重验证；下载 URL 从固定仓库和已验证 tag 推导，只允许 HTTPS GitHub Release 资产重定向。当前 EXE 复制为同版本 helper，父进程只能在 helper 完成计划/路径/hash/进程句柄预检并写入 readiness 后退出。
- 公开 UI 和窗口标题使用 ChatGPT Switch，但 GitHub Release 资产必须继续唯一命名为 `codex-switch.exe`；v0.1.9 updater 固定校验该名称，未经兼容迁移不得改为 `chatgpt-switch.exe`。
- EXE 替换必须在目标目录同卷完成：先写 replacement 并复核 hash，再备份旧 EXE、激活 replacement。新进程必须在 Tauri `RunEvent::Ready` 后写入绑定受控 plan、状态与目标 hash 的 ACK；helper 在 ACK 前保留 backup，早退/超时必须终止新进程并恢复旧 EXE。staging 名必须来自 Windows CSPRNG，目录按当前 token 是否 elevated 施加只允许 SYSTEM/Administrators 或 SYSTEM/owner 的受限 DACL，并在准备和 helper 执行期间持有目录句柄。replacement 的每个不可逆阶段必须先后持久化并校验 journal，重入时按 journal 和旧/新 hash 决定继续或回滚。debug 和非 Windows 构建必须明确拒绝真实安装。

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
npm run tauri -- build
npm run check:release
```

- `.github/workflows/ci.yml` 必须在 `windows-latest` 上覆盖前端测试/类型/构建、Rust fmt/clippy/test、完整 Tauri release 编译、版本/PE/路径/敏感 marker 合同及 verified artifact 留存；CI 文件存在不等于本轮已通过。
- 备份、切换、双根同步/删除/恢复等高风险变化必须有临时目录或临时 `CODEX_HOME` 测试，至少覆盖幂等、故障注入和回滚终态。
- 备份测试必须覆盖 v3 `scope` / `trackedDatabases`、runtime/sessions/state-only 实际写集、hard delete/full/restore safety 四库范围，以及局部快照不会进入 UI 可恢复列表。
- 切换测试必须覆盖真实 phase 顺序、页面内阶段呈现、写入前失败/已回滚/回滚失败，以及完成后只刷新 runtime、session/backup 按需加载；不得只断言最终按钮文案。
- Windows 进程控制测试必须覆盖 PID reuse 复检、受管根/后代、独立 `codex.exe` CLI 不受影响、温和关闭与强制兜底。updater 测试必须覆盖 elevated/non-elevated DACL、staging 目录句柄、journal 中断恢复、hash 篡改和 Ready ACK。
- Skill 安装测试必须使用临时 `CODEX_HOME` / `APPDATA`，覆盖 clean install、幂等 current、未知目录/漂移确认、旧目录备份、URL 拒绝、Key 密文与空 Key 保留；vendored PowerShell/Python 至少做语法解析，并验证 Rust DPAPI 密文可被 Windows PowerShell 读取。
- 发布回执必须区分已运行、未运行和被环境阻断的检查；不得把局部测试写成“全量通过”。
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
