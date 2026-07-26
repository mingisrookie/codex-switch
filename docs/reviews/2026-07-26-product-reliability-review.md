# ChatGPT Switch 产品与可靠性审查

日期：2026-07-26

## 结论

当前开发分支继续把问题从“长时间无响应的黑盒切换”收敛为“单一应用内 task overlay、后台真实阶段、失败可归因、成功后受控重开 ChatGPT”的任务流，并针对 Remote 历史可见性、v0.2.0 按次吞掉 C 盘空间和 provider 副本写放大补齐 immutable successor、严格 provenance 与证明式回收合同。

本审查在 PR 前的本地候选结论是 **GO for PR**。完整前端/Rust 门禁、隔离临时目录回归、三视口真实渲染、依赖与文本审计、elevated/non-elevated updater 安全测试以及 Windows release 产物合同均已通过。当时 `v0.2.0` 仍不能在远端交付闭环完成前发布：PR/CI、合并、tag CI、Release 重下载校验和真实 `v0.1.9 -> v0.2.0` 一键更新 smoke 是最后的阻断门禁。

下文原始“已解决”表示当时工作树已有对应实现并通过本地门禁，不等同于已发布。`v0.2.0` 的实际远端闭环和随后发现的缺口见下一节。

> 本文后半保留 v0.2.0/v0.2.1 当时结论作为审计历史；当前开发事实见“v0.2.2：v0.2.0 现场反馈与 PR #4 安全替代”。尚未出现的测试总数、PR/CI、tag 或 Release 资产不得从历史结论推断。

## 发布后闭环与二次审计

### v0.2.0 G11 已完成

- [PR #3](https://github.com/mingisrookie/codex-switch/pull/3) 于 `2026-07-25T20:06:34Z` 合并，merge commit 为 `0ca610d5b4fdffe6eb43829310a6865a1b10290c`；PR 两个 Windows quality gate run `30172484662` / `30172483131` 均成功。
- `main` CI [30172929511](https://github.com/mingisrookie/codex-switch/actions/runs/30172929511) 与 tag `v0.2.0` CI [30173342477](https://github.com/mingisrookie/codex-switch/actions/runs/30173342477) 均成功。
- [ChatGPT Switch v0.2.0](https://github.com/mingisrookie/codex-switch/releases/tag/v0.2.0) 是 latest stable；唯一资产 `codex-switch.exe` 为 `15,088,640` bytes，GitHub digest 为 `sha256:42012a1baa73c6f573c5cec047ce3bad0740ddeaf74ff234f80f2d510498a65a`，重下载后 `ProductVersion/FileVersion = 0.2.0`，Authenticode 状态为 `NotSigned`。
- `live_github_release_contract_is_compatible` 与 `live_github_asset_download_contract_is_compatible` 已对公开 Release 通过；隔离 `APPDATA` / `CODEX_HOME` 的真实 `v0.1.9 -> v0.2.0` UI Automation 单击更新成功，只发生一次 Invoke，重启后无 staging/helper 残留，未触碰真实用户 Home。
- PR #2 已留下不合并原因并关闭；v0.2.0 没有吸收其基于目录数量猜测的 retention。v0.2.1 采用独立终态证据：成功/完整回滚沿用严格双根，typed prewrite 可证明一根/两根，恢复可见只接受单根；都不依赖 mtime 或数量推断。

### v0.2.1 二次审计

发布后按产品经理、客户、恢复安全和前端可达性再次审查，确认 `v0.2.0` 仍有需要补丁发布的合同缺口：高频 switch/sync 会复制大体积会话正文、临时点与持久 Full 缺少完整治理、原生 `archived_sessions/` 未进入 Full/硬删除边界、source/target/index 仍有执行期竞态、关闭门禁注册窗口与切后 runtime 刷新存在重复点击窗口、updater 120 秒总超时不适合慢链路。此前发现的手工 Full、损坏候选回填、standalone CLI、Relay 模型与 Base URL exact、窄窗口、首次登录提示也一并纳入。

`v0.2.1` 将 changed switch 固定为 current `RuntimeState` + shared `StateOnly`，普通同步固定为双 `StateOnly`，不再把 GiB 级 session payload 当临时检查点。会话 JSONL 生产路径现为零 in-place：Create 原子 no-clobber；允许替换时，严格扩展和 Divergent 都按稳定 source SHA-256 生成完整 imported JSONL，旧短文件 bytes/hash/mtime 不变。current→shared 与 ChatGPT 已关闭的双向切换使用 `SelectMostComplete`，完整文件发布后才可推进既有 SQLite `rollout_path`；只有 hot shared→current 使用 `PreserveExisting`，既有 current thread 在 `existing_thread_rollout_path` / `copy_rollout_file` 前直接 duplicate+continue，target candidate 不读取、不写入、不发布，`copied_session_files = 0`。原 rollout/provider/title 与旧 writer 可见性保留，不会创建 imported orphan；hot 新 thread 仍事务插入，复用 source row 可用字段并归一 current provider。live current `session_index.jsonl` 使用 `Skip`，工具独占的 shared index 以完整 merged bytes 同目录 `atomic_write`，`Deny` 零写，`Unchanged` 返回前复检。

临时点在终态日志原子持久化后才释放：自动 checkpoint 必须是与唯一 terminal record 精确绑定的 v4；未绑定 v2/v3 自动点 fail closed 保留。plan 与 execute 两阶段都重新强校验完整 payload SHA-256，计划后漂移 fail closed，不按年龄、数量、mtime 或容量猜测。cleanup Summary、Receipt 与新操作记录都包含 `attemptedCount` / `failedCount`；只有计划内目录在执行期 revalidate/remove 失败才是 partial/Failed。持久 Full 列表只在用户请求时加载，最多返回 256 项；经 managed-full 强校验的 v2/v3/v4 Full 可显式管理且绝不自动删除。

前端在挂载时预注册关闭门禁，未 ready 时切换 fail closed；切换后 runtime refresh pending 期间禁用两个切换入口。mutation 错误只出现一次，Relay 验证失败不再由局部与全局路径重复显示；unknown 终态保守提示先不要重新打开 ChatGPT。Full 删除、配置、确认与错误均留在页面内。v0.2.1 下载器采用 30 秒连接、10 分钟总超时，并补 helper kill + wait 与 cleanup retry；已发布 v0.2.0 的首跳虽然仍受 120 秒总超时约束，但真实 `v0.2.0 -> v0.2.1` smoke 已在该窗口内完成。

发布首跳的资产链也已收窄：Cargo release 固定 `opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"`、strip symbols；`scripts/pack-windows-release.ps1` 固定官方 UPX 5.2.0，只压缩 raw 副本，并要求 raw+packed contract、`upx -t`、PE32+ x64/双版本和 3,000,000 bytes 硬门禁。CI 固定 ZIP SHA-256 `B471EBF1B7F20F4A89150264ED9A008A2A5BFD247F3C6D1184A75BB59CA08F5D` 与 `upx.exe` SHA-256 `F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D`，只上传 packed 的裸 `codex-switch.exe`。

### v0.2.1 发布闭环已完成

- [PR #5](https://github.com/mingisrookie/codex-switch/pull/5) 已从工作提交 `702dc37` 合并为 `3b4f440`；PR CI [30194264349](https://github.com/mingisrookie/codex-switch/actions/runs/30194264349) / [30194276794](https://github.com/mingisrookie/codex-switch/actions/runs/30194276794)、main CI [30194772843](https://github.com/mingisrookie/codex-switch/actions/runs/30194772843) 与 tag CI [30195207004](https://github.com/mingisrookie/codex-switch/actions/runs/30195207004) 均成功。
- annotated tag `v0.2.1` 指向 merge commit `3b4f440`。[ChatGPT Switch v0.2.1](https://github.com/mingisrookie/codex-switch/releases/tag/v0.2.1) 是 latest stable、非 draft，且只有一个 `codex-switch.exe` 资产。
- tag-CI artifact 与 Release 资产均为 `2,214,400` bytes、SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`、PE32+ x64、`FileVersion/ProductVersion = 0.2.1`，`upx -t` 通过；Release 重下载的 hash/bytes 与 tag artifact 完全一致。
- `live_github_release_contract_is_compatible` 与 `live_github_asset_download_contract_is_compatible` 两个 ignored live GitHub 合同测试分别执行且各 `1 passed`。
- 正式 v0.2.0（SHA-256 `42012…A65A`）已在隔离环境由 UI Automation 真实点击“立即更新”：约 `2.086s` 出现并点击，旧进程约 `89.103s` 后 exit `0`，`105.1s` 内自动替换并重启 ChatGPT Switch；目标文件为上述正式 v0.2.1 bytes/hash/version，staging/install leftovers 均为 `0`，没有手工替换。
- 发布后的最终 cleanup 为 `0 attempted / 0 failed / Succeeded / Complete`；已验证没有新的可回收临时点，既有 `17` 项继续按证据安全保留。
- 本机 Microsoft Defender Product/Feature disabled，相关命令无法形成扫描通过证据；本节只记录 release contract、hash、`upx -t` 和真实更新闭环，不将它们误写成 Defender 扫描通过。

**CLOSED 状态更新：会话 JSONL/index 原地写入风险、hot-rollout P1、显式 cleanup plan/execute 完整 SHA P1、warning/partial 终态语义和 Relay 重复错误均已修复；PR/main/tag CI、tag-CI packed artifact、正式 Release 回下载合同和真实 `v0.2.0 -> v0.2.1` 一键更新首跳均已完成，本轮产品交付闭环关闭。**

**Hot-rollout P1 已关闭。** `SessionSyncPolicy.existing_rollout_path = ExistingRolloutPathPolicy::PreserveExisting` 现在只用于 hot shared→current；既有 live thread 在任何 target rollout 查询或候选复制前就直接跳过，而不是先发布一个不抢占活动路径的文件。`hot_sync_keeps_the_live_rollout_visible_when_a_longer_different_name_arrives` 同时持有旧 writer、执行同步、再写入并断言 `copied_session_files = 0`、异名 candidate 不存在、DB 的 rollout/provider/title 不变且新增尾部可见；`hot_sync_preserves_active_rollout_when_shorter_candidate_has_a_different_name` 同样断言零复制、candidate 不存在并保留既有 provider/path。`source_database_rollout_advances_to_a_strictly_more_complete_candidate` 则保留 `ExistingRolloutPathPolicy::SelectMostComplete` 在非 hot/关闭态推进更完整引用的能力。

备份治理的对应回归包括：`recent_backup_listing_excludes_candidates_the_delete_contract_rejects` 证明列表与删除接受集合一致；`verified_full_backup_deletion_rejects_extra_files_and_payload_hash_drift` 锁定 extra/hash drift 的 fail-closed；`backup_list_exposes_recovery_points_beyond_the_previous_five_item_cap` 断言上限为 256 且第 6 项可见；`explicit_cleanup_keeps_informational_warnings_out_of_the_failure_count` 与 `checkpoint_cleanup_terminal_depends_on_real_failures_not_warnings` 锁定 warning 不等于失败，前端测试同时覆盖 retained warning 成功和真实删除失败 partial。它们不改变“Full 永不自动删除”的产品边界。

### v0.2.2 candidate：v0.2.0 现场反馈与 PR #4 安全替代

#### 用户现场与磁盘根因

用户明确试用的是 `v0.2.0`。该版本每次 changed switch 都创建并永久保留：

- current Home 的 `Runtime` checkpoint：包含 runtime 文件、`state_5.sqlite`、active session JSONL 和 index；
- shared Home 的 `Sessions` checkpoint：包含 active session JSONL、index 和 `state_5.sqlite`。

本机最新两份 current checkpoint 各约 `966 MB`，两份 shared checkpoint 约 `354 MB` 与 `190 MB`，四份合计约 `2.48 GiB`。这不是“执行文件”，而是 v0.2.0 每次复制整套会话正文留下的加密 checkpoint；成功切换也不删除，因此直接解释了 C 盘每切一次就减少。

当前 candidate 已把 changed switch 改为 current `RuntimeState` + shared `StateOnly`，对应本机 payload 合计约 `25 MB`，不再复制全会话正文；普通同步使用双 `StateOnly`。current RuntimeState 还保存 `process_manager/chat_processes.json` 的 exact bytes。自动 cleanup 只接受携带非空 `operationId + role`、并与唯一 terminal record 全量匹配的 bound BackupManifest v4；未绑定 v2/v3 自动点以及 Apply/rollback/终态日志失败、Full、孤儿、hash drift、incomplete、unclassified 项继续保留。

受控清理只证明 `21` 个目录、`6,327,089,609` bytes 降为 `17` 个目录、`2,693,977,957` bytes，回收 `3,633,111,652` bytes；不能表述成“旧备份已全部清空”。普通切换也不会创建 updater EXE；updater staging 只在用户主动更新时出现。当前普通切换的持久增长仅可能来自已纳入容量预检的 ordinary/new-session rollout、真正 Divergent 的 raw 分支、provider successor/marker 和窄 operation metadata/log。

#### PR #4 决策

审查时 GitHub 只有一个 open PR：[PR #4](https://github.com/mingisrookie/codex-switch/pull/4)。它识别出的核心问题可信：Remote 列表要求 SQLite `threads.model_provider` 与活动 JSONL 第一条 `session_meta.payload.model_provider` 一致，并要求官方 `rollout-<timestamp>-<thread-id>.jsonl` 文件名。

**决定：PR #4 不直接合并，当前任务从 latest main 安全替代。**

阻断项：

1. legacy Remote 文件名在 fast-path 被误判为 unchanged 并冻结 `Deny`，执行期却必须 `Create` 规范文件，形成确定性死路。
2. 在任何 checkpoint 之前修改 `process_manager/chat_processes.json`，且该副作用不在备份、rollback、receipt 或日志合同中。
3. session id 未经严格文件名组件验证就拼入目标路径，存在路径穿越/越界发布风险。
4. provider 副本输出未进入关闭前容量预检；按内容代际创建新副本会造成 O(n²) 全量累积。
5. provider candidate 可能绕过 `SelectMostComplete`，让较短或 divergent shared 分支替换更完整 current 活动历史。

安全替代保持以下顺序：

1. 先用现有 completeness/divergence 规则选出活动历史；
2. 再验证严格 UUID、选中来源与目标父目录的 canonical containment；
3. 在最多 32 个 thread/provider 候选中遇到完整槽位就复用，扫描时只记首个空位；只有有界扫描结束仍无完整槽位时才在该空位发布 Remote-compatible immutable successor；
4. 每个新槽位原子创建不超过 16 KiB 的 provenance marker，记录 `createdBytes` 与创建前缀 SHA-256。创建后的合法 append 可继续证明所有权，创建前缀或 marker 漂移则 fail closed；
5. 任何既有 JSONL 都不覆盖。首次发布或合法增长只写最终 provider 文件与 marker，不先落 raw 再写 provider 双份；真正 Divergent 为避免数据丢失可单独保留 raw 分支并分配独立候选；
6. SQLite provider/path 与活动 JSONL provider 一致提交；
7. durable terminal 之后，只有 current/shared 两库都不再引用、successor 完整包含 predecessor 且两边 marker/source 均未漂移时才回收旧工具槽位；任何证据不足都保留；
8. ChatGPT 仍运行时先用只读 plan 做 pre-close 保守容量初筛；关闭后重建 closed-session 权威写集并再次按实际卷与 checkpoint peak 聚合，每卷保留 `max(2 GiB, 15%)` 余量。容量是保守上界；执行在第一份 checkpoint 前和 checkpoint 后、live 写入前要求权威 plan 精确相等，晚期漂移至多留下 typed prewrite checkpoint，不做 live mutation。

切换与同步回执/UI 分开显示持久会话 `added / reclaimed / net` 和 transient checkpoint reclaimed。前者解释 provider 文件与 marker 的长期净变化，后者只表示本轮窄临时点释放，二者不得合并为一个“已释放”数字。普通 switch/sync 中符合成功、完整回滚或 typed prewrite allow-list 的临时点只在 durable terminal 与强校验通过后清理；手工 Full、hard-delete Full 和 restore safety backup 继续保留。

替代实现将 `process_manager/chat_processes.json` 纳入 current `RuntimeState`：双 checkpoint 和单次进程 inventory 门禁后进入真实 `repairingAppState` phase；任何合法 JSON 都 byte-exact 保留，仅明确空/全 NUL 损坏原子修复为 `[]`，缺失不创建，未知无效格式 fail closed。后续失败在再次进程门禁后恢复原字节，`chatProcessStateRepaired` 同步进入 receipt、操作计数和完成面。

#### 单一 task overlay 与受控启动

- 切换点击后只存在一个 modal/task overlay；它消费真实 `planningSessions` 至 `cleaningCheckpoints`/`LaunchingApp` phase，不显示伪造百分比，也不叠加 native dialog。
- exact verify、durable terminal 和 checkpoint cleanup 完成后，后端才用关闭前从受管 ChatGPT 根捕获的唯一 AppUserModelID 调用 Windows `IApplicationActivationManager`。
- 启动后必须出现同一 AUMID 的受管根才返回 `launched`；已经运行返回 `alreadyRunning`；缺失、冲突、激活失败或超时返回 typed `failed`。
- launch failed 不回滚成功切换；同一完成态显示 warning 和“重新打开 ChatGPT”，重试 command 不接受路径/命令，也不查找 PATH 中同名 EXE。
- UI 保持 Lucide-only、无 emoji，覆盖焦点捕获/恢复、screen reader 名称、reduced-motion 与 1200×820、900×640、390×844 三档无页面 overflow 合同。

上述为 `v0.2.2 candidate` 实现/合同；测试总数、提交 hash、替代 PR、CI、tag 或 Release 资产只在真实完成并核验后记录，不预写尚未产生的证据。

### 本机检查点占用证据与保留决策

清理前只读基线为 `%APPDATA%\codex-switch\backups` 共 `21` 个目录、`6,327,089,609` bytes；严格读取 `operations.jsonl` 并按 action/status/phase、目录引用、reason、source root、scope 与 manifest 交叉验证后，`4` 个目录、`3,633,111,652` bytes 可证明属于已终结临时点。

真实受控 UI 清理随后完成：目录从 `21` 降到 `17`，总量从 `6,327,089,609` 降到 `2,693,977,957` bytes，计划中的 `4/4` 项均删除，回收 `3,633,111,652` bytes；紧邻操作的 C 盘 free 实测增加 `3,637,547,008` bytes。首次执行用的是修复前候选 UI，它因保留 warning 把界面误标为 partial、把日志写成 Failed；这个历史事实和旧日志都不篡改。后续实现以 `failedCount` 而非 warnings 决定 terminal，并有 Rust/前端测试。发布闭环后的最终 cleanup 为 `0 attempted / 0 failed / Succeeded / Complete`；剩余 17 项继续安全保留，不自动删除。

v0.2.1 发布门禁已覆盖并证明：success/rolledBack/prewrite/restore-visible 与 Apply/rollback/log-failure 的保留矩阵；cleanup plan/execute 完整 SHA、`attemptedCount` / `failedCount` 语义与漂移 fail closed；Full list/delete 同强校验、extra/hash drift 排除、按需 256 项和绝不自动删 Full；active/archived 备份/恢复/硬删除；source 稳定性、hash-named 完整 import、旧目标 bytes/hash/mtime 不变、hot `PreserveExisting` 的 existing thread 零 candidate I/O/零 orphan/`copied_session_files = 0`、closed `SelectMostComplete`、旧 writer 可继续写、既有 rollout/provider/title 保留、新 thread 插入、index hot `Skip`/完整原子替换/`Deny` 零写与 `Unchanged` 终前复检；关闭门禁、runtime refresh pending、unknown 终态与 Relay 单一错误面；三尺寸最终产物无 overflow/dialog/emoji；tag-CI packed artifact 的最终大小/hash、Release 重下载合同；以及真实 `v0.2.0 -> v0.2.1` updater 首跳。Defender 因本机功能禁用未形成通过证据，不列入已证明项。

## 审查依据

- 修复前运行数字与根因判断直接内嵌在下方“问题基线”表；不链接被 `.gitignore` 排除的 Trellis runtime 文件，避免 GitHub 死链。
- 当前主链路为 [App.tsx](../../src/App.tsx) -> [api.ts](../../src/api.ts) -> [commands.rs](../../src-tauri/src/commands.rs) -> [runtime_switcher.rs](../../src-tauri/src/runtime_switcher.rs)。
- 会话合并策略位于 [session_sync.rs](../../src-tauri/src/session_sync.rs)，备份与恢复合同位于 [backup.rs](../../src-tauri/src/backup.rs)。
- Windows 进程边界位于 [process_control.rs](../../src-tauri/src/process_control.rs)。
- 凭据绑定与回滚分别位于 [runtime_store.rs](../../src-tauri/src/runtime_store.rs) 和 [skill_manager.rs](../../src-tauri/src/skill_manager.rs)。
- GitHub [PR #2](https://github.com/mingisrookie/codex-switch/pull/2) 审查对象为 head commit [`8c026f61fcaafc2e7f398a769c4afa743ec470cc`](https://github.com/marvellam/codex-switch/commit/8c026f61fcaafc2e7f398a769c4afa743ec470cc)。

## 问题基线

| 用户感知 | 已捕获证据 | 根因判断 | v0.2 成功信号 |
| --- | --- | --- | --- |
| 点击切换后像卡死 | 修复前 Windows WER 有两次 `AppHangB1` | 同步 Tauri command 在 UI 敏感执行路径中做网络、DPAPI、SQLite 和大量文件 I/O | 切换在 blocking worker 中执行，窗口持续响应，开始后立即出现真实任务状态 |
| 一次成功切换仍耗时 137.72 秒 | current 备份约 921.88 MiB / 57 秒，shared 备份约 480.86 MiB / 29.5 秒 | Runtime/Sessions scoped 快照复制大量会话 payload | changed switch 始终使用 `RuntimeState + StateOnly`，会话差异走零 in-place `Allow`，不再扩大检查点 |
| 切换后 ChatGPT 明显变卡 | 532 个 JSONL、约 876.43 MiB 在切换尾段被重写；约 5.6 秒后启动 ChatGPT | 只为 provider 变化触碰全部历史文件，可能触发 ChatGPT 全量重索引 | 原 JSONL 内容与 mtime 保持不变；Remote 只发布/复用活动历史对应的稳定 provider 槽位 |
| 切换完成后仍有额外磁盘压力 | cleanup 前为 21 个目录/6,327,089,609 bytes；受控 UI 已删 4/4、回收 3,633,111,652 bytes，现为 17 个/2,693,977,957 bytes | 高流量临时点未终结，持久 Full 无管理入口，mutation 后重扫 | 高频操作只建窄状态点；严格终态自动/显式释放；已验证 Full 页面内删除；昂贵域按需刷新 |
| 弹窗和控制台闪窗造成困惑 | 旧生产 UI 使用浏览器确认框、prompt、原生 dialog；Windows 子进程未隐藏窗口 | 操作授权与状态被拆散在系统级 UI 中 | 所有授权、配置、进度和错误都留在页面内，Windows helper 无可见控制台 |

上述数字是问题诊断基线，不是新版本性能结果。最终发布必须重新采样，不能直接用基线证明修复有效。

## 当前已解决

### 产品与客户体验

1. **点击即进入任务态**
   - `App.handleSwitch` 在调用后端前同步创建 `switchFlow`。
   - [RuntimeSwitchProgressPanel.tsx](../../src/RuntimeSwitchProgressPanel.tsx) 只消费后端 `RuntimeSwitchProgress` 阶段，不伪造连续百分比。
   - 任务面板位于页面分支之外，切换导航页不会丢失当前任务。
   - 成功、写入前失败、已回滚和回滚失败有不同终态文案；unknown/缺失终态不承诺数据未变化，提示先不要重开 ChatGPT。

2. **不再依赖系统弹窗**
   - 账号覆盖、会话 dry-run、备份恢复、Full 恢复点删除、会话删除和 Skill 配置改为页面内二阶段确认或展开面板。
   - 名称仍为 `RelayRuntimeDialog` 的兼容组件实际渲染 `<section>`，不是原生 `<dialog>`。
   - 关闭监听在应用挂载时预注册，注册未 ready 或失败时切换 fail closed；运行中拦截关闭请求但不弹确认框。

3. **用户可见品牌改为 ChatGPT**
   - 主控制台、账号态、会话加载和任务步骤使用 ChatGPT 文案。
   - `.codex`、`CODEX_HOME`、`codexHome`、`plus` 和 Tauri command 名属于兼容合同，继续保留。
   - 仓库名、包名和 README 仍需在发布门禁中统一审阅，不能把内部兼容标识机械替换掉。

4. **界面语言统一**
   - 生产 React 组件统一从 `lucide-react` 引入图标。
   - 当前生产源码扫描没有 emoji、手写 SVG、`window.confirm`、`window.prompt` 或原生 `<dialog>`。
   - 新样式采用低圆角、中性色基础与 teal/amber/red/blue 状态色，不使用装饰性渐变球体；新增 `public/favicon.ico`。
   - 1200x820、900x640、390x844 三尺寸预验未发现页面级 overflow、dialog 或 emoji；最终发布 dist 仍需复验并留证。

### 可靠性与数据安全

1. **阻塞工作移出 UI 路径**
   - `commands.rs::switch_runtime` 是 async wrapper，完整 mutation 在 `spawn_blocking` worker 中执行。
   - `sync_all_sessions` 和 `inspect_checkpoint_storage` 同样使用 async wrapper + `spawn_blocking`；后者在扫描期间持有 mutation guard。
   - mutation lock、操作记录和 terminal progress 都保留在同一个后端工作单元内。

2. **真实进程树和温和关闭**
   - `process_control.rs` 使用 Windows ToolHelp 枚举 PID/PPID。
   - 仅把 `ChatGPT.exe` 和兼容的 `OpenAI.Codex.exe` 作为受管根，再沿父子关系纳入 `codex.exe` / host 子进程。
   - 独立 `codex.exe` CLI 和 `codex-switch.exe` 不作为 kill root。
   - 先执行非强制 `/PID /T`，轮询最多约 8 秒；只对仍可用 PID、PPID、image 三元组证明身份的存活进程执行 `/F`。
   - `taskkill.exe` 从真实 Windows system directory 解析，拒绝 reparse path，并通过 `CREATE_NO_WINDOW` 隐藏控制台。

3. **pre-close 初筛与 post-close 权威重算形成双容量门禁**
   - `prepare_runtime_switch_before_close` 先在 ChatGPT 仍运行时解析只读 switch plan，并以计划的普通/provider rollout、最多 16 KiB marker 预留、SQLite workspace、index 输出和窄 checkpoint peak 做第一轮按卷容量 fail-fast。
   - 需要变化时完成 roots/容量/relay 初筛后才检测和关闭 ChatGPT；关闭完成后重新构建 closed-session 权威写集，并对同一保守容量模型再次校验。
   - 只有 post-close plan 会进入执行；第一份 checkpoint 前和 checkpoint 后、live 写入前都必须重新证明它未漂移。初筛不足不会关闭 ChatGPT；post-close replan/capacity 失败或第一次 plan 复核失败不创建 checkpoint/output；checkpoint 后才出现的漂移不发布 live JSONL/runtime，已有 typed prewrite checkpoint 按终态证据清理。

4. **备份与失败恢复保持强合同**
   - changed switch 固定使用 current `RuntimeState` + shared `StateOnly`，普通 sync 固定使用双 `StateOnly`，不复制 live 会话 payload。
   - SQLite 使用 Online Backup，而不是直接复制可能处于 WAL 状态的数据库。
   - manifest 校验 payload 路径、大小、SHA-256 和加密标志。
   - 切换失败补偿 current/shared config/SQLite；热同步失败只补偿 shared SQLite，不用旧状态覆盖 live current。实际进入 Create/Import 的 JSONL 始终完整发布；hot existing thread 则在文件处理前直接跳过，零 candidate I/O、零 orphan、`copied_session_files = 0`。前端不把跨介质失败包装成 bit-exact 文件回滚。

5. **临时检查点有可证明的终点**
   - success、完整 rolledBack、typed `Failed + Backup` prewrite 和 restore-visible success 只在操作日志持久化后删除；Apply/rollback/log 失败或复核异常保持不动。
   - 显式 cleanup 严格解析完整 `operations.jsonl`：自动点只接受与唯一 terminal 在 operation ID、role、action/status/phase、时间窗、reason、canonical root、scope、引用和 payload hash 全部匹配的 bound v4；未绑定 v2/v3 自动点保留。
   - plan 与 execute 都重新比较 manifest、受管路径、精确文件集/大小和完整 payload SHA-256；计划后漂移、Full、孤儿、重复引用、日志/manifest/path/hash 损坏全部 fail closed。
   - UI 展示目录占用、可回收字节/数量、安全保留项、最近结果和警告，并用页面内单一诚实运行态与真实成功/失败终态执行，不弹系统窗口。Summary/Receipt/log 均记录 `attemptedCount` / `failedCount`；只有执行期 revalidate/remove 失败才显示 partial 并记录 Failed，安全保留 warning 只显示“完成（有保留说明）”。
   - 普通 switch/sync 中符合成功、完整回滚或 typed prewrite allow-list 的窄临时点只在 durable terminal 与强校验通过后自动释放；Full、hard-delete Full 和 restore safety backup 是持久恢复点，不进入这条自动清理。

6. **持久恢复点由用户管理**
   - v2 legacy Full、v3 Full 与 v4 Full 只在用户请求时扫描，最多返回 256 个；前端展示后端返回的完整 verified 列表，不再裁成最近 5 个。
   - 列表与删除共用 `verify_managed_full_backup`：受管直接子目录、Full scope、manifest、精确文件集合/大小和 payload SHA-256 任一不符都不会显示 verified。
   - 删除要求页面内显式确认并再次双重复检，回报 reclaimed bytes 并写 `deleteBackup` 审计；extra file/hash drift 候选保持在磁盘。
   - 手工 Full、hard delete 和 restore safety 不自动删；失败、孤儿与无证据目录也不会被 mtime/数量规则猜删。

7. **凭据绑定到服务来源**
   - relay 或 Skill 的 URL origin 改变时，空 Key 不再继承旧来源凭据。
   - 同 origin 只修改 path 或 model 时可保留既有 Key。
   - runtime 和 Skill 配置的成对写入失败会恢复并验证旧文件；回滚失败会明确升级错误。
   - 远程 relay 禁止明文 HTTP，只有 loopback HTTP 例外。

8. **破坏性路径 fail closed**
   - `CODEX_HOME` 和 `sqlite_home` 必须是绝对路径，拒绝 `.`、`..` 和相对配置。
   - backup/current/shared 根必须互不相同且互不包含。
   - 会话硬删除同时清理四个受管 SQLite thread 关联，并处理 current/shared 的 `sessions/` 与 `archived_sessions/`；Full/Sessions 备份覆盖 active/archived，runtime/state scope 不扩大。

9. **会话 JSONL/index 生产路径零 in-place**
   - source 在执行期校验完整尾行、session ID、长度和 SHA-256 前后稳定，只允许 bounded retry；`Unchanged` 返回前复检 source version 与 live relation。
   - Create/Import 在同目录临时文件内完成 provider 元数据归一和同步，再通过 atomic hard-link no-clobber 发布；目标已存在时重新比较或 fail closed。任何既有 JSONL 都不覆盖，包括工具拥有的 provider predecessor。
   - provider-aware 路径最多检查 32 个候选，遇到完整 Remote 槽位就复用；扫描时只记首个空位，只有无完整槽位时才在该空位发布 immutable successor。sidecar marker 不超过 16 KiB，并以 `createdBytes` 前缀 SHA-256、thread/provider、文件名和 origin 证明工具所有权；创建后合法 append 可继续使用，创建前缀变化则拒绝所有权。
   - 首次或合法增长只发布最终 provider JSONL 与 marker，不同时生成 raw/provider 两份；真正 Divergent 为避免数据丢失可额外保留 raw 分支。旧 provider predecessor 只有在 durable terminal、两库无引用且 successor 完整包含它时才回收；清理前任一复核失败都保留。
   - current→shared 与关闭态使用 `SelectMostComplete`，完整文件发布后才可推进既有 SQLite 引用；hot shared→current 使用 `PreserveExisting`，既有 thread 在 `existing_thread_rollout_path` / `copy_rollout_file` 前直接 duplicate+continue，不创建 candidate/orphan，保留 rollout/provider/title 和旧 writer 可见性；hot 新 thread 仍进入发布与事务插入。
   - hot current `session_index.jsonl` 使用 `Skip`；closed/app-owned shared index 生成完整 merged bytes 并同目录 `atomic_write`；`Deny` 只校验且零写入。

### 性能

1. **切换不再批量触碰未变化历史**
   - `session_sync.rs::copy_rollout_file` 对相等文件不复制。
   - 关闭态/provider switch 先选择完整活动历史，再复用完整 Remote provider 槽位或发布 immutable successor，让 SQLite provider/path 与活动 JSONL provider 一致；原正文相同 JSONL 不重写。hot shared→current 的既有 row 则按 `PreserveExisting` 保留 provider/title。
   - 只有真正复制或发布最终 provider successor 时才做一次 `session_meta.payload.model_provider` 归一；首次/增长路径不额外落 raw 副本。
   - provider 回归锁定原 JSONL 内容/mtime 不变、16 KiB marker/创建前缀合同、32 个有界候选、前空后完整仍复用完整槽位、无完整槽位才使用首空位、合法 append、immutable growth、证明式 GC 和 SQLite/活动 JSONL provider 一致。
   - typed result/UI 同时展示 `persistentSessionBytesAdded`、`persistentSessionBytesReclaimed` 及其净值；added 按实际 JSONL 与实际 marker 序列化长度计，reclaimed 按证明式 GC 的 JSONL+marker 计，`checkpointCleanup.reclaimedBytes` 单独展示，不能混算。
   - hot shared→current 的 existing branch 更早在 `copy_rollout_file` 前退出，既不读取/散列 target candidate，也不写入/发布文件；重复同步保持 `copied_session_files = 0`，避免异名 imported 文件累积、C 盘增长以及 ChatGPT 文件观察/索引压力。

2. **所有高频切换/同步都使用窄状态检查点**
   - 切换在开始大会话规划前先通过 `Channel` 发送真实 `planningSessions`，避免首个可见阶段晚于昂贵扫描。
   - 切换在关闭前先做只读 plan/capacity 初筛；关闭后再生成用于执行的权威 plan，并再次检查 current→shared 与 shared→current 的 immutable successor/普通 Create/Import、SQLite 和 index 输出。
   - changed switch 始终 current `RuntimeState` + shared `StateOnly`，普通 sync 始终双 `StateOnly`；不再复制 session payload。两边无需文件写入时冻结 `Deny`，需要合并时使用零 in-place `Allow`。
   - `Deny` 后发生 JSONL/index 漂移会零写入并 fail closed；`Allow` 依赖 source 稳定快照、完整 JSONL 原子 no-clobber 发布、显式 rollout 选择策略和 index 策略。post-close plan 在第一份 checkpoint 前和最后写前都复核，不扩大检查点范围。
   - 切换 `Channel` 在后置校验或已验证回滚之后发送真实 `cleaningCheckpoints` 阶段，最终 Complete/Failed 不会掩盖仍在执行的磁盘清理。

3. **昂贵 Dashboard 域按需加载**
   - `loadRuntimeDashboard` 只读 Home 摘要、runtime、active status 和最近操作。
   - `loadSessionDashboard` 只在进入会话页或明确刷新时扫描。
   - `loadBackupDashboard` 只在用户点击加载备份时执行校验，最多返回 256 个与删除合同一致的 verified Full；extra/hash drift 候选不展示。随后读取操作记录并进入持 guard 的检查点空间扫描。
   - 切换成功后刷新 runtime 域并标记 session/backup stale，不再立即读取 GiB 级历史和备份。
   - 若一次备份扫描尚未完成时又有 mutation 请求刷新，前端记录 queued rerun，旧扫描结束后再取一次最新状态，不把 mutation 前快照长期留在页面。

4. **慢链路更新与 helper 清理**
   - v0.2.1 下载器将连接超时与总超时分离为 30 秒和 10 分钟。
   - helper readiness 超时后显式 kill + wait，staging 删除使用有界重试，避免僵尸 helper 或瞬时文件占用留下残留。
   - 该改动不能反向改变已发布 v0.2.0 的 120 秒下载器；真实首跳已在约 `105.1s` 内完成自动替换、重启和零残留验证。

5. **最终资产只发布经过双合同的 packed EXE**
   - Cargo release profile 使用 `opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"` 和 strip symbols。
   - 固定 UPX 5.2.0 仅压缩 raw 副本；raw/packed 都跑 release contract，packed 另跑 `upx -t`、PE32+ x64/双版本和 3,000,000 bytes 上限。
   - 本地临时候选 raw `5,955,584` bytes → packed `2,228,224` bytes（SHA-256 `4DDC…CED7A`）重复打包一致，但只作为历史候选；最终 tag-CI/Release 资产为 `2,214,400` bytes、SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`，回下载合同与真实 v0.2.0 首跳均已完成。

### 可访问性

- 图标按钮提供 `aria-label` / `title`，装饰图标使用 `aria-hidden`。
- busy、错误和任务阶段分别使用 `role="status"`、`role="alert"` 与受控 `aria-live`。
- inline confirmation 自动聚焦标题，取消或完成后恢复触发点焦点。
- CSS 提供 `:focus-visible`、窄屏布局和 `prefers-reduced-motion` 降级。
- 250ms 计时只更新视觉耗时，任务阶段播报不会跟随每次时钟刷新。

## PR #2 决策

审查时 PR #2 是 Draft，GitHub 报告 `MERGEABLE/CLEAN`，Windows CI 成功。CI 通过只证明该分支满足当时测试，不代表其数据保留语义安全。

**决定：不直接 merge 或 cherry-pick PR #2。**

阻断理由：

1. PR 的进程谓词只识别 `codex.exe` / `OpenAI.Codex.exe`，不识别当前桌面主进程 `ChatGPT.exe`。
2. PR 直接强制结束所有匹配进程，不能区分 ChatGPT 子进程和独立 CLI。
3. retention 用“最近成功记录且 `backup_dirs.len() >= 2`”推断完整 current/shared 组；一次同根 restore 也可能有两个备份，因此该条件不是操作组证明。
4. 被保护路径缺失或损坏时，逻辑仍可能继续删除更早的有效快照，保护集没有以“两个 sourceRoot 均验证通过”作为前提。
5. PR 从可写的 `SystemRoot` 环境值拼接 helper 路径；当前实现改为系统 API 解析并验证 canonical path。

已吸收但采用了更强实现的部分：

- ToolHelp 原生进程枚举，替代 `tasklist` CSV 解析。
- `GetDiskFreeSpaceExW` 容量 fail-fast。
- 容量估算进一步覆盖 DPAPI payload 开销、manifest 开销、SQLite Online Backup 工作空间、最低 2 GiB 或 15% reserve，以及 current/shared/backup 和外置 SQLite root 的冲突检查。

没有合并或复用 PR #2 的 retention 算法。v0.2.1 只吸收“磁盘占用必须可见、恢复点必须可治理”的产品目标，明确拒绝用 mtime、路径数量、年龄或总量推断安全性。自动点以严格 operation record、唯一 ID、有效时间窗、reason/source root/scope 和删除前 payload hash 共同证明；持久 Full 则要求用户显式确认并再次强校验。

## CC Switch 对照结论

官方 CC Switch 当前会话迁移更接近单个 Home 的一次性 provider bucket/migration，不是本项目 current/shared 双根热同步，不能直接移植。可借鉴的是三点：迁移/写入前冻结 source 稳定性、把归档目录视作一等数据、把迁移来源与结果写入可审计记录。本项目据此补齐 source 尾行/session ID/len/hash 稳定校验和 `archived_sessions/` Full/硬删除范围，但保留更强的跨进程 mutation guard、DPAPI scoped checkpoint、带严格 provenance 的 immutable successor、`PreserveExisting`/`SelectMostComplete` 活动引用策略和按运行态选择的 index 原子策略；不复制其单 Home 整文件 migration。

## v0.2.0 发布门禁

| Gate | 必须提供的证明 | 当前判定 |
| --- | --- | --- |
| G1 前端行为 | 更新旧测试后，Channel 成功/失败/回滚、一次点击切换、跨 tab 任务轨道、lazy sessions/backups、无 native dialog 全覆盖；`npm test -- --run` 通过 | 通过：5 个文件、76/76 |
| G2 类型与构建 | `npm run typecheck`、`npm run build` 通过 | 通过：0 diagnostics，1789 modules |
| G3 Rust 质量 | `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked` 通过 | 通过：202 passed / 0 failed / 2 live tests ignored |
| G4 临时 Home E2E | 只用隔离的临时 `CODEX_HOME` 验证 relay/account 成功、幂等、写入前失败、写入后回滚；不得关闭承载当前任务的真实 ChatGPT | 通过：临时目录集成测试覆盖成功、no-op、失败和双根回滚；未触碰真实 Home |
| G5 性能回归 | 证明等价 JSONL 内容与 mtime 不变；切换后 invoke 列表不包含 session/backup 全量扫描；任务期间 UI event loop 持续响应 | 通过：bytes/mtime、runtime-only scan、lazy stale 和 blocking worker 均有回归 |
| G6 视觉与交互 | 1200x820、900x640、390x844 截图；无重叠、溢出、不可操作控件；Playwright `dialog` 事件计数为 0；reduced motion 截图通过 | 通过：三视口页面级横向溢出为 0；移动导航遮挡已修复；dialog=0；reduced-motion 动画/transition=0 |
| G7 安装更新权限边界 | 验证 elevated token 检测、CSPRNG staging 名、受限 DACL、canonical path、持有目录句柄和任一步失败即中止；若无法证明则 elevated self-update 必须 fail closed | 通过：实际父/子 DACL、CSPRNG、lease、journal、WRITE_THROUGH、离线恢复和 Ready ACK 回归通过；elevated 无参数恢复发现 fail closed |
| G8 依赖与敏感信息 | 修复或解释依赖漏洞；secret、URL credential、明文 Key 扫描通过 | 通过：PostCSS 8.5.23 / NanoID 3.3.16；生产与完整 `npm audit` 均为 0；敏感形态扫描无真实凭据 |
| G9 品牌、版本与文档 | `package.json`、Cargo、Tauri config、README、CHANGELOG 和 release contract 全部为 `0.2.0`；用户文案为 ChatGPT，兼容标识保留 | 通过：五处版本为 0.2.0，内嵌 Skill 与 PE 品牌同步为 ChatGPT Switch |
| G10 Windows 产物 | Tauri release build 成功；PE `ProductVersion/FileVersion`、资产名、SHA-256、体积和 `npm run check:release` 一致 | 通过：15,129,600 bytes；双版本 0.2.0；本地 SHA-256 `d9e7b8ce1a576635ddd3c46060b4020c5c28fdc90414c4314b3ba473b3c5fa94`；Authenticode `NotSigned` |
| G11 交付闭环 | PR CI 绿并合并；tag `v0.2.0`；Release 资产重下载校验；两个 live GitHub 合同测试；真实 v0.1.9 一键更新；PR #2 留说明并关闭 | 发布后通过；commit、run、资产 digest 与隔离 UIA 证据见上方 addendum |
| G12 文本卫生 | 生产源码无 emoji、native dialog、手写 SVG、乱码和异常替换字符；`git diff --check` 通过 | 通过 |
| G13 v0.2.1 补丁闭环 | PR/main/tag CI；packed 单资产；Release 回下载；两个 live GitHub 合同；真实 v0.2.0 一键更新；最终 cleanup | 通过：merge `3b4f440`、tag run `30195207004`、资产 `2,214,400` bytes / `8F6E…020C`、UIA `105.1s` 内自动重启、leftovers `0`、cleanup `0 attempted / 0 failed / Succeeded / Complete`；Defender disabled，不计为扫描通过 |

上表 G10 是已发布 v0.2.0 的历史 raw 产物证据，不应改写。v0.2.1 已新增 fixed-profile + pinned-UPX packed 链；本地临时候选 `5,955,584 -> 2,228,224` bytes、SHA-256 `4DDC…CED7A` 只作为历史候选。最终 tag-CI 与 Release 回下载资产均为 `2,214,400` bytes、SHA-256 `8F6EA219A53BB3395F039327A3CD3827B53EE67B8DAF4B130E60235940A3020C`，并已完成双向一致性复核。

### elevated self-update 为什么仍是发布门禁

updater 会把 helper、更新资产和 plan 放在系统临时目录，再由 helper 替换安装位置。高完整性进程若使用普通低权限可写 staging，会形成不应接受的权限边界。

当前源码已经增加 `process_is_elevated`、系统 CSPRNG 随机目录名、创建时受限 DACL 和 `StagingGuard`。elevated staging 的 ACL 只允许 `SYSTEM` / `Administrators`，普通 staging 只允许 `SYSTEM` / owner；安全描述符、目录创建、canonical 校验或句柄获取任一步失败都会中止。正常 helper 仍以显式参数闭合完成或回滚；elevated 无参数启动不会扫描普通 `%TEMP%` 自动发现并执行恢复计划，token elevation 查询失败同样 fail closed。

Windows 定向测试已实际创建受保护 staging 和子文件：父目录 DACL 受保护并具有预期 `OICI`，子文件只保留受限主体，当前进程仍可正常读写；target lease 也通过跨进程排他测试。该证明覆盖本次实现边界，但不替代 Authenticode 或离线签名信任根。

## 产品经理与客户视角

### 核心产品承诺

用户购买的不是“瞬时切换”，而是“我知道现在正在做什么，失败时不会覆盖已有会话”。高频 switch/sync 的安全边界不是复制全部 JSONL，而是窄 config/SQLite 检查点、pre-close 初筛 + post-close 权威写集的保守容量门禁，以及不覆盖既有文件的 immutable successor 协议；旧工具槽位也只能在 durable terminal、两库无引用和完整 successor 的共同证明下回收。真正会删除完整数据的 hard delete、手工 Full 和 restore safety 仍必须覆盖四库与 active/archived 会话正文。正确优化顺序是：

1. 立即反馈并保持窗口响应。
2. 不做与本次变更无关的 I/O。
3. 让每个阶段、失败位置和回滚结果可解释。
4. 再优化备份结构和总体耗时。

### 当前信息架构的优点

- 首屏直接是可操作的双槽位控制台，不是营销 landing page。
- “ChatGPT 账号态”和“API 中转站态”对应用户真实心智，不伪装成无限 provider 管理器。
- 任务轨道、操作回执、备份恢复和历史记录形成同一条可信链。
- 会话页和技能页按使用场景分离，故障不会污染首屏核心切换。

### 仍可能让客户困惑的地方

- 用户主动 Full、hard delete 和完整恢复仍可能复制 GiB 级 active/archived payload，需要几十秒；当前只有阶段和总耗时，没有字节进度或阶段内 ETA。
- 切换点击即授权关闭 ChatGPT，这符合无弹窗目标，但按钮附近应持续保留简短、非弹窗式的行为说明。
- 切换期间禁止关闭窗口但没有取消。进入写入阶段后不应提供强制取消；未来只能在任何写入前提供安全取消。
- 仓库和可执行资产仍使用 `codex-switch` 兼容名，而界面叫 ChatGPT Switch。短期保留可避免破坏 updater URL 和用户路径，但发布说明必须解释这是兼容命名。
- 持久 Full 已可按需列出最多 256 个并逐个删除，但尚无 pin/按操作组的 retention；extra/hash drift、失败、孤儿与无证据目录不会伪装成 verified，也仍会长期占用磁盘。容量预检与“可证明回收”不等于所有历史目录可删。
- provider predecessor GC 不属于 backup retention：它只能处理 provenance marker 明确归属、已被完整 successor 取代且 current/shared 两库都不再引用的旧会话槽位，不能授权按年龄、数量或剩余空间清理任意 JSONL、Full 或孤儿目录。

## 后续路线

### 后续：持久恢复点 retention 生命周期

- 当前严格 action/status/phase 解决自动临时点，页面内强校验 + 确认解决单个 Full 删除；仍不授权自动删除失败或无证据目录。
- 若未来治理持久恢复点，应为每次 mutation 写入不可歧义的 `operationGroupId`，并记录 `sourceRootKind`、canonical source root、完成状态和 verify 状态。
- 只有 current/shared 两个预期来源均存在且校验成功，才能把该组标为可恢复完整组；至少保留最近完整成功组、最近失败/回滚组和用户 pin 的组。
- 删除前再次校验保护集；保护集不完整时 fail closed，不执行任何 prune。UI 必须继续展示预计释放空间、保留原因和清理结果，不做静默清理。

### 后续：性能可观测性

- 在 operation record 中记录每阶段开始、结束、输入文件数和字节数。
- 只保存本机脱敏统计，不记录 API Key、会话正文或 relay 响应。
- 增加“导出诊断摘要”，默认只含版本、阶段耗时、错误类别和计数。
- 用固定的合成大 Home 做基准，防止再次出现全量 JSONL mtime 抖动。

### v0.2.x: updater 与供应链

- v0.2.1 已将下载连接/总超时调整为 30 秒/10 分钟并补 helper kill + wait、cleanup retry；真实 v0.2.0 首跳已完成，后续版本可使用新超时合同。
- 在受限 DACL 已验证的基础上继续收紧 helper/asset 的不可变句柄与替换时身份复检；无法证明时保持 elevated self-update fail closed。
- 引入 Windows Authenticode 签名；签名前 Release 必须同时发布 SHA-256 并明确“未签名”。
- 固定依赖版本、保留 lockfile，建立依赖漏洞的可利用性审查记录。

### v0.3: 更快但不削弱恢复能力

- 研究 Full/硬删除/restore safety 的内容寻址、分块或增量加密备份，目标是减少真正持久恢复点的重复 GiB payload；高频 switch/sync 已不再复制会话正文。
- SQLite 继续使用 Online Backup；不能用普通文件 copy 换速度。
- JSONL 去重方案必须考虑 DPAPI 密文非确定性，不能把明文内容哈希暴露到公共日志。
- 任何增量方案都必须能从单个 manifest 验证并完整恢复，不能依赖猜测目录状态。

## 发布结论

本节原为 PR 前发布建议。G11 已按上方发布后 addendum 完成，`v0.2.0` 已发布；二次审计发现的补丁项也已由 `v0.2.1` 独立通过本地、PR/main/tag CI、Release 重下载和隔离一键更新闭环，本轮产品目标已完成。发布说明应明确：

- 已修复 UI 卡死和切后全量历史触碰；
- changed switch 固定 `RuntimeState + StateOnly`、普通 sync 固定双 `StateOnly`，会话差异通过零 in-place `Allow` 与完整文件原子发布处理，不再扩大为 Runtime/Sessions；
- hot shared→current 的既有 thread 在 target candidate 文件处理前直接跳过，零复制、零 orphan，避免重复同步增加 C 盘会话文件；
- 任务轨道显示真实阶段，不是估算百分比；
- 不再使用系统确认弹窗；
- Relay 验证失败只进入一个页面错误面，不再重复显示；
- 自动清理成功/完整回滚、typed prewrite 和恢复可见临时点；显式 cleanup 的 plan/execute 都复检完整 SHA；Apply/回滚/日志失败、孤儿与无证据目录保留，且不按年龄/数量/mtime prune；
- cleanup 的 `attemptedCount` / `failedCount` 区分实际执行失败与安全保留 warning；真实受控 UI 已从 21 目录/6,327,089,609 bytes 清到 17 目录/2,693,977,957 bytes，4/4 删除、回收 3,633,111,652 bytes，剩余 17 项不自动删；旧候选产生的 partial/Failed 历史记录不改写；
- Full/Sessions 与 hard delete 覆盖 `archived_sessions/`；列表/删除使用同一强校验，extra/hash drift 不展示 verified，按需最多 256 个已验证 Full 可页面内显式管理，手工/hard-delete/restore-safety Full 不自动删；
- v0.2.1 的 30 秒连接/10 分钟下载超时只影响升级后的下载器；真实 `v0.2.0 -> v0.2.1` 首跳已在 `105.1s` 内自动替换并重启，且零 staging/install leftovers；
- 发布资产使用固定 profile 与 pinned UPX copy-only pipeline，CI 只上传 packed `codex-switch.exe`；本地 `2,228,224` bytes / `4DDC…CED7A` 只是历史候选，最终 tag-CI/Release 资产为 `2,214,400` bytes / `8F6E…020C`；
- EXE 是否完成 Authenticode 签名。
- Microsoft Defender 因本机 Product/Feature disabled 未形成扫描通过证据，不能把 release contract 或 `upx -t` 冒充为 Defender 扫描。
