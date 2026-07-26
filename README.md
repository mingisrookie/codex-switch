<div align="center">

# ChatGPT Switch

**把固定的 ChatGPT 账号态与 API 中转站态做成可验证、可回滚、可同步的本地运行态工作台。**

保存当前账号登录态；配置一个 OpenAI-compatible API 中转站；安装并配置 Image2 / Grok 搜索 Skill；切换与普通同步只创建覆盖 config/SQLite 写集的窄临时检查点，不再为每次操作复制 GiB 级会话正文；需要写入的会话 JSONL 走零 in-place、完整文件原子 no-clobber 发布，hot shared→current 的既有 thread 则在文件处理前直接跳过，索引按运行态跳过或整文件原子替换；已终结临时点自动释放，完整恢复点由用户在页面内管理；并提供真实任务进度、dry-run、操作回执、操作历史、备份恢复和独立会话管理页。

[快速使用](#快速使用) · [下载 Release](https://github.com/mingisrookie/codex-switch/releases/latest) · [更新日志](CHANGELOG.md) · [安全说明](#安全说明) · [开发](#开发)

![release](https://img.shields.io/badge/release-v0.2.1-087f75)
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

## 开发过程

本项目把 DXM 大项目协作规范也放进仓库，方便外部查看需求澄清、开发边界、链路说明和 PR 流程：

- [AGENTS.md](AGENTS.md)：Codex / AI 协作入口规则。
- [项目开发规范（AI协作）.md](项目开发规范（AI协作）.md)：开发、测试、文档同步和交付标准。
- [项目完整链路说明.md](项目完整链路说明.md)：运行态切换、会话同步和数据流说明。
- [项目文件结构说明.md](项目文件结构说明.md)：文件职责和维护边界。
- [开发者AI开发与PR提交流程.md](开发者AI开发与PR提交流程.md)：GitHub / PR / 发布流程。

## 能做什么

- 固定管理两个槽位：一个 ChatGPT 账号态、一个 API 中转站态；当前版本不承诺任意数量账号池。
- 保存当前 ChatGPT 账号登录态前验证 `auth_mode = chatgpt`；覆盖已有账号槽位使用页面内确认，并保留加密历史版本。
- 配置一个 API 中转站：填写 Base URL、模型名和 API Key；Key 不回填，留空可保留同一 origin 的已保存 Key，并可独立验证连接。保存失败时页面内表单保留本次 Key 方便重试，保存成功或取消后销毁输入值；origin 改变时必须输入新 Key。
- 在独立“技能”页安装固定来源的 `newapi-image2-client` 和 `grok-search`；Skill 按页面懒加载，不会让技能扫描故障污染运行态 Dashboard。
- Image2 与 Grok 分别填写自己的服务 URL 和 API Key；Key 使用当前 Windows 用户的 DPAPI 加密，Skill、非敏感配置、UI、回执和日志中都不保存明文。
- 分开显示“已保存”“当前运行”“最近验证”；只有 `auth.json` 和运行态绑定都精确匹配时才认定为当前态。
- 切换基于 live `config.toml` 应用运行态 overlay，只修改模型/service tier/provider 绑定，不覆盖 `model_instructions_file`、MCP、项目和其他全局设置。
- 点击切换后立即显示页面内任务执行器；在大会话扫描开始前先进入 `planningSessions`，后续真实后端阶段覆盖 relay 验证、ChatGPT 检测/关闭、两根窄状态检查点、双向同步、运行态应用、后置校验、回滚和临时检查点释放，不显示伪造百分比。
- 发生变化的切换固定使用 current `RuntimeState` + shared `StateOnly`，普通同步固定使用双 `StateOnly`；会话文件使用零 in-place 的 `Allow`，只有执行期证明 JSONL/index 完全不变时才冻结为 `Deny`。Create 原子 no-clobber 发布；允许替换时，严格扩展和分叉来源都按稳定 source SHA-256 生成完整 imported JSONL，旧目标的 bytes/hash/mtime 不变。关闭态和 current→shared 使用 `SelectMostComplete`，完整文件发布后才允许 SQLite 切换引用；只有 hot shared→current 使用 `PreserveExisting`：运行中 current 一旦已有该 thread，就在查询既有 `rollout_path` 和复制候选前直接记为 duplicate 并跳过，target candidate 不读取、不写入、不发布，`copied_session_files = 0`。切换失败补偿 current/shared config/SQLite；热同步失败只恢复工具内部 shared SQLite，不用旧状态覆盖可能仍在变化的 current，因此不宣称跨介质 bit-exact 回滚。
- 成功、完整回滚、typed `Failed + Backup` 写入前失败和恢复可见成功，只有在终态日志持久化并再次强校验临时点后才自动释放；Apply 失败、回滚失败、日志失败、损坏或无法证明的目录保留。手工 full backup、hard delete 和完整恢复 safety backup 不自动删除。
- 窗口关闭门禁在应用挂载时预注册，未注册成功前切换按钮保持禁用；切换后会先显示“正在确认当前运行态”，确认完成前不允许再次切换。mutation 错误只出现在一个页面错误面，Relay 验证失败不再重复显示，刷新失败单独表达；未收到可信终态时页面会保守提示先不要重新打开 ChatGPT。
- 命令完成后按钮会解除 busy；会话索引在打开会话页时刷新，备份只在用户点击加载时校验。已有备份扫描尚未结束时收到的新刷新请求会排队补跑，不复用 mutation 前的旧快照。
- 自动识别 Codex 的 `sqlite_home` / `CODEX_SQLITE_HOME`，避免把会话库固定写死在 `%USERPROFILE%\.codex`。
- 单独执行会话热同步，把本地 SQLite 会话索引、`sessions/**/*.jsonl` 正文和 `session_index.jsonl` 合并到共享会话池；source 在执行期校验完整尾行、session ID、长度与 SHA-256 稳定性。current→shared 可选择更完整的已发布文件；shared→live current 一旦发现既有 ID，便在 target path 查询和候选复制前直接跳过，原 SQLite `rollout_path`、provider、title 与旧 writer 可见性保持不变；不会创建 imported candidate/orphan，`copied_session_files = 0`，避免重复热同步增加 C 盘占用和无效文件 I/O。新 ID 仍会在事务中插入，复用 source row 字段（包括存在时的 title），并把 provider 归一为 current provider。live current `session_index.jsonl` 使用 `Skip`；关闭态或工具独占的 shared index 生成完整 merged bytes 并在同目录原子替换。
- 在“会话管理”页合并展示当前 Codex Home 与 shared-sessions，会话来源标记为“本机 / 共享池 / 两边都有”。
- 已归档会话默认不参与同步，只跳过不自动清理；可手动恢复可见或安全硬删除。
- 所有会话硬删除都使用页面内确认；删除和恢复可见前由后端关闭并复检 ChatGPT，防止运行中的应用覆盖结果。
- 可手工创建覆盖四个受管 SQLite、`sessions/`、`archived_sessions/` 与索引的 full backup，并查看最近已验证、可完整恢复的备份；runtime / sessions / state-only 局部快照不出现在 UI 恢复列表。恢复前还会创建 full safety snapshot。备份区显示目录总占用、严格可回收空间和安全保留数量；已验证 Full/legacy v2 恢复点可通过页面内二次确认单独删除，操作会重新强校验并写入审计记录。
- 每次启动在后台检查本仓库最新正式 Release；有新版时显示可关闭的非阻塞横幅，点击一次即可自动下载、校验、替换并重启。v0.2.1 的下载连接超时为 30 秒、总下载超时为 10 分钟；helper 超时会先结束并等待子进程，再重试受控清理。更新 staging 使用系统随机目录、受限 Windows DACL 和持有目录句柄，替换过程持久化阶段 journal；新进程必须进入 Tauri Ready 并回传受控 ACK，在此之前失败、早退、超时或中断恢复都会尝试恢复旧 EXE。
- 敏感信息只加密存储，不在界面、日志、README 或导出内容里展示。

## 下载与运行

1. 打开 GitHub Releases。
2. 下载 Windows 版 `codex-switch.exe`。
3. 双击运行。

当前版本面向 Windows + ChatGPT Desktop / Codex CLI 用户。

### 自动更新

ChatGPT Switch 每次启动只在当前进程内检查一次更新，不安装后台服务、不设置自启动项，也不会在运行期间周期轮询。检查源固定为本仓库最新正式 GitHub Release，draft 和 prerelease 不参与比较；网络失败不会阻塞其他功能。发现新版后点击“立即更新”，应用会下载固定的 Windows x64 EXE、核对 GitHub 资产大小和 SHA-256，在受限 staging 中准备 helper、plan 和持久化 journal，退出当前进程后替换原 EXE，再自动启动新版本；启动失败或检测到未完成阶段时会验证并恢复旧版本。v0.2.1 下载器使用 30 秒连接超时和 10 分钟总超时，helper readiness 超时会执行 kill + wait，staging 清理使用有界重试。正常 helper 的显式完成/回滚启动仍会闭合恢复；管理员权限进程若在 helper 之外无参数重启，不会扫描 `%TEMP%` 自动续跑，避免接受低完整性进程预造的恢复计划。

> v0.1.6 本身尚未包含 updater，因此从 v0.1.6 升级到 v0.1.7 需要最后一次手动替换；从 v0.1.7 开始，后续版本可在应用内一键更新。已发布 v0.2.0 内置的首跳下载器仍是 120 秒总超时，10 分钟合同只有升级到 v0.2.1 后才生效；`v0.2.0 -> v0.2.1` 必须以真实一键更新 smoke 为准，不能只凭新版本源码宣称首跳已修复。

当前仍只构建和发布 Windows x64 `codex-switch.exe`。更新发现和版本比较保持平台中立，EXE 替换收敛在 Windows 平台边界，为未来 macOS/Linux 的独立安装策略留出接口；这不代表当前版本已经支持非 Windows 平台。

`v0.2.1` 候选的发布链先使用体积优先的 Cargo release profile（`opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"`、strip symbols）构建 raw EXE，再对副本使用固定官方 UPX 5.2.0 执行 `--best --lzma`。raw 与 packed 文件都必须通过 release contract，packed 文件还必须通过 `upx -t`、PE32+ x64、`ProductVersion` / `FileVersion` 和 3,000,000 bytes 硬门禁；CI 只上传 packed 的裸 `codex-switch.exe`。本地临时候选为 raw `5,955,584` bytes、packed `2,228,224` bytes（SHA-256 `4DDC…CED7A`），但这不是最终 Release：最终 tag-CI 资产的大小/hash、重下载合同及真实 `v0.2.0 -> v0.2.1` 一键更新仍须在发布闭环中确认。

## 快速使用

### 1. 保存当前 ChatGPT 账号态

先确保你当前的 ChatGPT 账号能正常使用，然后点击：

```text
保存当前账号态
```

工具会先确认当前 `auth.json` 是 ChatGPT 账号登录态，再把认证状态用 Windows DPAPI 加密保存。账号槽位已存在时会展开页面内确认；旧版本会归档到工具自己的历史目录。

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
- “验证连接”会用 Bearer 认证请求 `<Base URL>/models`，10 秒超时且禁止重定向；返回列表必须非空且包含配置的精确 model ID，错误不会回显 Key 或响应正文。
- Codex CLI 当前不接受在 provider 配置里直接写 `api_key` 字段；本工具会把 Key 写入切换后的 `auth.json`，`config.toml` 只保存 provider 连接参数。

### 3. 切换到中转站

点击：

```text
切换到中转站
```

点击后页面立即进入 loading 和任务执行器，不会再弹系统确认框。大会话扫描开始前会先显示“规划会话写集”，后端随后按真实阶段执行：

1. 解析目标认证与 config overlay，检查 current/shared/backup/外置 SQLite roots 不重叠，并按本次 scope 的实际写集、SQLite workspace 和安全余量预检磁盘空间；这些失败都发生在关闭 ChatGPT 前。
2. 中转站目标先验证 `/models`，随后用 Windows ToolHelp 检测 `ChatGPT.exe` 受管进程树；先温和关闭并等待，超时才强制结束仍能证明身份的受管根。独立 `codex.exe` CLI 永远不会被结束，但关闭态操作会提示用户先自行关闭，避免与相同 Home 并发写入。
3. 双向规划会话文件写集并冻结策略。发生变化的切换始终只创建 current `RuntimeState` 与 shared `StateOnly`，不再把 GiB 级 `sessions/` 复制进切换检查点；若执行期完整证明 JSONL/index 无需写入则使用 `Deny`，否则使用零 in-place、完整文件原子发布的 `Allow`。
4. 同步当前会话到共享会话池，原子替换 `auth.json`，并在 live `config.toml` 上只应用运行态绑定字段。
5. 把共享会话写回当前 Codex Home；source 必须在执行期通过完整尾行、session ID、长度和 SHA-256 稳定性复核。Create 用 atomic hard-link no-clobber 发布；允许替换时，严格扩展和 Divergent 都发布按稳定 source hash 命名的完整 imported JSONL，旧短文件不被改写。运行态切换已关闭 ChatGPT，因此使用 `SelectMostComplete`，可在完整文件发布后更新既有 SQLite 引用；普通热同步的 shared→current 才使用 `PreserveExisting`。热同步对 live current index 使用 `Skip`，关闭态或工具独占的 shared index 使用完整 merged bytes 同目录 `atomic_write`。
6. 精确校验目标运行态；失败补偿恢复 config/SQLite，已经完整发布但尚未引用的新 JSONL 可保留供重试，绝不会留下半截文件尾。页面区分写入前失败、已回滚、回滚失败与无法验证终态，不把后者包装成“数据未变化”。
7. 先持久化操作终态，再进入可见的 `cleaningCheckpoints` 阶段：成功、已验证完整回滚或 typed `Failed + Backup` 写入前失败会在强 hash 复检后自动删除本次一份或两份临时点；终态日志写入失败、Apply 失败、回滚失败或无法重新证明完整 manifest/path/payload 的目录保持不动。

如果 UI 只显示“模式匹配”而不是“当前运行”，按钮会显示“重新应用”；只有认证、模型、provider 和 Relay Base URL 都精确匹配才会跳过重复写入。

完成后重新打开 ChatGPT Desktop / Codex CLI，就会使用中转站 API。

### 4. 切回 ChatGPT 账号态

点击：

```text
切换到 ChatGPT 账号
```

流程同样会先创建两根窄状态检查点并同步会话，然后恢复之前保存的账号态 `auth.json`，并在 live `config.toml` 上应用账号态 overlay。

### 5. 会话热同步

点击：

```text
立即同步
```

这个操作只同步会话，不切换登录态；ChatGPT 正在运行时也可以执行。执行前在页面内展示 current ↔ shared 双向 dry-run 的新增/重复数量，用户确认后创建两份 `StateOnly` 临时检查点并同步，不再复制 `sessions/` payload。同步成功，或明确发生在 Backup 阶段、尚未写 live 数据的 typed 失败，在终态记录落盘和强复检后释放相应一份或两份检查点；Apply、回滚、日志或证据异常时保留。同步策略是 **JSONL-first**：

- 以 `sessions/**/*.jsonl` 中的 `session_meta.payload.id` 作为可靠会话来源。
- 只合并存在正文 JSONL 的会话；只有 SQLite 行但找不到 JSONL 正文的孤儿记录会跳过，避免把不可打开的空会话同步出去。
- 合并 `session_index.jsonl`，让不同运行态看到同一批历史会话。
- 修复重复会话的缺失 JSONL / 错误 `rollout_path`；同一会话 ID 存在多份 JSONL 时优先使用 SQLite 当前指向的活动文件，并只沿严格前缀关系选择更完整的独立版本。
- 热同步和运行态切换都不会只为 provider 变化重写已经存在且内容等价的 live JSONL；provider 可更新到 SQLite，只有实际写入目标的 JSONL 才归一元数据。比较历史完整性时忽略这个运行态字段，避免把相同正文误判为冲突。
- 每次真正写 JSONL 前重新计算 live source/target relation，不执行已经过时的 `Create/Import` 判断。source 快照必须通过完整尾行、session ID、长度与 SHA-256 的前后稳定性校验；发现漂移时只做有界重试，无法冻结则 fail closed。判定为 `Unchanged` 时在返回前还会复检 source version 与 live relation，`Deny` 遇到任何晚到变化都零写入并 fail closed。
- JSONL 生产路径不修改任何已有文件。Create 与 imported 文件先在同目录临时文件中完成 provider 元数据归一并同步，再用 atomic hard-link no-clobber 发布；若目标抢先出现则重新比较，不能覆盖对方。允许替换时，目标是严格旧前缀或内容分叉都会按稳定 source SHA-256 生成并发布完整 imported JSONL；旧目标的 bytes/hash/mtime 保持不变。
- SQLite 活动文件选择是独立策略：current→shared 和 ChatGPT 已关闭的双向切换使用 `SelectMostComplete`，可在完整新文件发布后推进既有 `rollout_path`；只有 hot shared→current 使用 `PreserveExisting`，既有 thread 在 `existing_thread_rollout_path` / `copy_rollout_file` 前直接 duplicate+continue，不读取、写入或发布 target candidate，`copied_session_files = 0`，并保留原 `rollout_path`、provider、title 与旧 writer 可见性。hot 新 thread 仍正常插入，使用已发布文件路径、source row 可用字段和 current provider。
- `session_index.jsonl` 按目标运行态执行显式策略：热同步的 live current 使用 `Skip`，新 thread 可见性由 SQLite 事务插入提供，既有 row 保持 current 状态；关闭态或工具独占的 shared index 重新读取后生成完整 merged bytes，并用同目录 `atomic_write` 原子替换；`Deny` 只校验、零写入。无效或半截 JSON 一律 fail closed。
- 已归档会话默认跳过同步，不会自动写回当前 Codex Home，也不会自动从 shared-sessions 清理。

如果热同步在 Apply 阶段失败，后端只恢复工具内部 shared 的 SQLite 状态，不会用旧状态覆盖可能仍在变化的 live current；完整 no-clobber 文件不会产生半截尾。hot shared→current 的 existing + `PreserveExisting` 在任何 target candidate 文件处理前就直接跳过，因此不会为该既有 thread 留下 imported/orphan 文件。只有实际进入 Create/Import 的 hot 新 thread、current→shared 或关闭态路径，失败时才可能留下完整但尚未引用的文件供重试。操作仍记录为 Failed，两根临时状态检查点都保留，不能宣称 bit-exact 回到操作前。会话同步解决的是 current/shared 的逻辑数据合并，不是持久备份；错误状态也可能被传播，因此不能替代用户主动创建的 full backup。

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

备份区会只读统计 `%APPDATA%\codex-switch\backups` 的目录数量、总字节数、可证明回收的检查点和必须保留的项目。本轮操作仅在终态**成功写入操作日志后**自动处理：同步/切换成功、切换完整回滚、typed `Failed + Backup` 写入前失败，以及恢复可见成功；v3 写入前失败允许严格的一根或两根临时点，恢复可见只允许一根 `StateOnly`。旧成功目录仍要求 current/shared 两个预期目录、唯一 operation ID、有效时间窗、正确 reason 与 canonical source root；legacy v2 只兼容这类严格成功/完整回滚双根证明。cleanup 的 Summary、Receipt 和新操作记录都包含 `attemptedCount` / `failedCount`：只有已进入本轮计划后在执行期 revalidate 或 remove 失败的目录才计入 `failedCount` 并显示部分完成/Failed；Full、孤儿、unclassified 等安全保留项只通过 `retainedCount` 与 warning 说明，不会把成功清理误判为失败。

当前版本**不按年龄、目录数量、mtime 或空间阈值猜测清理**。Apply/回滚/日志失败、孤儿目录、损坏或无法分类的 manifest、缺少终态证据的旧快照全部保留；显式 cleanup 在 plan 与 execute 两阶段都会重新解析严格操作记录，并对完整 manifest、受管路径、文件集合、大小与完整 payload SHA-256 做强复检，计划后任何漂移都 fail closed。只有执行期复检或删除真正失败时，回执才会出现 `failedCount > 0`，页面显示部分完成且操作历史记为 Failed；纯保留 warning 仍是“清理完成（有保留说明）”。持久 Full 不参与自动释放，只能由用户逐个确认删除；创建备份时若 partial 目录清理也失败，错误会同时报告而不再吞掉残留。

2026-07-26 的受控页面清理已把真实备份根从 `21` 个目录、`6,327,089,609` bytes 降到 `17` 个目录、`2,693,977,957` bytes：计划中的 `4/4` 个检查点均删除，回收 `3,633,111,652` bytes，紧邻操作的 C 盘空闲空间测得增加 `3,637,547,008` bytes。首次执行使用的是修复前候选 UI，它把安全保留 warning 误标为“部分完成”，并留下历史 Failed 日志；该历史不改写。现有 `attemptedCount` / `failedCount` 语义和前后端回归已修复此问题，剩余 `17` 项继续保留且不会自动删除。

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
- 脱敏操作记录
- Image2 / Grok 的非敏感配置与 DPAPI 加密凭据

Codex 会话存储说明：

- 官方会话索引默认是 `state_5.sqlite`，辅助 thread 数据还可能位于 `goals_1.sqlite`、`memories_1.sqlite` 和 `logs_2.sqlite`；这些库可能被 `config.toml` 的 `sqlite_home` 或环境变量 `CODEX_SQLITE_HOME` 一起改到别的位置。
- 活跃会话正文位于 Codex home 下的 `sessions/**/*.jsonl`，原生归档正文可能位于 `archived_sessions/**/*.jsonl`；普通热同步只处理活跃 `sessions/`，Full/Sessions 备份与硬删除覆盖两者。
- `session_index.jsonl` 是会话索引增量文件；本工具会一起合并。
- `sqlite/codex-dev.db` 不是当前同步算法依赖的会话来源。

会话管理删除会同时处理当前 Codex Home 和 shared-sessions 的 active/archived 会话；恢复可见只处理当前 Codex Home。

工具自身关键目录：

```text
%APPDATA%\codex-switch\runtimes\plus
%APPDATA%\codex-switch\runtimes\relay
%APPDATA%\codex-switch\shared-sessions
%APPDATA%\codex-switch\backups
%APPDATA%\codex-switch\logs\operations.jsonl
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
- manifest v3 显式记录 `scope` 与 `trackedDatabases`。changed switch 固定使用 `RuntimeState + StateOnly`，普通同步固定使用双 `StateOnly`，不把 live 会话正文复制进临时点；Full/Sessions scope 覆盖 `sessions/` 与 `archived_sessions/`，runtime/state scope 不扩大。hard delete、手工 full backup 和 restore safety 覆盖四个受管 SQLite。所有 payload 使用当前 Windows 用户的 DPAPI 加密并记录大小/SHA-256；v2 快照仍可恢复。
- 会话文件 `Allow` 路径保持零 in-place：Create 原子 no-clobber；允许替换时，严格扩展与分叉来源发布按稳定 source hash 命名的完整 imported JSONL，旧文件 bytes/hash/mtime 不变。closed/current→shared 的 `SelectMostComplete` 只在发布完成后切换引用；hot shared→current 的 existing + `PreserveExisting` 必须在 target path 查询/复制前直接跳过，保持零 target candidate I/O、零 orphan 和 `copied_session_files = 0`。live current index 使用 `Skip`，关闭态或工具独占的 shared index 用完整 merged bytes 原子替换；`Deny` 零写入，`Unchanged` 返回前复检。
- 自动释放只接受成功、完整回滚、typed `Failed + Backup` 写入前失败和恢复可见成功，并要求终态日志、唯一 operation ID、有效时间窗、正确 reason/source root/scope 与删除前 payload hash 复检。v3 写入前失败可严格证明一根或两根，恢复可见只允许单根；legacy v2 只兼容旧成功/完整回滚双根。Full、Apply/回滚/日志失败、孤儿和证据不足目录保留，不按年龄/数量/mtime推测。已验证 Full/legacy v2 只能通过页面内显式确认删除。
- 所有受管 SQLite 备份使用 SQLite Online Backup API，不直接复制可能不一致的 WAL/SHM 文件。
- 所有会创建安全快照的操作都在第一份新快照前检查受管根互不重叠和备份卷容量；空间估算按每个 source 的真实 scope 累加 payload、DPAPI/manifest 开销，使用最大的 SQLite 工作空间，并包含至少 2 GiB 或 15% 的安全余量。
- 关闭态 mutation 会在入口和最后写入前复检受管 ChatGPT 与独立 `codex.exe` CLI；只关闭受管 ChatGPT 进程树，独立 CLI 只作为 fail-closed 阻断信号。
- 需要整文件替换的配置、备份和可写 index 先生成同目录完整临时文件并同步，再用 Windows `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` 原子替换。会话 JSONL 的 Create/Import 使用同目录完整临时文件 + hard-link no-clobber，生产路径绝不原地改写已有 JSONL；任何策略允许的 SQLite 引用切换都只能发生在完整新文件发布后。
- 后端用进程内 try-lock + Windows 独占 `%APPDATA%\codex-switch\mutation.lock` 文件句柄串行化保存、验证、切换、同步、删除、恢复和一键更新安装；同一进程或第二个 ChatGPT Switch 进程已有写操作时，新操作会立即拒绝。更新进入退出阶段后锁保持到父进程终止，前端同时双向禁用其他 mutation。
- 操作记录只保存 action、阶段、终态、操作 ID、备份目录和计数，不保存凭据、请求正文或自由文本错误。
- 会话管理里的删除是硬删除；工具会先备份并支持失败补偿，同时清理四个受管 SQLite 中可识别的 thread 关联。操作前仍需在页面内确认选择范围。
- 自更新 staging 名来自 Windows CSPRNG；目录创建时按当前 token 是否 elevated 应用受限 DACL，并持有目录句柄。helper 以持久化阶段 journal、目标/备份 hash 和 Tauri Ready ACK 决定继续、完成或回滚；elevated 无参数启动不会从 `%TEMP%` 自动发现并执行恢复计划。
- 生产 UI 不使用 emoji、`window.alert`、`window.confirm`、`window.prompt` 或原生 `<dialog>`；配置、确认、进度、Full 删除和失败都在页面内呈现，图标统一使用 Lucide。切换关闭门禁必须在挂载时预注册，运行态刷新完成前不得允许再次切换。

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
npm run tauri -- build
npm run check:release
```

最终 Release 打包使用显式 UPX 路径，不会从 PATH 任取工具或原地修改 raw EXE。脚本固定验证官方 UPX 5.2.0 的 `upx.exe` SHA-256 `F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D`；CI 下载的官方 ZIP 还固定为 `B471EBF1B7F20F4A89150264ED9A008A2A5BFD247F3C6D1184A75BB59CA08F5D`：

```powershell
.\scripts\pack-windows-release.ps1 -UpxPath "C:\path\to\upx.exe"
```

## License

MIT
