# Changelog

## Unreleased

### Fixed

- 运行态切换在确认 ChatGPT 完全退出后校验其 `process_manager/chat_processes.json`。若关闭期间留下截断、全 NUL 或错误结构的瞬态状态，切换会原子重建为空数组并复核。
- 修复从中转站切回 ChatGPT 账号态后手机 Remote 只剩少量新会话的问题。关闭态切换不再只更新 SQLite provider；当既有 JSONL 的 `session_meta.payload.model_provider` 不匹配目标 provider，工具会保留原文件、原子发布 Remote 可识别的 provider 归一副本，并把活动 `rollout_path` 与 `threads.model_provider` 一起切到目标 provider。相同正文/provider 的副本会幂等复用，热同步仍不触碰既有 live 会话。

## v0.2.1 - 2026-07-26

### 备份与数据安全

- 新增页面内“创建完整备份”入口：在 blocking worker 中串行执行容量预检、关闭态复检、current/shared 两根 full snapshot 和强校验，并返回操作 ID、备份路径与 typed receipt；全程不使用系统弹窗。
- changed switch 固定创建 current `RuntimeState` + shared `StateOnly`，普通同步固定创建双 `StateOnly`，不再为了临时回滚复制 GiB 级 `sessions/` payload。切换失败补偿 current/shared config/SQLite；热同步失败只补偿 shared SQLite。实际进入 Create/Import 的路径在失败或崩溃时最多留下未被 SQLite 引用的完整 imported JSONL，不会留下半截文件尾；hot shared→current 的 existing + `PreserveExisting` 在文件处理前直接跳过，不会产生这类残留；不宣称跨介质 bit-exact 回滚。
- 临时点只在终态日志成功落盘后释放：成功、切换完整回滚、typed `Failed + Backup` 写入前失败，以及恢复可见成功；Apply/回滚/日志失败、无法复核或清理失败时保留。删除前重新验证完整 manifest、路径、文件集合、大小与 payload SHA-256。
- 备份区新增占用审计与页面内安全释放任务流，显示目录总占用、可证明回收的字节/数量、安全保留数量、最近结果和警告；显式 cleanup 还支持严格 v3 写入前失败的一根/两根临时点和恢复可见单根 `StateOnly`，legacy v2 只兼容旧成功/完整回滚双根。cleanup 在 plan 与 execute 两阶段都重新验证完整 payload SHA-256，计划后发生任何漂移都保留目录并 fail closed；全程不按年龄、数量、mtime 或空间阈值猜测删除。
- cleanup Summary、Receipt 与操作记录新增 `attemptedCount` / `failedCount`。只有计划内目录在执行期 revalidate 或 remove 失败才计入失败、触发页面“部分完成”和 Failed 日志；Full、孤儿、unclassified 等安全保留 warning 仅作说明，`failedCount = 0` 时仍为成功。对应 Rust 与前端测试同时覆盖“有保留说明但成功”和“无 warning 但真实删除失败”。
- 真实受控 UI 清理已将 `%APPDATA%\codex-switch\backups` 从 `21` 个目录、`6,327,089,609` bytes 降到 `17` 个目录、`2,693,977,957` bytes，计划项 `4/4` 删除并回收 `3,633,111,652` bytes，紧邻操作的 C 盘空闲实测增加 `3,637,547,008` bytes。首次执行使用旧候选 UI，因把安全保留 warning 当作 partial 而留下历史 Failed 记录；该历史不篡改，语义 bug 已由上述字段与测试修复。剩余 17 项继续安全保留，不自动删除。
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
- Full 删除、检查点释放、配置和所有确认继续使用页面内流程；生产 UI 统一 Lucide、禁止 emoji，新增 `public/favicon.ico`。三种目标尺寸的视觉 QA 未发现页面级 overflow、native dialog 或 emoji；最终发布产物仍需复验。
- Relay `/models` 验证现在拒绝空模型列表，并要求配置的 model ID 与 `data[].id` 精确匹配；错误不会回显 API Key 或响应正文，同一次失败只进入一个页面错误面，不再重复显示。
- Relay 当前态精确匹配纳入 provider `base_url`。修改已激活 Relay 的地址后会显示待重新应用，不再把旧地址误判为当前配置。
- 正式 Tauri 窗口最小宽度降到 390px，使 820px/520px 响应式布局在 EXE 中真实可达；会话表是可聚焦、可命名的横向滚动区域。
- 缺少 `auth.json` 或 `config.toml` 时，运行态页面会直接提示先打开 ChatGPT 完成登录并刷新；切换按钮旁持续说明会关闭 ChatGPT，不增加确认弹窗。
- updater 下载连接超时为 30 秒、总超时为 10 分钟；helper readiness 超时执行 kill + wait，staging 清理使用有界重试。已发布 v0.2.0 的首跳仍受旧 120 秒总超时约束，`v0.2.0 -> v0.2.1` 必须由真实一键更新 smoke 证明。
- Windows release profile 固定为 `opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"` 与 strip symbols。新增 `scripts/pack-windows-release.ps1`：验证固定官方 UPX 5.2.0/EXE SHA-256，只压缩 raw 副本，依次执行 raw contract、`--best --lzma`、`upx -t`、packed contract、PE32+ x64/双版本和 3,000,000 bytes 硬门禁；CI 固定官方 ZIP/EXE 两级 SHA-256，并只上传 packed 的裸 `codex-switch.exe`。

### 回归证据

- 新增隔离合成大 Home 回归：64 个等价 JSONL 在 provider 切换后保持文件数、bytes、SHA-256 与 mtime 不变，零复制、零 imported 副本，只更新对应 SQLite provider。可选 ignored benchmark 支持环境变量放大规模。
- 增加容量边界、archived sessions 覆盖、损坏候选回填、list/delete 同强校验、额外文件/hash 漂移排除、256 个显式 Full 管理上限、独立 CLI、Relay 空列表/模型匹配与单一错误面、手工 full backup、source 稳定性、完整 hash-named import、旧目标 bytes/hash/mtime 不变、atomic no-clobber create、hot `PreserveExisting` 的既有 thread 零 candidate I/O/零 orphan/`copied_session_files = 0`、closed `SelectMostComplete`、旧 writer 继续可见、既有 rollout/provider/title 保留、新 thread 插入、index hot `Skip`/完整原子替换/`Deny` 零写、`Unchanged` 终前复检、cleanup plan/execute 完整 SHA、`planningSessions` / `cleaningCheckpoints` 阶段、typed prewrite/恢复可见临时点释放、legacy v2 严格双根证明、备份刷新排队、关闭门禁、runtime refresh pending、updater 超时/清理和页面内任务流测试。
- 发布打包本地临时候选从 raw `5,955,584` bytes 得到 packed `2,228,224` bytes（SHA-256 `4DDC…CED7A`），同一 raw 重复打包 hash 一致。该结果只证明当前本地链路；最终 tag-CI 资产大小/hash、Release 重下载合同和真实 `v0.2.0 -> v0.2.1` 一键更新仍待发布闭环确认。

### 兼容性与已知边界

- 继续只发布 Windows x64 `codex-switch.exe`；当前不按年龄、数量、mtime 或容量阈值自动 prune。持久 Full 可由用户逐个确认删除，但失败、孤儿和无证据目录仍会占用磁盘。
- 热同步仍不是跨 SQLite、JSONL 和 `session_index.jsonl` 的单一 durable transaction；实际进入 Create/Import 的路径会先发布完整 JSONL 且不会产生半尾。hot shared→current 对既有 thread 在文件处理前直接跳过，保持零 target candidate I/O、零 orphan 和 `copied_session_files = 0`；closed `SelectMostComplete` 才会在发布后推进既有引用。合成回归也不等于真实 ChatGPT 重索引 wall-clock 已量化归零。

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
