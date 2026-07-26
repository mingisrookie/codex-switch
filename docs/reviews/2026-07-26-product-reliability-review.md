# ChatGPT Switch 产品与可靠性审查

日期：2026-07-26

## 结论

### 2026-07-26 Remote 可见性更正

后续手机 Remote 实测推翻了本文“provider 只更新 SQLite 即可”的假设：本机 319 个交互会话中，Remote 恰好只显示 SQLite 与 JSONL 首条 `session_meta.payload.model_provider` 都为 `openai` 的 3 个会话；把一条不匹配 JSONL 修正为 `openai` 后，隔离 `codex app-server thread/list` 立即从同一 fixture 枚举到该会话。默认会话枚举会扫描 JSONL 元数据，SQLite-only provider 更新会让历史在切换回账号态后被过滤。

当前修复保持本文的零 in-place 安全边界，但修正了关闭态策略：provider 不匹配时不改写旧文件，而是原子发布 provider 已归一、文件名仍符合官方 rollout 形态的完整副本，并让 SQLite 指向它；相同内容/provider 幂等复用。hot shared→current 的 `PreserveExisting` 仍保持完全不触碰既有 live thread。下文关于 `v0.2.1` 的 SQLite-only 结论属于当时发布行为，不再代表当前正确合同。

当前分支已经把问题从“长时间无响应的黑盒切换”改造成“后台执行、页面内展示真实阶段、失败可归因”的任务流，并针对切换后的磁盘争用修复了三个主要放大器：无变化历史 JSONL 的批量重写、切换结束后的全量会话/备份扫描，以及已经终结的切换/同步检查点长期累积。

本审查在 PR 前的本地候选结论是 **GO for PR**。完整前端/Rust 门禁、隔离临时目录回归、三视口真实渲染、依赖与文本审计、elevated/non-elevated updater 安全测试以及 Windows release 产物合同均已通过。当时 `v0.2.0` 仍不能在远端交付闭环完成前发布：PR/CI、合并、tag CI、Release 重下载校验和真实 `v0.1.9 -> v0.2.0` 一键更新 smoke 是最后的阻断门禁。

下文原始“已解决”表示当时工作树已有对应实现并通过本地门禁，不等同于已发布。`v0.2.0` 的实际远端闭环和随后发现的缺口见下一节。

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

临时点在终态日志持久化后才释放：成功、切换完整回滚、typed `Failed + Backup` 写入前失败，以及恢复可见成功。显式 cleanup 支持严格 v3 prewrite 一根/两根和 restore-visible 单根，legacy v2 仅兼容旧成功/完整回滚双根；plan 与 execute 两阶段都重新强校验完整 payload SHA-256，计划后漂移 fail closed，不按年龄、数量、mtime 或容量猜测。cleanup Summary、Receipt 与新操作记录都包含 `attemptedCount` / `failedCount`；只有计划内目录在执行期 revalidate/remove 失败才是 partial/Failed，Full、孤儿、unclassified 等安全保留 warning 只作说明。持久 Full 列表只在用户请求时加载，最多返回 256 项；列表和删除共用 managed-full 强校验，extra file/hash drift 不显示为 verified 且保持不动。手工 Full、hard delete、restore safety 不自动删，但通过校验的 Full/legacy v2 可在页面内显式确认删除并写审计。Full/Sessions 与硬删除覆盖 `archived_sessions/`，runtime/state scope 不扩大。

前端在挂载时预注册关闭门禁，未 ready 时切换 fail closed；切换后 runtime refresh pending 期间禁用两个切换入口。mutation 错误只出现一次，Relay 验证失败不再由局部与全局路径重复显示；unknown 终态保守提示先不要重新打开 ChatGPT。Full 删除、配置、确认与错误均留在页面内。v0.2.1 下载器采用 30 秒连接、10 分钟总超时，并补 helper kill + wait 与 cleanup retry；但已发布 v0.2.0 的首跳仍是 120 秒，必须用真实 `v0.2.0 -> v0.2.1` smoke 证明。

发布首跳的资产链也已收窄：Cargo release 固定 `opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"`、strip symbols；`scripts/pack-windows-release.ps1` 固定官方 UPX 5.2.0，只压缩 raw 副本，并要求 raw+packed contract、`upx -t`、PE32+ x64/双版本和 3,000,000 bytes 硬门禁。CI 固定 ZIP SHA-256 `B471EBF1B7F20F4A89150264ED9A008A2A5BFD247F3C6D1184A75BB59CA08F5D` 与 `upx.exe` SHA-256 `F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D`，只上传 packed 的裸 `codex-switch.exe`。

**BLOCK 状态更新：会话 JSONL/index 原地写入风险、hot-rollout P1、显式 cleanup plan/execute 完整 SHA P1、warning/partial 终态语义和 Relay 重复错误均已修复，不再作为当前候选的阻断项。整体交付仍等待 tag-CI packed artifact、正式 Release 重下载闭环，以及真实 `v0.2.0 -> v0.2.1` 一键更新首跳验证。**

**Hot-rollout P1 已关闭。** `SessionSyncPolicy.existing_rollout_path = ExistingRolloutPathPolicy::PreserveExisting` 现在只用于 hot shared→current；既有 live thread 在任何 target rollout 查询或候选复制前就直接跳过，而不是先发布一个不抢占活动路径的文件。`hot_sync_keeps_the_live_rollout_visible_when_a_longer_different_name_arrives` 同时持有旧 writer、执行同步、再写入并断言 `copied_session_files = 0`、异名 candidate 不存在、DB 的 rollout/provider/title 不变且新增尾部可见；`hot_sync_preserves_active_rollout_when_shorter_candidate_has_a_different_name` 同样断言零复制、candidate 不存在并保留既有 provider/path。`source_database_rollout_advances_to_a_strictly_more_complete_candidate` 则保留 `ExistingRolloutPathPolicy::SelectMostComplete` 在非 hot/关闭态推进更完整引用的能力。

备份治理的对应回归包括：`recent_backup_listing_excludes_candidates_the_delete_contract_rejects` 证明列表与删除接受集合一致；`verified_full_backup_deletion_rejects_extra_files_and_payload_hash_drift` 锁定 extra/hash drift 的 fail-closed；`backup_list_exposes_recovery_points_beyond_the_previous_five_item_cap` 断言上限为 256 且第 6 项可见；`explicit_cleanup_keeps_informational_warnings_out_of_the_failure_count` 与 `checkpoint_cleanup_terminal_depends_on_real_failures_not_warnings` 锁定 warning 不等于失败，前端测试同时覆盖 retained warning 成功和真实删除失败 partial。它们不改变“Full 永不自动删除”的产品边界。

### 本机检查点占用证据与保留决策

清理前只读基线为 `%APPDATA%\codex-switch\backups` 共 `21` 个目录、`6,327,089,609` bytes；严格读取 `operations.jsonl` 并按 action/status/phase、目录引用、reason、source root、scope 与 manifest 交叉验证后，`4` 个目录、`3,633,111,652` bytes 可证明属于已终结临时点。

真实受控 UI 清理随后完成：目录从 `21` 降到 `17`，总量从 `6,327,089,609` 降到 `2,693,977,957` bytes，计划中的 `4/4` 项均删除，回收 `3,633,111,652` bytes；紧邻操作的 C 盘 free 实测增加 `3,637,547,008` bytes。首次执行用的是修复前候选 UI，它因保留 warning 把界面误标为 partial、把日志写成 Failed；这个历史事实和旧日志都不篡改。后续实现以 `failedCount` 而非 warnings 决定 terminal，并有 Rust/前端测试。剩余 17 项继续安全保留，不自动删除。

v0.2.1 发布门禁必须额外证明：success/rolledBack/prewrite/restore-visible 与 Apply/rollback/log-failure 的保留矩阵；cleanup plan/execute 完整 SHA、`attemptedCount` / `failedCount` 语义与漂移 fail closed；Full list/delete 同强校验、extra/hash drift 排除、按需 256 项和绝不自动删 Full；active/archived 备份/恢复/硬删除；source 稳定性、hash-named 完整 import、旧目标 bytes/hash/mtime 不变、hot `PreserveExisting` 的 existing thread 零 candidate I/O/零 orphan/`copied_session_files = 0`、closed `SelectMostComplete`、旧 writer 可继续写、既有 rollout/provider/title 保留、新 thread 插入、index hot `Skip`/完整原子替换/`Deny` 零写与 `Unchanged` 终前复检；关闭门禁、runtime refresh pending、unknown 终态与 Relay 单一错误面；三尺寸最终产物无 overflow/dialog/emoji；tag-CI packed artifact 的最终大小/hash、Release 重下载合同；以及真实 `v0.2.0 -> v0.2.1` updater 首跳。

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
| 切换后 ChatGPT 明显变卡 | 532 个 JSONL、约 876.43 MiB 在切换尾段被重写；约 5.6 秒后启动 ChatGPT | 只为 provider 变化触碰全部历史文件，可能触发 ChatGPT 全量重索引 | 无变化 JSONL 的内容与 mtime 保持不变，只更新必要 SQLite 行和真正复制的文件 |
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

3. **关闭 ChatGPT 前完成只读 fail-fast**
   - `prepare_runtime_switch_before_close` 先解析完整 switch plan。
   - 需要变化时先执行双根备份容量预检和根目录互斥检查，再验证 relay，之后才检测和关闭 ChatGPT。
   - 空间不足、根目录重叠、runtime 文件无效或 relay 不可用时，不应先关闭用户正在使用的 ChatGPT。

4. **备份与失败恢复保持强合同**
   - changed switch 固定使用 current `RuntimeState` + shared `StateOnly`，普通 sync 固定使用双 `StateOnly`，不复制 live 会话 payload。
   - SQLite 使用 Online Backup，而不是直接复制可能处于 WAL 状态的数据库。
   - manifest 校验 payload 路径、大小、SHA-256 和加密标志。
   - 切换失败补偿 current/shared config/SQLite；热同步失败只补偿 shared SQLite，不用旧状态覆盖 live current。实际进入 Create/Import 的 JSONL 始终完整发布；hot existing thread 则在文件处理前直接跳过，零 candidate I/O、零 orphan、`copied_session_files = 0`。前端不把跨介质失败包装成 bit-exact 文件回滚。

5. **临时检查点有可证明的终点**
   - success、完整 rolledBack、typed `Failed + Backup` prewrite 和 restore-visible success 只在操作日志持久化后删除；Apply/rollback/log 失败或复核异常保持不动。
   - 显式 cleanup 严格解析完整 `operations.jsonl`：v3 支持 prewrite 一根/两根、restore-visible 单根和成功/完整回滚双根；legacy v2 只兼容旧成功/完整回滚双根。operation ID、时间窗、reason、canonical root、scope 与引用必须精确。
   - plan 与 execute 都重新比较 manifest、受管路径、精确文件集/大小和完整 payload SHA-256；计划后漂移、Full、孤儿、重复引用、日志/manifest/path/hash 损坏全部 fail closed。
   - UI 展示目录占用、可回收字节/数量、安全保留项、最近结果和警告，并用页面内单一诚实运行态与真实成功/失败终态执行，不弹系统窗口。Summary/Receipt/log 均记录 `attemptedCount` / `failedCount`；只有执行期 revalidate/remove 失败才显示 partial 并记录 Failed，安全保留 warning 只显示“完成（有保留说明）”。

6. **持久恢复点由用户管理**
   - Full 与 legacy v2 只在用户请求时扫描，最多返回 256 个；前端展示后端返回的完整 verified 列表，不再裁成最近 5 个。
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
   - Create/Import 在同目录临时文件内完成 provider 元数据归一和同步，再通过 atomic hard-link no-clobber 发布；目标已存在时重新比较或 fail closed。允许替换时，严格扩展与 Divergent 都发布按稳定 source hash 命名的完整 imported JSONL，旧目标 bytes/hash/mtime 不变。
   - current→shared 与关闭态使用 `SelectMostComplete`，完整文件发布后才可推进既有 SQLite 引用；hot shared→current 使用 `PreserveExisting`，既有 thread 在 `existing_thread_rollout_path` / `copy_rollout_file` 前直接 duplicate+continue，不创建 candidate/orphan，保留 rollout/provider/title 和旧 writer 可见性；hot 新 thread 仍进入发布与事务插入。
   - hot current `session_index.jsonl` 使用 `Skip`；closed/app-owned shared index 生成完整 merged bytes 并同目录 `atomic_write`；`Deny` 只校验且零写入。

### 性能

1. **切换不再批量触碰未变化历史**
   - `session_sync.rs::copy_rollout_file` 对相等文件不复制。
   - 关闭态/provider switch 可以更新 SQLite `threads.model_provider`，不要求重写已经存在且正文相同的 JSONL；hot shared→current 的既有 row 则按 `PreserveExisting` 保留 provider/title。
   - 只有真正复制、新建或发布完整 imported JSONL 时才做一次 `session_meta.payload.model_provider` 归一。
   - `provider_switch_updates_sqlite_without_rewriting_unchanged_existing_jsonl` 明确断言内容和 mtime 不变。
   - hot shared→current 的 existing branch 更早在 `copy_rollout_file` 前退出，既不读取/散列 target candidate，也不写入/发布文件；重复同步保持 `copied_session_files = 0`，避免异名 imported 文件累积、C 盘增长以及 ChatGPT 文件观察/索引压力。

2. **所有高频切换/同步都使用窄状态检查点**
   - 切换在开始大会话规划前先通过 `Channel` 发送真实 `planningSessions`，避免首个可见阶段晚于昂贵扫描。
   - 切换 plan 同时检查 current→shared 与 shared→current 是否需要 Create/Import JSONL，以及 index 应采用 `Skip`、完整原子合并或 `Deny`。
   - changed switch 始终 current `RuntimeState` + shared `StateOnly`，普通 sync 始终双 `StateOnly`；不再复制 session payload。两边无需文件写入时冻结 `Deny`，需要合并时使用零 in-place `Allow`。
   - `Deny` 后发生 JSONL/index 漂移会零写入并 fail closed；`Allow` 依赖 source 稳定快照、完整 JSONL 原子 no-clobber 发布、显式 rollout 选择策略和 index 策略，不扩大检查点范围。
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
   - 该改动不能反向改变已发布 v0.2.0 的 120 秒下载器；首跳必须以真实更新 smoke 证明。

5. **最终资产只发布经过双合同的 packed EXE**
   - Cargo release profile 使用 `opt-level = "z"`、LTO、单 codegen unit、`panic = "abort"` 和 strip symbols。
   - 固定 UPX 5.2.0 仅压缩 raw 副本；raw/packed 都跑 release contract，packed 另跑 `upx -t`、PE32+ x64/双版本和 3,000,000 bytes 上限。
   - 本地临时候选为 raw `5,955,584` bytes、packed `2,228,224` bytes（SHA-256 `4DDC…CED7A`），重复打包一致；最终 tag-CI hash/尺寸、Release 重下载和真实 v0.2.0 首跳仍未由该本地证据替代。

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

官方 CC Switch 当前会话迁移更接近单个 Home 的一次性 provider bucket/migration，不是本项目 current/shared 双根热同步，不能直接移植。可借鉴的是三点：迁移/写入前冻结 source 稳定性、把归档目录视作一等数据、把迁移来源与结果写入可审计记录。本项目据此补齐 source 尾行/session ID/len/hash 稳定校验和 `archived_sessions/` Full/硬删除范围，但保留更强的跨进程 mutation guard、DPAPI scoped checkpoint、完整 hash-named JSONL 的 no-clobber 发布、`PreserveExisting`/`SelectMostComplete` 活动引用策略和按运行态选择的 index 原子策略；不复制其单 Home 整文件 migration。

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

上表 G10 是已发布 v0.2.0 的历史 raw 产物证据，不应改写。v0.2.1 已新增 fixed-profile + pinned-UPX packed 链，本地临时候选 `5,955,584 -> 2,228,224` bytes、SHA-256 `4DDC…CED7A` 只证明当前工作树；最终尺寸/hash 必须来自 tag CI 并在 Release 重下载后复核。

### elevated self-update 为什么仍是发布门禁

updater 会把 helper、更新资产和 plan 放在系统临时目录，再由 helper 替换安装位置。高完整性进程若使用普通低权限可写 staging，会形成不应接受的权限边界。

当前源码已经增加 `process_is_elevated`、系统 CSPRNG 随机目录名、创建时受限 DACL 和 `StagingGuard`。elevated staging 的 ACL 只允许 `SYSTEM` / `Administrators`，普通 staging 只允许 `SYSTEM` / owner；安全描述符、目录创建、canonical 校验或句柄获取任一步失败都会中止。正常 helper 仍以显式参数闭合完成或回滚；elevated 无参数启动不会扫描普通 `%TEMP%` 自动发现并执行恢复计划，token elevation 查询失败同样 fail closed。

Windows 定向测试已实际创建受保护 staging 和子文件：父目录 DACL 受保护并具有预期 `OICI`，子文件只保留受限主体，当前进程仍可正常读写；target lease 也通过跨进程排他测试。该证明覆盖本次实现边界，但不替代 Authenticode 或离线签名信任根。

## 产品经理与客户视角

### 核心产品承诺

用户购买的不是“瞬时切换”，而是“我知道现在正在做什么，失败时不会覆盖已有会话”。高频 switch/sync 的安全边界不是复制全部 JSONL，而是窄 config/SQLite 检查点加零 in-place 的完整文件发布协议；真正会删除或覆盖完整数据的 hard delete、手工 Full 和 restore safety 仍必须覆盖四库与 active/archived 会话正文。正确优化顺序是：

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

- v0.2.1 已将下载连接/总超时调整为 30 秒/10 分钟并补 helper kill + wait、cleanup retry；发布前必须完成真实 v0.2.0 首跳验证，之后版本才能受益于新超时。
- 在受限 DACL 已验证的基础上继续收紧 helper/asset 的不可变句柄与替换时身份复检；无法证明时保持 elevated self-update fail closed。
- 引入 Windows Authenticode 签名；签名前 Release 必须同时发布 SHA-256 并明确“未签名”。
- 固定依赖版本、保留 lockfile，建立依赖漏洞的可利用性审查记录。

### v0.3: 更快但不削弱恢复能力

- 研究 Full/硬删除/restore safety 的内容寻址、分块或增量加密备份，目标是减少真正持久恢复点的重复 GiB payload；高频 switch/sync 已不再复制会话正文。
- SQLite 继续使用 Online Backup；不能用普通文件 copy 换速度。
- JSONL 去重方案必须考虑 DPAPI 密文非确定性，不能把明文内容哈希暴露到公共日志。
- 任何增量方案都必须能从单个 manifest 验证并完整恢复，不能依赖猜测目录状态。

## 发布建议

本节是 PR 前发布建议。G11 已按上方发布后 addendum 完成，`v0.2.0` 已发布；二次审计发现的补丁项必须在 `v0.2.1` 独立通过本地、PR/main/tag CI、Release 重下载和隔离一键更新闭环后，才能宣告本轮产品目标完成。发布说明应明确：

- 已修复 UI 卡死和切后全量历史触碰；
- changed switch 固定 `RuntimeState + StateOnly`、普通 sync 固定双 `StateOnly`，会话差异通过零 in-place `Allow` 与完整文件原子发布处理，不再扩大为 Runtime/Sessions；
- hot shared→current 的既有 thread 在 target candidate 文件处理前直接跳过，零复制、零 orphan，避免重复同步增加 C 盘会话文件；
- 任务轨道显示真实阶段，不是估算百分比；
- 不再使用系统确认弹窗；
- Relay 验证失败只进入一个页面错误面，不再重复显示；
- 自动清理成功/完整回滚、typed prewrite 和恢复可见临时点；显式 cleanup 的 plan/execute 都复检完整 SHA；Apply/回滚/日志失败、孤儿与无证据目录保留，且不按年龄/数量/mtime prune；
- cleanup 的 `attemptedCount` / `failedCount` 区分实际执行失败与安全保留 warning；真实受控 UI 已从 21 目录/6,327,089,609 bytes 清到 17 目录/2,693,977,957 bytes，4/4 删除、回收 3,633,111,652 bytes，剩余 17 项不自动删；旧候选产生的 partial/Failed 历史记录不改写；
- Full/Sessions 与 hard delete 覆盖 `archived_sessions/`；列表/删除使用同一强校验，extra/hash drift 不展示 verified，按需最多 256 个已验证 Full 可页面内显式管理，手工/hard-delete/restore-safety Full 不自动删；
- v0.2.1 的 30 秒连接/10 分钟下载超时只影响升级后的下载器，必须单列真实 `v0.2.0 -> v0.2.1` 首跳 smoke；
- 发布资产使用固定 profile 与 pinned UPX copy-only pipeline，CI 只上传 packed `codex-switch.exe`；本地 `2,228,224` bytes / `4DDC…CED7A` 不能冒充最终 tag-CI/Release 资产；
- EXE 是否完成 Authenticode 签名。
