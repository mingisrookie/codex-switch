<div align="center">

# ChatGPT Switch

**保留官方登录态，只切换请求端，并把会话同步从十分钟级切换主链中拆出来。**

保存当前账号配置；配置一个 OpenAI-compatible API 中转站；live `auth.json` 始终保留官方 ChatGPT 登录。切入 Relay 时使用隔离 SQLite 会话视图，历史会话继续可见；切回 Account 前把本轮新 Relay 会话有界发布到 Account 视图，未收口时保留 Relay 而不是让会话暂时消失。旧历史不会被自动上传，可按单个会话或手动“完全同步”处理。切换由单一应用内 task overlay 展示不超过 7 个真实步骤及耗时，成功后受控重新打开 ChatGPT。

[快速使用](#快速使用) · [诊断与支持](#9-诊断与支持) · [下载 Release](https://github.com/mingisrookie/codex-switch/releases/latest) · [更新日志](CHANGELOG.md) · [安全说明](#安全说明) · [开发](#开发)

![release](https://img.shields.io/github/v/release/mingisrookie/codex-switch?display_name=tag&label=release&color=087f75)
![license](https://img.shields.io/badge/license-MIT-16a34a)
![platform](https://img.shields.io/badge/platform-Windows-2563eb)
![stack](https://img.shields.io/badge/Tauri-2.x-24c8db)
![runtime](https://img.shields.io/badge/ChatGPT-runtime-111827)

<br />
<br />

<img src="docs/assets/screenshot.png" alt="ChatGPT Switch 运行态控制台" width="920" />

</div>

## 项目定位

ChatGPT Switch 是一个 Windows 桌面工具，用来在 **ChatGPT 账号态** 和 **一个 OpenAI-compatible API 中转站态** 之间安全切换，同时保持本地会话可同步、可管理。公开 UI 和窗口标题使用 ChatGPT Switch；仓库名、`.codex`、`CODEX_HOME` 和 `plus` 等标识继续保留兼容命名。GitHub Release 资产仍必须唯一命名为 `codex-switch.exe`，因为 v0.1.9 updater 固定校验该资产名，不能改成 `chatgpt-switch.exe`。

> 当前源码目标版本为 `v0.2.7`；正式可下载资产及校验结果仍以 GitHub Releases 的 latest stable 为准。`v0.2.7` 的 PR/main/tag CI、GitHub Release/Latest、公开回下载 hash/版本合同和 `v0.2.5 -> v0.2.7` 一键更新证据，必须等本次发布最终闭环后再补，当前文档不把源码或本地候选写成已发布。

## 开发过程

本项目把 DXM 大项目协作规范也放进仓库，方便外部查看需求澄清、开发边界、链路说明和 PR 流程：

- [AGENTS.md](AGENTS.md)：Codex / AI 协作入口规则。
- [项目开发规范（AI协作）.md](项目开发规范（AI协作）.md)：开发、测试、文档同步和交付标准。
- [项目完整链路说明.md](项目完整链路说明.md)：运行态切换、会话同步和数据流说明。
- [项目文件结构说明.md](项目文件结构说明.md)：文件职责和维护边界。
- [开发者AI开发与PR提交流程.md](开发者AI开发与PR提交流程.md)：GitHub / PR / 发布流程。

## 能做什么

- 固定管理两个槽位：一个 ChatGPT 账号态、一个 API 中转站态；当前版本不承诺任意数量账号池。
- 保存当前 ChatGPT 账号配置前验证 live `auth.json` 的 `auth_mode = chatgpt`；覆盖已有账号槽位使用页面内确认，并保留加密历史版本。保存的认证副本只用于槽位历史/恢复证据，请求端切换不会把它写回 live `auth.json`。
- 配置一个 API 中转站：填写 Base URL、模型名和 API Key；Key 不回填，留空可保留同一 origin 的已保存 Key，并可独立验证连接。保存失败时页面内表单保留本次 Key 方便重试，保存成功或取消后销毁输入值；origin 改变时必须输入新 Key。首次切换或地址/凭据变化后，页面询问“验证连接后切换”还是“直接切换”，并记住选择。
- 在独立“技能”页安装固定来源的 `newapi-image2-client` 和 `grok-search`；Skill 按页面懒加载，不会让技能扫描故障污染运行态 Dashboard。
- Image2 与 Grok 分别填写自己的服务 URL 和 API Key；Key 使用当前 Windows 用户的 DPAPI 加密，Skill、非敏感配置、UI、回执和日志中都不保存明文。
- 顶部工具栏提供固定“诊断”入口；所有用户主动 mutation 都从 command boundary 建立 attempt，记录真实 phase/关键安全 branch/typed terminal，并在 durable operation ID 可用时关联。失败面拿到真实 operation/attempt ID 时可直接“导出本次诊断”，没有 ID 时导出保留窗口内的最近诊断。诊断事件写入独立有界存储，导出前自动再次脱敏并生成一个 ZIP，不改变原操作结果，也不修改用于备份清理证明的 `operations.jsonl`。
- 分开显示“已保存”“当前运行”“最近验证”；live `auth.json` 只需保持官方 `chatgpt` 登录态，请求路由字段必须精确匹配。官方 token 正常刷新不会把当前态误判为失配。
- 切换基于 live `config.toml` 应用最小请求路由 patch，只修改模型/service tier/provider/受管 `sqlite_home` 绑定，不覆盖 `model_instructions_file`、MCP、项目和其他全局设置。切换到 Relay 时把已用 DPAPI 保存的 Key 投影为受管 provider 的 `experimental_bearer_token`，同时写入 `requires_openai_auth = true` 保持 Desktop 官方账户识别，并写入 `supports_websockets = true` 允许 Responses WebSocket；切回账号态会删除整个受管 `openai_custom` provider 表和其中的明文 token，并恢复原 Account SQLite home。
- 点击切换后只显示一个页面内 task overlay；后端原始 phase/timestamp 全部保留，UI 聚合为“准备、验证 Relay、关闭 ChatGPT、应用请求端、验证并记录、增量会话、启动 ChatGPT”最多 7 步。请求路由 mutation 不进入会话全量扫描/同步、容量规划、checkpoint 或 provider GC；`auth.json` 从 preflight 到后置验证必须 byte-exact 不变。只有 `config.toml` 写入后的失败会回滚原始配置，官方登录态始终不参与回滚写入。
- 切入 Relay 前通过 SQLite Online Backup 把 Account 的 `state_5/logs_2/goals_1/memories_1.sqlite` 刷新到 `%APPDATA%\codex-switch\relay-sqlite`，只在副本中把 thread provider 归一为 `openai_custom`。会话正文仍使用同一个受管 `sessions/`，Account 原库不被改写。
- 首次启用手机连续性时只从 SQLite 记录当前全部 thread ID 作为 cutover，不扫描或上传既有 JSONL。以后只把 cutover 后全新出现、未归档且 provider 为受管 Relay 的会话加入 `%APPDATA%\codex-switch\mobile-continuity-v1.json` 持久队列；旧会话后续追加仍保持手动。
- 切回 Account 的配置写入前，在 ChatGPT 仍关闭的窗口中最多处理 4 批；每批最多 8 个、合计 8 MiB，总预算 30 秒。发布使用 immutable/no-clobber `openai` provider successor、SQLite transaction/quick-check 和官方 Remote 文件名/首条 provider 复核；预算内仍有排队项时 fail closed 并保持 Relay 请求端，不会让会话因提前切回 Account 而暂时消失。
- UI 只显示“本机 Remote 已发布”或“已提交到手机同步”，不把本地成功误报成“手机已同步”。发现本地附件/文件标记时显示“部分内容仅本机”；冲突或不满足安全证明时保留原分支并要求手动处理。删除与归档不传播、不提示。
- 手动“完全同步”关闭 ChatGPT，跳过旧的前端 dry-run 重复扫描，完整对账 current ↔ shared 的活跃会话；`archived_sessions/` 与已归档 SQLite 行不参与、不复活。旧 Relay 会话也可在 Account 请求端下逐条点击“同步此会话”。手工 full backup、hard delete 和完整恢复 safety backup 仍是持久恢复点，不参与自动清理。
- v0.2.0 每次 changed switch 会永久保留 current `Runtime` + shared `Sessions` 全会话 checkpoint，已确认这是 C 盘按次下降的直接原因；当前请求路由和手机连续性发布都不创建全会话 checkpoint。普通切换不会生成 updater EXE。
- 窗口关闭监听在应用挂载时预注册，未注册成功前切换按钮保持禁用；关闭按钮始终走后端安全退出。手机连续性正在发布时会明确询问“继续等待 / 仍然退出”；确认退出后等待当前原子步骤、持久化队列并真正退出，下次切回 Account 继续处理。
- 命令完成后按钮会解除 busy；自动增量或手动完全同步改变会话后，会话页按 revision 刷新，备份只在用户点击加载时校验。已有备份扫描尚未结束时收到的新刷新请求会排队补跑，不复用 mutation 前的旧快照。
- 自动识别 Codex 的 `sqlite_home` / `CODEX_SQLITE_HOME`，避免把会话库固定写死在 `%USERPROFILE%\.codex`。
- 手动完全同步继续使用 JSONL-first、完整度选择、source 稳定复检、SQLite transaction/quick-check 与 immutable/no-clobber 发布；既有文件不原地覆盖，必要时发布 Remote-compatible successor。current→shared 与 shared→current 都在 ChatGPT 关闭后执行，最终 current 活跃会话统一为 `openai` provider，供切回官方请求端后手机 Remote 正常识别。
- 在“会话管理”页合并展示当前 Codex Home 与 shared-sessions，会话来源标记为“本机 / 共享池 / 两边都有”。
- 已归档会话默认不参与同步，只跳过不自动清理；可手动恢复可见或安全硬删除。
- 所有会话硬删除都使用页面内确认；删除和恢复可见前由后端关闭并复检 ChatGPT，防止运行中的应用覆盖结果。
- 可手工创建覆盖四个受管 SQLite、`sessions/`、`archived_sessions/` 与索引的 full backup，并查看最近已验证、可完整恢复的备份；runtime / sessions / state-only 局部快照不出现在 UI 恢复列表。恢复前还会创建 full safety snapshot。备份区显示目录总占用、严格可回收空间和安全保留数量；经强校验的 v2 legacy Full、v3 Full 与 v4 Full 恢复点可通过页面内二次确认单独删除，操作会重新强校验并写入审计记录。
- 每次启动在后台检查本仓库最新正式 Release；有新版时显示可关闭的非阻塞横幅，点击一次即可自动下载、校验、替换并重启。v0.2.1 的下载连接超时为 30 秒、总下载超时为 10 分钟；helper 超时会先结束并等待子进程，再重试受控清理。更新 staging 使用系统随机目录、受限 Windows DACL 和持有目录句柄，替换过程持久化阶段 journal；新进程必须进入 Tauri Ready 并回传受控 ACK，在此之前失败、早退、超时或中断恢复都会尝试恢复旧 EXE。
- 槽位中的敏感信息使用 DPAPI 加密，界面、日志和回执不回显。用户选择 Relay 时，Codex 当前实现要求 bearer token 以明文存在 live `config.toml` 的受管 provider 表中；切回账号态会原子清除该表，`auth.json` 始终保持官方登录态不变。

## 下载与运行

1. 打开 GitHub Releases。
2. 下载 Windows 版 `codex-switch.exe`。
3. 双击运行。

当前版本面向 Windows + ChatGPT Desktop / Codex CLI 用户。

### 自动更新

ChatGPT Switch 每次启动只在当前进程内检查一次更新，不安装后台服务、不设置自启动项，也不会在运行期间周期轮询。检查源固定为本仓库最新正式 GitHub Release，draft 和 prerelease 不参与比较；网络失败不会阻塞其他功能。发现新版后点击“立即更新”，应用会下载固定的 Windows x64 EXE、核对 GitHub 资产大小和 SHA-256，在受限 staging 中准备 helper、plan 和持久化 journal，退出当前进程后替换原 EXE，再自动启动新版本；启动失败或检测到未完成阶段时会验证并恢复旧版本。v0.2.1 下载器使用 30 秒连接超时和 10 分钟总超时，helper readiness 超时会执行 kill + wait，staging 清理使用有界重试。正常 helper 的显式完成/回滚启动仍会闭合恢复；管理员权限进程若在 helper 之外无参数重启，不会扫描 `%TEMP%` 自动续跑，避免接受低完整性进程预造的恢复计划。

> v0.1.6 本身尚未包含 updater，因此从 v0.1.6 升级到 v0.1.7 需要最后一次手动替换；从 v0.1.7 开始，后续版本可在应用内一键更新。已发布 v0.2.0 内置的首跳下载器仍是 120 秒总超时，10 分钟合同只有升级到 v0.2.1 后才生效。本次已用正式 v0.2.0（SHA-256 `42012…A65A`）在隔离环境完成真实 UI Automation 首跳：约 `2.086s` 出现并点击“立即更新”，旧进程约 `89.103s` 后以 exit code `0` 退出，`105.1s` 内自动替换并重启 ChatGPT Switch；目标文件为 `2,214,400` bytes、SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`、版本 `0.2.1`，staging/install leftovers 均为 `0`，没有手工替换。

当前仍只构建和发布 Windows x64 `codex-switch.exe`。更新发现和版本比较保持平台中立，EXE 替换收敛在 Windows 平台边界，为未来 macOS/Linux 的独立安装策略留出接口；这不代表当前版本已经支持非 Windows 平台。

`v0.2.1` 发布链使用体积优先的 Cargo release profile（`opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"`、strip symbols）构建 raw EXE，再对副本使用固定官方 UPX 5.2.0 执行 `--best --lzma`。raw 与 packed 文件都通过 release contract，packed 文件还通过 `upx -t`、PE32+ x64、`ProductVersion` / `FileVersion` 和 3,000,000 bytes 硬门禁；CI 只上传 packed 的裸 `codex-switch.exe`。本地临时候选 raw `5,955,584` bytes → packed `2,228,224` bytes（SHA-256 `4DDC…CED7A`）仅是历史候选，不是正式资产。

[PR #5](https://github.com/mingisrookie/codex-switch/pull/5) 已从工作提交 `702dc37` 合并为 `3b4f440`；PR CI `30194264349` / `30194276794`、main CI `30194772843` 与 tag CI `30195207004` 均通过。annotated tag `v0.2.1` 指向 `3b4f440`；[v0.2.1 Release](https://github.com/mingisrookie/codex-switch/releases/tag/v0.2.1) 是 latest stable、非 draft，唯一资产 `codex-switch.exe` 为 `2,214,400` bytes，SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`，PE32+ x64、`FileVersion/ProductVersion = 0.2.1` 且 `upx -t` 通过。Release 回下载与 tag-CI artifact 的 hash/bytes 完全一致，两个 ignored live GitHub 合同测试各 `1 passed`，上述真实 v0.2.0 首跳也已完成。

## 快速使用

### 1. 保存当前 ChatGPT 账号态

先确保你当前的 ChatGPT 账号能正常使用，然后点击：

```text
保存当前账号态
```

工具会先确认当前 `auth.json` 是 ChatGPT 官方账号登录态，再把账号配置和认证快照用 Windows DPAPI 加密保存。账号槽位已存在时会展开页面内确认；旧版本会归档到工具自己的历史目录。运行态切换只使用账号配置，不会把该认证快照写回 live `auth.json`。

### 2. 配置 API 中转站

点击：

```text
配置 API 中转站
```

依次填写：

- Base URL：例如 `https://your-relay.example.com/v1`
- 模型名：例如你的中转站支持的模型名
- API Key：只会加密保存，不会显示在界面上

说明：

- 如果 Base URL 没写 `http://` 或 `https://`，工具默认按 `https://` 处理；路径未以 `/v1` 结尾时会补上 `/v1`，且不接受内嵌用户名/密码、query 或 fragment。
- 非 loopback 的明文 `http://` 地址会被拒绝；HTTP 只允许 localhost 或回环地址。
- 首次保存必须输入 API Key；以后只在 URL origin 不变时允许 Key 留空并保留已加密值，改变 scheme/host/port 必须输入新 Key。
- “验证连接”会用 Bearer 认证请求 `<Base URL>/models`，10 秒超时且禁止重定向；只以 2xx 判断地址、网络和鉴权可用，不读取或判断模型列表，错误不会回显 Key 或响应正文。
- 槽位存储中的 Key 始终是 DPAPI 密文；切换到 Relay 时，工具会在 live `config.toml` 的 `[model_providers.openai_custom]` 中投影 `experimental_bearer_token`，并权威写入 `supports_websockets = true`。这是当前请求端路由所需的运行态；切回账号态会删除整个受管 provider 表。工具不会改写 `auth.json`。

### 3. 切换到中转站

点击：

```text
切换到中转站
```

点击后页面立即进入任务执行器，不会再弹系统确认框。后端原始 phase/timestamp 全部保留，界面聚合为最多 7 个可见步骤，显示当前步骤耗时、每个已完成步骤耗时和终态最慢步骤；这些时间来自真实后端事件，不是伪造百分比。后端依次执行：

1. 读取目标槽位并校验 live `auth.json` 是官方 `chatgpt` 登录态；缺失、损坏或非官方模式都在任何进程/配置 mutation 前 fail closed。Relay 首次切换或地址/凭据变化后由用户选择“验证连接后切换”或“直接切换”；直接切换不会发起 Relay 网络请求。
2. 从受管 ChatGPT 根进程捕获并校验唯一 Windows AppUserModelID，再用 ToolHelp 检测/关闭进程树。先温和关闭并等待，超时才强制结束仍能证明身份的受管根；独立 `codex.exe` CLI 永远不会被结束。
3. ChatGPT 关闭后重新读取官方登录态与 live 配置，生成只涉及模型/service tier/受管 provider/受管 `sqlite_home` 的最小 patch；同时仅检查并按既有规则修复 `process_manager/chat_processes.json` 的明确空/全 NUL 损坏。
4. 切入 Relay 时先从 Account 四库生成隔离的 Relay SQLite 会话视图；切回 Account 时先在 30 秒预算内发布新 Relay 会话，若队列未收口则保持 Relay。该步骤不全量扫描/复制 JSONL、不创建会话 checkpoint，也不做 provider slot GC。
5. 原子替换 live `config.toml`。Relay 路由注入 `experimental_bearer_token`、`requires_openai_auth = true`、`supports_websockets = true` 和受管 `sqlite_home`；账号路由移除整个 `model_providers.openai_custom` 受管表并恢复原 SQLite home。
6. 重新读取 `auth.json` 并与关闭后快照逐字节比较，再精确验证目标请求路由和 SQLite home。官方 token 可在 preflight 与关闭之间由 ChatGPT 正常刷新，但 switcher 自己绝不写入认证文件。
7. 若配置写入后的验证或元数据记录失败，只恢复原始 `config.toml` 并再次证明 `auth.json` 未变；随后持久化脱敏终态，通过关闭前捕获的 AppUserModelID 受控打开 ChatGPT。启动失败不会撤销成功路由，并在同一 task overlay 提供重试。

如果 UI 只显示“模式匹配”而不是“当前运行”，按钮会显示“重新应用”；只有官方 `chatgpt` 登录模式和模型/provider/Relay 路由字段（包括 bearer token）都精确匹配才会跳过重复写入。官方 token 自身的正常刷新不影响账号态 exact 判定。

完成后 ChatGPT Desktop 会自动重新打开并使用中转站 API；如果 Windows 激活失败，可在同一 task overlay 里重试。Codex CLI 不会由工具自动启动。

### 4. 切回 ChatGPT 账号态

点击：

```text
切换到 ChatGPT 账号
```

流程同样先校验官方登录态并安全关闭 ChatGPT。配置写入前先把 Relay 期间新增会话发布为 Account/手机 Remote 兼容的 `openai` successor；30 秒预算内仍有待处理项就保留 Relay 并提示完全同步，而不是先切回再隐藏会话。随后应用账号态 overlay，删除受管 Relay provider 表与明文 token，并恢复切入 Relay 前的 Account SQLite home。保存的账号 overlay 若没有 `model` / `service_tier`，会主动删除 live 中遗留的 Relay-only 值。全程不会恢复或改写保存的账号态 `auth.json`。

### 5. 手动完全同步

点击：

```text
完全同步
```

这个操作只同步会话，不切换请求端。确认后由后端关闭并复核 ChatGPT，不再先做一次前端 dry-run；随后创建 current/shared 两份 `StateOnly` 临时检查点，对两边全部**活跃会话**做一次完整对账，再建立下一次切换可复用的增量索引并受控重新打开 ChatGPT。手动完全同步的耗时单独计算，不叠加到普通切换按钮；数据量大时仍可能需要几十秒到数分钟。

同步策略是 **JSONL-first**：

- 以 `sessions/**/*.jsonl` 中的 `session_meta.payload.id` 作为可靠会话来源。
- 只合并存在正文 JSONL 的会话；只有 SQLite 行但找不到 JSONL 正文的孤儿记录会跳过，避免把不可打开的空会话同步出去。
- current → shared 与 shared → current 都在 ChatGPT 关闭态完整对账；最终 current 的活跃会话使用手机 Remote 兼容的 `openai` provider。
- 修复重复会话的缺失 JSONL / 错误 `rollout_path`；同一会话 ID 存在多份 JSONL 时优先使用 SQLite 当前指向的活动文件，并只沿严格前缀关系选择更完整的独立版本。
- 每次真正写 JSONL 前重新计算 live source/target relation，不执行已经过时的 `Create/Import` 判断。source 快照必须通过完整尾行、session ID、长度与 SHA-256 的前后稳定性校验；发现漂移时只做有界重试，无法冻结则 fail closed。判定为 `Unchanged` 时在返回前还会复检 source version 与 live relation，`Deny` 遇到任何晚到变化都零写入并 fail closed。
- JSONL 生产路径不修改任何已有文件。Create 与 imported 文件先在同目录临时文件中完成 provider 元数据归一并同步，再用 atomic hard-link no-clobber 发布；若目标抢先出现则重新比较，不能覆盖对方。Remote provider 槽位在 32 个有界候选中先复用完整文件；首次或增长时只发布新的最终 provider successor 及严格 provenance marker，不生成 raw/provider 双份，真正 Divergent 的 raw 分支保留是避免数据丢失的明确例外。marker 的 `createdBytes` 前缀 SHA-256 允许创建后合法 append，但前缀、marker、session ID、provider 或 containment 任一漂移都会失去工具所有权并 fail closed。
- 已归档 SQLite 行和 `archived_sessions/` 正文完全跳过：不复制、不恢复可见、不从任一侧清理。

Apply 失败会使用两份 `StateOnly` 检查点恢复 current/shared 数据库；immutable no-clobber JSONL 新增可能完整保留，供下一次完全同步重新对账，不会产生半截文件。只有终态日志和检查点强校验都通过，自动点才会释放；终态日志无法持久化或回滚失败会返回 `blocked` 启动态、保持 ChatGPT 关闭并保留检查点，界面不会提供直接重启。会话同步解决的是 current/shared 的逻辑数据合并，不是持久备份，因此不能替代用户主动创建的 full backup。

### 6. 会话管理

顶部切到：

```text
会话管理
```

这里会合并展示：

- 当前 Codex Home
- `%APPDATA%\codex-switch\shared-sessions`

同一个会话 ID 两边都有时，以当前 Codex Home 为准，shared-sessions 只补缺。你可以：

- 按全部 / 未归档 / 已归档 / 本机 / 共享池筛选，并可搜索、排序；列表每页 50 条。
- 用表格左上角“全选本页”复选框，或工具条的“全选本页 / 反选本页 / 清空选择”控制当前页；跨页选择会保留，当前页部分选中时显示 indeterminate 状态。列表只展示一行会话标题，过长自动省略。
- 选择会话后点击“恢复可见”：只更新当前 Codex Home 的归档状态，下次同步再正常参与。
- 选择会话后点击“删除所选”：页面内确认后，后端会关闭并复检 ChatGPT，创建覆盖 active/archived JSONL、索引和四个受管 SQLite 的 Full 备份，再从当前 Codex Home 和 shared-sessions 的 `sessions/` 与 `archived_sessions/` 同步硬删除。

删除规则：

- 已归档和未归档会话：都必须确认后才能硬删除。
- 一次硬删除超过 10 个会话时，还必须输入“删除 N”完成高风险确认。
- 不提供单独“排除同步”按钮；同步排除只使用 Codex 原生归档状态。

### 7. 技能安装与配置

顶部切到：

```text
技能
```

页面固定提供两项能力：

- **Image2**：安装目录为当前 `CODEX_HOME\skills\newapi-image2-client`，来源锁定为 `https://lcming951.com/image2-skill.zip`；当前审查基线 SHA-256 为 `648C192C2414BBFD9DBA36E264C01932BDCF7E2057A8BA2DA7006B40A94B332B`。默认 URL 是 `https://api.lcming951.com/v1`，模型固定为 `gpt-image-2`。
- **Grok 搜索**：安装目录为当前 `CODEX_HOME\skills\grok-search`，提供 Web/X 搜索脚本，模型固定为 `grok-4.5`；服务 URL 由用户填写。

使用顺序：

1. 点击“安装”，后端会关闭并复检 ChatGPT，页面不弹系统确认框。
2. 安装成功后点击“配置”，填写服务 URL 和自己的 API Key。
3. 保存成功后重启 ChatGPT / Codex CLI，使新 Skill 被重新扫描。

安装/更新只接受上述两个固定 Skill ID，不接受前端传入任意源路径或目标路径。包体已内嵌到便携 EXE，运行时不会从当前管理员目录复制，也不会在线下载。已有未知目录或已修改文件不会被静默覆盖；确认覆盖时会先把完整旧目录移动到当前 Codex Home 下的 `.codex-switch\skill-backups`。安装事务在 `.codex-switch\skill-transactions` 留下原子 journal；进程中断后下一次安装会先保留已验证的新版本或恢复旧目录，再继续操作。

非敏感 URL/模型配置和 DPAPI 密文位于 `%APPDATA%\codex-switch\skills\<skill>`。API Key 输入框不会回填；首次必填，后续留空表示保留已经加密保存的 Key。Image2 自带 PowerShell helper，用正确的 Images API `/images/generations` / `/images/edits` 调用 `gpt-image-2`；Grok 脚本用 `/v1/responses` 调用 Web/X 搜索。

### 8. 备份恢复

运行态页可以手工创建 current 与 shared-sessions 两根 full backup。Full 覆盖四个受管 SQLite、`sessions/`、`archived_sessions/` 和 `session_index.jsonl`；操作会先做容量预检，关闭并复检 ChatGPT，拒绝与独立 Codex CLI 并发，然后在页面内显示 loading、操作 ID 和两份已验证快照，不弹系统窗口。

恢复列表只纳入 full、可完整恢复的备份；runtime / sessions / state-only 局部快照只用于原操作补偿，不会出现在 UI 恢复列表。用户主动加载时，后端按时间从新到旧遍历 full 候选，最多返回 256 个通过强校验的恢复点供逐份恢复或删除。列表与删除使用同一套 managed-full 强校验；出现未声明额外文件、payload 大小/hash 漂移、路径或 manifest 异常的目录不会显示为 `verified`，也不会自动删除。手工 full backup、硬删除前快照和恢复覆盖前的 full safety snapshot 是持久恢复点，仍只由用户显式管理。恢复时：

1. 只允许选择 `%APPDATA%\codex-switch\backups` 的直接子目录，并校验 manifest、路径、文件大小和 SHA-256。
2. 按 manifest 的 `sourceRoot` 恢复到当前 Codex Home 或 shared-sessions，不能任意指定目标目录。
3. 恢复前为目标再创建一份覆盖四个受管 SQLite 的 full safety snapshot；恢复失败则尝试恢复安全快照。
4. 恢复使用备份内的 `config.toml` 重新解析 `sqlite_home`，删除快照中不存在的额外 active/archived 会话文件，并按 manifest 的 `trackedDatabases` 执行 `PRAGMA quick_check`；full backup 覆盖四个受管 SQLite。

即使 Codex Home 扫描损坏，只要备份列表域成功加载，已验证备份的恢复入口仍保持可用。运行态页还会独立显示最近 10 条本机操作历史，包括动作、终态、操作 ID、完成时间和关联备份路径；后端读取上限为最近 20 条。

列表命令先在 mutation guard 临界区清理 legacy plaintext auth，随即释放锁；之后读取 manifest 元数据并排除局部 scope，再按 `createdAtMs` 从新到旧调用与显式删除相同的 managed-full 校验，核对直接子目录、manifest、精确文件集合、大小和 payload SHA-256。前端等待列表/迁移与操作记录完成后才调用检查点空间扫描；空间扫描自身也在 blocking worker 中持有 mutation guard，避免与清理或迁移并发。无效候选跳过且留在磁盘，不会伪装为 verified；按需最多返回 256 个有效 full backup。删除与真正恢复都会再次完整校验，不能拿列表时的结果代替 mutation 前检查。

备份区会只读统计 `%APPDATA%\codex-switch\backups` 的目录数量、总字节数、可证明回收的检查点和必须保留的项目。本轮操作仅在终态**成功写入操作日志后**自动处理：自动 checkpoint 必须是带非空 `operationId` 和精确 `role` 的 bound BackupManifest v4，并与唯一 terminal record 的 ID、角色、action/status/phase、目录、reason/scope、时间窗和完整 payload hash 全部匹配。未绑定 v2/v3 自动点一律 fail closed 保留；恢复可见只允许绑定 v4 单根 `StateOnly`。cleanup 的 Summary、Receipt 和新操作记录都包含 `attemptedCount` / `failedCount`：只有已进入本轮计划后在执行期 revalidate 或 remove 失败的目录才计入 `failedCount` 并显示部分完成/Failed；Full、孤儿、unclassified 等安全保留项只通过 `retainedCount` 与 warning 说明，不会把成功清理误判为失败。

当前版本**不按年龄、目录数量、mtime 或空间阈值猜测清理**。Apply/回滚/日志失败、孤儿目录、损坏或无法分类的 manifest、缺少终态证据的旧快照全部保留；显式 cleanup 在 plan 与 execute 两阶段都会重新解析严格操作记录，并对完整 manifest、受管路径、文件集合、大小与完整 payload SHA-256 做强复检，计划后任何漂移都 fail closed。只有执行期复检或删除真正失败时，回执才会出现 `failedCount > 0`，页面显示部分完成且操作历史记为 Failed；纯保留 warning 仍是“清理完成（有保留说明）”。持久 Full 不参与自动释放，只能由用户逐个确认删除；创建备份时若 partial 目录清理也失败，错误会同时报告而不再吞掉残留。

2026-07-26 的受控页面清理已把真实备份根从 `21` 个目录、`6,327,089,609` bytes 降到 `17` 个目录、`2,693,977,957` bytes：计划中的 `4/4` 个检查点均删除，回收 `3,633,111,652` bytes，紧邻操作的 C 盘空闲空间测得增加 `3,637,547,008` bytes。首次执行使用的是修复前候选 UI，它把安全保留 warning 误标为“部分完成”，并留下历史 Failed 日志；该历史不改写。现有 `attemptedCount` / `failedCount` 语义和前后端回归已修复此问题；发布闭环后的最终 cleanup 为 `0 attempted / 0 failed / Succeeded / Complete`，剩余 `17` 项继续保留且不会自动删除。

### 9. 诊断与支持

`v0.2.7` 源码新增独立的支持诊断层。它和 `%APPDATA%\codex-switch\logs\operations.jsonl` 是两套不同合同：

- `%APPDATA%\codex-switch\logs\diagnostics\events-*.jsonl` 保存已经结构化、限长并脱敏的诊断事件。Windows 上，同一登录会话内、使用同一规范化诊断根的 append/read/status/prune/clear 通过 root-scoped named mutex 跨进程协调，但不取得业务 mutation guard；command/lifecycle recorder 和 panic 路径拿不到锁都会立即放弃，低层管理 API 才使用有界等待。读取时每个 segment 只容忍最后一条未写完整的尾记录，发现 dirty tail 后不会继续追加旧段；内部损坏或不兼容 schema 会明确失败。诊断写入、轮转或清理仍是 best-effort，不会阻止启动、改变 mutation 终态或触发业务回滚；事件已经 `sync_data` 成功后，后置 prune 失败也不会把这次 durable append 误报成失败。
- `logs\operations.jsonl` 仍是严格、durable 的终态审计账本和检查点清理证据。诊断导出只读它并生成去掉备份路径等敏感字段的相关子集；诊断面板的轮转、导出和“清除诊断日志”都不会删除、截断或改写该账本。

使用方式：

1. 点击顶部工具栏的“诊断”，可以查看诊断是否可用、事件数量、占用和保留上限，并执行“导出最近诊断”“打开日志目录”或页面内确认的“清除诊断日志”。清除只处理受管诊断事件，不处理操作历史、备份、会话、配置、凭据、已经导出的 ZIP 或导出中断遗留的同目录 staging 文件。
2. 请求端切换、同步、备份/恢复、Skill、更新等用户主动 mutation 都进入统一诊断生命周期。后端在 Tauri rejection 中用固定、严格校验的错误信封携带真实 durable operation ID，尚未绑定时携带本次 attempt ID；前端统一解包但兼容既有裸字符串错误，不会猜测“最近一次操作”。失败面只有拿到真实关联 ID 时才显示“导出本次诊断”，并选择该 attempt 的完整事件及操作开始前 10 分钟上下文；没有 ID 时只提供“导出最近诊断”。
3. 默认导出使用 Windows Known Folder API 解析真实“下载”目录，不假定 `%USERPROFILE%\Downloads`。文件名为 `ChatGPT-Switch-Diagnostics-<本地时间>.zip`；同名时追加数字后缀，绝不覆盖旧包。ZIP 先在受控内存中完成固定五文件、逐项 hash、脱敏扫描和自检，再在目标目录写入同目录 staging，通过 Windows write-through、no-clobber 原子发布。只有目标解析/发布失败才返回有界、短期有效的 opaque retry ID；“重试下载目录”和用户主动点击的“改存应用诊断目录”复用同一份 prepared bytes/hash/selection，不重新采集日志。后者固定写入 `%APPDATA%\codex-switch\diagnostic-exports`，不会静默 fallback，也不接受任意文件路径；采集/准备失败不会伪装成下载目录失败。

诊断 ZIP 固定只含 5 个文件：

- `README.txt`：包用途、隐私边界和文件说明。
- `manifest.json`：schema/应用与构建版本、导出时间和时区偏移、平台/架构、选择窗口、脱敏策略版本、每个负载文件的 bytes/SHA-256，以及明确的 unavailable/warning；`timestampUnit` 固定为 `unixEpochMilliseconds`，说明事件 `timestamp` 是 Unix epoch 毫秒。manifest 不递归记录自身 hash。
- `diagnostics.jsonl`：已按 operation/attempt、action、真实 phase、typed terminal/error code 关联的脱敏诊断事件。
- `operations.jsonl`：相关 durable 操作终态的脱敏子集，不包含原账本中的备份路径。
- `health.json`：只读健康摘要；包含应用版本、OS/架构与 best-effort Windows 版本、APPDATA/CODEX_HOME 和诊断存储摘要、当前 route/auth mode 结构、受管 ChatGPT/独立 Codex 进程计数，以及四个受管 SQLite 的 present/readable/bytes/只读 `schema_version`。任一 collector 失败时逐项标记 unavailable，不伪造成健康或空数据；它不读取进程命令行，也不执行写入式修复。

事件在落盘前先经过字段 allowlist、深度/数量/长度限制和集中 sanitizer；导出层还会执行更严格的禁用字段删除、身份/路径/秘密形态扫描和 ZIP 自检。自由文本中的业务 session/thread UUID 会固定脱敏，诊断自身随机 session/event/attempt/operation 关联字段仍保留。默认包不包含 API Key、token、Authorization、cookie、凭据、聊天/会话正文、原始 session JSONL、SQLite、`auth.json`、完整 `config.toml`、请求/响应正文、全量环境变量、进程命令行、Windows WER、机器名、Windows 用户名或稳定设备 ID。已知受管根只在完整路径边界上映射为 `%CODEX_HOME%` / `%APPDATA%` / `%USERPROFILE%`；盘符、反斜杠/正斜杠 UNC、设备路径和其他未知绝对路径都不会原样导出，带 scheme 的 URL 仍按 URL 规则处理。即使已脱敏，也建议只把生成的单个 ZIP 私下发给维护者，不要上传整个 `%APPDATA%\codex-switch`。

本地诊断 store 默认只读取/导出 `timestamp` 位于最近 **14 天**事件窗口内的记录，同时把同一诊断根的 segment 总量限制为 **10 MiB**；单个 segment 达到 512 KiB、创建时间超出窗口或尾部不完整时轮转。年龄清理只有在逐条验证一个 clean segment 的全部事件都已过期后才删除整段，不能因文件名较旧而误删窗口内的新事件；容量上限仍可淘汰结构有效、尾部完整的最旧段，因此实际可用窗口可能短于 14 天。dirty tail、内部损坏或未知 schema 不会被 prune 掩盖。事件读取仍按 session causal sequence 展示，但状态和 retained-window manifest 使用事件 `timestamp` 的真实最小/最大值，操作子集也按规范化时间区间求交，避免系统墙钟回拨漏掉相关记录。窗口依赖 Windows 系统墙钟，人为大幅回拨仍会影响 14 天判断；不同登录会话或指向同目录的不同路径别名也不保证共享同一个 named mutex。

应用会尽早记录随机、仅本次启动有效的 `sessionStarted`；Windows session/attempt ID 优先来自 OS CSPRNG，失败时依次使用 `CoCreateGuid` 和不可稳定关联的进程局部 fallback。`sessionStarted` 带 PID、timestamp unit，并在系统允许时尽力带 process creation stamp，Tauri Ready/正常退出记录 `appReady` / `sessionEnded`；下次启动在 PID + creation stamp 可用时先排除仍在运行的另一个实例和 PID reuse，再对缺少 clean terminal 的上一 session 记录 `previousSessionUnclean`。Rust panic hook 和前端 `error` / `unhandledrejection` 只写最小、限长、脱敏的 best-effort 事件，release 的 `panic = "abort"` 保持不变：它不恢复 panic、不增加 watchdog，也不承诺捕获来不及执行落盘代码的访问冲突、强制结束、断电、硬卡死或其他硬崩溃。硬崩溃若发生在 ZIP 最终发布前，还可能在 Downloads 或固定备用目录留下 `.chatgpt-switch-diagnostics.<pid>.<sequence>.tmp`；正常错误会尽力删除，但“清除诊断日志”不会处理该文件。`previousSessionUnclean` 只能证明缺少干净终态，不能单独证明具体崩溃原因。

## 文件位置

ChatGPT Switch 默认操作当前用户的 Codex Home。解析顺序：

1. 如果设置了 `CODEX_HOME`，优先使用它。
2. 否则使用当前 Windows 用户目录下的 `.codex`。

```text
C:\Users\<你>\.codex
```

工具自身数据保存在：

```text
%APPDATA%\codex-switch
```

主要包含：

- 加密后的运行态
- 切换/同步的临时回滚检查点，以及删除/恢复/手工创建的持久加密备份
- 共享会话池
- `mobile-continuity-v1.json` 手机连续性 cutover 与持久队列；只保存 thread ID、source fingerprint、typed 状态、重试元数据和脱敏失败分类
- `session-sync-state-v1.json` 仅供既有手动完全同步维护 current/shared 对账基线，不再进入普通请求端切换
- 脱敏操作记录
- 最近 14 天事件窗口 / 10 MiB 总量上限的脱敏诊断事件；默认导出的诊断 ZIP 位于 Windows Known Folder 解析的“下载”目录，不保存在 APPDATA 日志根
- 用户在下载目录导出失败后主动选择的备用诊断 ZIP：`%APPDATA%\codex-switch\diagnostic-exports`；不属于 diagnostics event retention，也不会被“清除诊断日志”删除
- Image2 / Grok 的非敏感配置与 DPAPI 加密凭据

Codex 会话存储说明：

- 官方会话索引默认是 `state_5.sqlite`，辅助 thread 数据还可能位于 `goals_1.sqlite`、`memories_1.sqlite` 和 `logs_2.sqlite`；这些库可能被 `config.toml` 的 `sqlite_home` 或环境变量 `CODEX_SQLITE_HOME` 一起改到别的位置。
- 活跃会话正文位于 Codex home 下的 `sessions/**/*.jsonl`，原生归档正文可能位于 `archived_sessions/**/*.jsonl`；自动增量与手动完全同步只处理活跃 `sessions/`，Full/Sessions 备份与硬删除覆盖两者。
- `session_index.jsonl` 是 Codex 自身会话索引文件；手机连续性状态是独立的 `%APPDATA%\codex-switch\mobile-continuity-v1.json`，两者不能混用。
- `sqlite/codex-dev.db` 不是当前同步算法依赖的会话来源。

会话管理删除会同时处理当前 Codex Home 和 shared-sessions 的 active/archived 会话；恢复可见只处理当前 Codex Home。

工具自身关键目录：

```text
%APPDATA%\codex-switch\runtimes\plus
%APPDATA%\codex-switch\runtimes\relay
%APPDATA%\codex-switch\shared-sessions
%APPDATA%\codex-switch\backups
%APPDATA%\codex-switch\logs\operations.jsonl
%APPDATA%\codex-switch\logs\diagnostics
%APPDATA%\codex-switch\diagnostic-exports  # 仅在用户主动改存时创建/使用
%APPDATA%\codex-switch\skills\image2
%APPDATA%\codex-switch\skills\grok-search
```

受管 Skill 安装到当前 Codex Home：

```text
%CODEX_HOME%\skills\newapi-image2-client
%CODEX_HOME%\skills\grok-search
%CODEX_HOME%\.codex-switch\skill-backups
```

其中 `plus` 只是 ChatGPT 账号槽位的内部兼容 ID，不代表套餐。

## 安全说明

- 不要把自己的 `auth.json`、API Key、备份目录或 `%APPDATA%\codex-switch` 上传给别人。
- 本工具不会在 UI 中展示真实 Token 或 API Key。
- Image2 / Grok Key 只以当前 Windows 用户可解密的 DPAPI 密文保存；前端只拿到“已配置/未配置”，安装包、Skill 文件、操作记录和回执不包含 Key。
- Skill 安装只处理两个编译期固定 allowlist；目标路径由后端从绝对 `CODEX_HOME` 推导，拒绝符号链接、junction/reparse point 和未确认的本地漂移。
- 当前 writer 生成 BackupManifest v4：继承 `scope` 与 `trackedDatabases`，并为会创建自动点的操作绑定 `operationId + role`；请求路由和手机连续性发布都不创建全会话 checkpoint，手动完全同步继续使用双 `StateOnly`。Full/Sessions scope 覆盖 `sessions/` 与 `archived_sessions/`，runtime/state scope 不扩大。hard delete、手工 full backup 和 restore safety 覆盖四个受管 SQLite。所有 payload 使用当前 Windows 用户的 DPAPI 加密并记录大小/SHA-256；经强校验的 v2/v3/v4 Full 仍可显式恢复。
- 会话文件 `Allow` 路径保持零 in-place：Create 原子 no-clobber；普通 import 的严格扩展与分叉来源使用完整文件发布，旧文件 bytes/hash/mtime 不变。closed/current→shared 先用 `SelectMostComplete` 选活动历史，随后才为目标 provider 在 32 个受管候选中复用完整槽位或发布 immutable successor；marker 不超过 16 KiB，并用 `createdBytes` 前缀 SHA-256 证明创建后 append 没有篡改创建前缀。首次/增长只写最终 provider 文件，旧槽位不覆盖；durable terminal 后只有 successor 完整包含 predecessor 且 current/shared SQLite 都不再引用时才回收，无法证明即保留。hot shared→current 的 existing + `PreserveExisting` 必须在 target path 查询/复制前直接跳过，保持零 target candidate I/O、零 orphan 和 `copied_session_files = 0`。live current index 使用 `Skip`，关闭态或工具独占的 shared index 用完整 merged bytes 原子替换；`Deny` 零写入，`Unchanged` 返回前复检。
- 自动释放只接受与唯一 durable terminal 精确绑定的 v4 自动点，并要求 operation ID、role、action/status/phase、时间窗、reason/source root/scope、路径和删除前 payload hash 全部通过复检。未绑定 v2/v3 自动点、Full、Apply/回滚/日志失败、孤儿和证据不足目录保留，不按年龄/数量/mtime 推测。经强校验的 v2 legacy Full、v3 Full、v4 Full 只能通过页面内显式确认删除。
- 所有受管 SQLite 备份使用 SQLite Online Backup API，不直接复制可能不一致的 WAL/SHM 文件。
- 所有会创建安全快照的操作都在第一份新快照前检查受管根互不重叠和容量。请求端切换不创建安全快照：它在关闭 ChatGPT 后重新冻结 `auth.json` 与 `config.toml`，按目标准备隔离 SQLite 会话视图或有界发布队列，再原子替换请求配置，并在写后逐字节验证官方登录态。
- 关闭态 mutation 会在入口和最后写入前复检受管 ChatGPT 与独立 `codex.exe` CLI；只关闭受管 ChatGPT 进程树，独立 CLI 只作为 fail-closed 阻断信号。自动重启只使用关闭前从受管根捕获并在启动后重新验证的 AppUserModelID，不按 PATH 或任意 EXE 路径执行。
- 需要整文件替换的配置、备份、可写 index 与 `operations.jsonl` 先生成同目录完整临时文件并同步，再用 Windows `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` 原子替换。`operations.jsonl` 业务语义 append-only，但每次会锁内严格解析旧文件、拒绝损坏历史和重复 operation ID，再原子发布完整新文件；失败时旧日志 byte-exact。会话 JSONL 的 Create/Import/provider successor 使用同目录完整临时文件 + hard-link no-clobber，生产路径绝不覆盖已有 JSONL；任何策略允许的 SQLite 引用切换都只能发生在完整新文件与 provenance marker 发布后。
- 后端用进程内 try-lock + Windows 独占 `%APPDATA%\codex-switch\mutation.lock` 文件句柄串行化保存、验证、切换、同步、删除、恢复、一键更新安装和应用退出；同一进程或第二个 ChatGPT Switch 进程已有写操作时，新操作会立即拒绝。普通退出只有取得同一锁并把它保留到进程终止后才执行；繁忙时前端排队重试。更新进入退出阶段后锁同样保持到父进程终止，前端同时双向禁用其他 mutation。
- `operations.jsonl` 只保存 action、阶段、终态、操作 ID、备份目录和计数，不保存凭据、请求正文或自由文本错误；它是严格的 durable 终态账本。独立 diagnostics store 只保存先脱敏的支持事件，导出时再次脱敏和扫描；诊断失败不会覆盖、伪造或削弱原 mutation 终态。
- 会话管理里的删除是硬删除；工具会先备份并支持失败补偿，同时清理四个受管 SQLite 中可识别的 thread 关联。操作前仍需在页面内确认选择范围。
- 自更新 staging 名来自 Windows CSPRNG；目录创建时按当前 token 是否 elevated 应用受限 DACL，并持有目录句柄。helper 以持久化阶段 journal、目标/备份 hash 和 Tauri Ready ACK 决定继续、完成或回滚；elevated 无参数启动不会从 `%TEMP%` 自动发现并执行恢复计划。
- 本机 Microsoft Defender 的 Product/Feature 处于 disabled 状态，相关命令无法形成“扫描通过”证据；Release 的 hash、合同、`upx -t` 和真实更新验证不应被误述为 Defender 扫描结果。
- 生产 UI 不使用 emoji、`window.alert`、`window.confirm`、`window.prompt` 或原生 `<dialog>`；配置、确认、Full 删除和失败都在页面内呈现，图标统一使用 Lucide。切换使用唯一 modal/task overlay，真实细分 phase、逐步耗时、退出排队回执和终态留在同一焦点层；关闭监听必须在挂载时预注册，运行态刷新完成前不得允许再次切换。

## 开发

```bash
npm install
npm run tauri -- dev
```

常用检查：

```bash
npm test -- --run
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path src-tauri/Cargo.toml
npm run tauri -- build --no-bundle
npm run tauri -- build
npm run check:release
```

桌面 UI 与原生窗口/切换实测应启动 Tauri CLI 构建的产物；`--no-bundle` 可跳过安装包但仍执行前端生产构建。裸 `cargo build --release` 只验证 Rust 编译，不能替代生产 custom protocol 下的桌面 UI 验证。

最终 Release 打包使用显式 UPX 路径，不会从 PATH 任取工具或原地修改 raw EXE。脚本固定验证官方 UPX 5.2.0 的 `upx.exe` SHA-256 `F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D`；CI 下载的官方 ZIP 还固定为 `B471EBF1B7F20F4A89150264ED9A008A2A5BFD247F3C6D1184A75BB59CA08F5D`：

```powershell
.\scripts\pack-windows-release.ps1 -UpxPath "C:\path\to\upx.exe"
```

`v0.2.7` 还必须在隔离 `APPDATA` / `CODEX_HOME` 下完成诊断写入、跨进程 root lock、dirty tail 封存、事件时间 14 天/10 MiB segment 轮转、operation 分离、health 只读/逐项 unavailable、ZIP 五文件/hash/敏感扫描、Known Folder/显式备用目录/no-clobber、前端失败导出和真实 packed EXE 导出验证；随后才进入 PR/main/tag CI、GitHub Release/Latest、公开回下载一致性与正式 `v0.2.5 -> v0.2.7` updater smoke。上述发布证据当前待主任务最终闭环，不得从本 README 的目标说明推断已经完成。

## License

MIT
