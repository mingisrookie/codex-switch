# CC-Switch 会话同步机制审查

日期：2026-07-26

## 审查对象

- 官方仓库：[farion1231/cc-switch](https://github.com/farion1231/cc-switch)
- 源码审查 commit：[`878c26f31e012ba32b9772bd080bd4fa9e7d495e`](https://github.com/farion1231/cc-switch/commit/878c26f31e012ba32b9772bd080bd4fa9e7d495e)
- 关键文件：[codex_history_migration.rs](https://github.com/farion1231/cc-switch/blob/878c26f31e012ba32b9772bd080bd4fa9e7d495e/src-tauri/src/codex_history_migration.rs)
- 审查时最新正式 Release：[CC Switch v3.18.0](https://github.com/farion1231/cc-switch/releases/tag/v3.18.0)

源码 commit 日期晚于 `v3.18.0` 的发布时间。本审查用 commit 固定源码行为，用 Release URL 说明当时的正式版本背景，不主张该 commit 与 `v3.18.0` tag 完全相同。

## 结论

CC-Switch 值得借鉴的是迁移纪律，不是把它的 provider bucket 算法直接搬进每次运行态切换。

它把历史统一设计成一次性、显式 opt-in、目录绑定、有 marker、有备份账本的迁移；只修改真正匹配来源 provider 的记录，并在写 JSONL 前复检 mtime 和长度。SQLite 使用 Online Backup 后再在 transaction 中更新匹配行。这些原则适合任何高风险会话迁移。

但 CC-Switch 解决的是“把既有 provider bucket 搬到统一 bucket”，本项目解决的是“current Home 和 shared-sessions 双向合并，并在账号态和 relay 态之间切换”。如果把 CC-Switch 的整文件 provider rewrite 放进每次切换，会重新引入本次已定位的性能回归。

因此本项目的明确合同是：

> 每次运行态切换不得覆盖任何既有 JSONL。热同步的 live existing thread 保持零文件 I/O；关闭态先按完整度选出活动历史，再在最多 32 个受管候选中复用完整 Remote provider 槽位，或发布带严格 provenance marker 的 immutable successor。SQLite 只在完整新槽位发布后改指它；旧工具槽位只有在 durable terminal、两库均无引用且 successor 完整包含 predecessor 时才可回收，证据不足就保留。

## v0.2.2 candidate 补充：哪些借、哪些不移植

当前 Remote provider 可见性修复继续借用 CC-Switch 的**选择性更新和可证明账本**理念，但没有移植它的单 Home 整文件 migration：

| CC-Switch 原则 | 本项目当前落点 |
| --- | --- |
| 无匹配即无写 | hot shared→current existing thread 在任何 target candidate I/O 前跳过；closed provider 路径先在 32 个候选中复用完整槽位 |
| 写前稳定性复检 | source 完整尾行/session ID/length/SHA-256 前后复检且仅有界重试；新 provider 槽位用 16 KiB 上限 marker 的 `createdBytes` 前缀 SHA-256 证明所有权 |
| Online Backup + transaction | 窄 checkpoint 继续用 SQLite Online Backup；SQLite provider/path 在目标 transaction 中更新 |
| 恢复依赖账本，不猜来源 | checkpoint cleanup 只接受与唯一 terminal record 全量匹配的 bound BackupManifest v4；未绑定 v2/v3 自动点保留。provider predecessor GC 只接受完整 successor 与两库无引用的实证 |
| 大范围迁移显式 opt-in | 普通 runtime switch 不扫描/重写全部旧历史；未来统一旧历史仍需独立工具 |

不直接移植：

- 不移植每次运行态切换对匹配 JSONL 的整文件原地 rewrite；Remote provider 通过旁路 immutable successor 解决，任何既有 JSONL 都不覆盖。
- 不移植单根 provider bucket 数据模型；本项目必须同时处理 current/shared、外置 `sqlite_home`、完整度/分叉、index 策略和双根补偿。
- 不把一次性 migration marker 当持续同步游标；本项目自己的 provider marker 只证明固定 `createdBytes` 前缀、thread/provider/文件名与 origin，不授权覆盖或按年龄删除。
- 不把 CC-Switch 的归档处理等同本项目普通同步：当前 provider 槽位只处理 active `sessions/`，`archived_sessions/` 仍只进入 Full/硬删除/管理边界。

仍存在的真实风险：

- source TOCTOU 只能通过执行期稳定性复检、no-clobber/原子发布和 fail closed 缩小，不能形成外部 ChatGPT 共同参与的单一事务。
- SQLite、JSONL 与 `session_index.jsonl` 仍不是一个 durable transaction；失败时可能保留完整但未引用的工具文件，不能按 mtime/年龄反向猜测删除。
- 首次为新 provider 生成槽位仍可能新增一份完整 JSONL 与 marker；当前先做 pre-close 保守容量初筛，关闭 ChatGPT 后以权威写集再次按卷校验。容量是保守上界，执行则在第一份 checkpoint 前和 live 写入前要求权威写集精确不变；晚期漂移至多留下 typed prewrite checkpoint，不产生 live mutation。合法增长不会覆盖旧槽位，而是只写最终 provider successor；真正 Divergent 为避免数据丢失可额外保留 raw 分支。旧 predecessor 只能在 durable terminal、两库无引用且 successor 完整包含它时回收。
- `SessionSyncResult` 以实际 JSONL 与实际 marker 序列化长度统计持久会话 `added`，以证明式 GC 的 JSONL+marker 统计 `reclaimed`，前端派生 `net`；这些字段与 transient `checkpointCleanup.reclaimedBytes` 分开，不能把临时点释放计入会话净变化。
- 普通 switch/sync 的窄 transient checkpoint 只在 durable terminal 落盘且证据重新通过后清理；manual Full、hard-delete Full 与 restore-safety 是持久恢复点，继续保留并由用户显式管理。
- `archived_sessions/` 的 provider 可见性统一不在当前任务；把它加入普通切换前需要独立产品语义、容量和恢复合同。

## CC-Switch 实际做法

### 1. 迁移是显式、一次性且绑定目录

[L190-L272](https://github.com/farion1231/cc-switch/blob/878c26f31e012ba32b9772bd080bd4fa9e7d495e/src-tauri/src/codex_history_migration.rs#L190-L272) 的官方历史统一迁移有以下前置条件：

- 用户已经开启统一会话；
- 用户明确选择迁入既有官方会话；
- 当前 Codex 目录尚未记录完成 marker；
- live config 已经真实路由到目标 `custom` bucket；
- 操作持有专用锁。

marker 绑定 canonical Codex directory。用户切换目录后，旧目录 marker 不会阻止新目录迁移。迁移期间开关被关闭时，条件写 marker 会失败，避免把未完成或已失效的意愿记录成成功。

这是“一次性数据迁移”语义，而不是“每次 provider 切换都全量修正历史”。

### 2. 只改真正匹配 provider 的 JSONL

[L1011-L1073](https://github.com/farion1231/cc-switch/blob/878c26f31e012ba32b9772bd080bd4fa9e7d495e/src-tauri/src/codex_history_migration.rs#L1011-L1073) 先读取文件，再逐行调用 provider 匹配函数：

- 没有匹配行时直接返回 `false`，不备份、不写回；
- 有匹配行时记录初始 mtime 和 length；
- 备份前复检一次；
- 备份后、原子写入前再复检一次；
- 文件被并发修改时中止，而不是覆盖新数据。

这个“无变化即无写入”的原则应当保留。

### 3. SQLite 先 Online Backup，再事务更新

[L1101-L1169](https://github.com/farion1231/cc-switch/blob/878c26f31e012ba32b9772bd080bd4fa9e7d495e/src-tauri/src/codex_history_migration.rs#L1101-L1169) 的状态库迁移：

- 先检查数据库、`threads` 表和 `model_provider` 列；
- 先计数，零匹配时不产生备份或写入；
- 有匹配行时先调用 Online Backup；
- 只对来源 provider 集合中的行执行 `UPDATE`；
- 在 transaction 中提交。

[L1178-L1209](https://github.com/farion1231/cc-switch/blob/878c26f31e012ba32b9772bd080bd4fa9e7d495e/src-tauri/src/codex_history_migration.rs#L1178-L1209) 显示 JSONL 与 SQLite 分别进入账本目录，SQLite 通过 `rusqlite::backup::Backup::run_to_completion` 生成一致快照。

### 4. 反向恢复依赖账本，不猜来源

同一文件中的恢复逻辑只把备份里能够证明原先属于 official provider 的会话恢复回去。它不会尝试从已混合的 `custom` bucket 反推历史来源。这个限制是正确的：provider 已混合后，仅凭当前状态无法可靠恢复归属。

## 可以借鉴

| 机制 | 为什么有价值 | 本项目采用方式 |
| --- | --- | --- |
| 显式 opt-in | 大范围存量迁移不应由普通切换隐式触发 | 如果未来提供“统一旧历史”工具，必须是独立动作，不放入运行态切换 |
| 目录绑定 marker | 避免切换 Home 后错误复用完成状态 | marker 必须包含 canonical source root 和 schema version |
| 只选择匹配记录 | 降低写放大和误改风险 | 只更新目标 provider 的 SQLite 行；JSONL 仅在真正复制时归一 |
| 无变化不写 | 保留 mtime，避免触发索引器和备份软件 | 当前相等 JSONL 不 copy、不 rewrite |
| 写前并发复检 | 防止用户重新打开 ChatGPT 后覆盖新内容 | mutation 前后复检进程；未来一次性迁移还应复用 mtime + size 检查 |
| SQLite Online Backup | 对 WAL 数据库提供一致快照 | 当前 scoped/full 备份按 `trackedDatabases` 继续使用 SQLite Online Backup |
| transaction 更新 | 中途失败不会留下部分 SQLite 行 | 会话导入和 provider 更新继续在 target transaction 中执行 |
| 精确恢复账本 | 回滚依据事实，不依据当前 bucket 猜测 | snapshot manifest 保留 source root、payload hash 和完整性标志 |
| 条件写 marker | 操作期间用户意愿变化时不误记成功 | 未来迁移 marker 与开关状态共用锁和条件写 |

## 不应照搬

### 1. 不把一次性 bucket migration 变成每次切换步骤

CC-Switch 的 rewrite 针对明确的存量迁移，完成后由 marker 阻止重复执行。本项目的 runtime switch 是高频操作。如果每次切换都扫描并重写匹配的旧 JSONL，成本会随历史线性增长。

### 2. 不为 provider 字段重写等价正文

CC-Switch 对实际匹配文件仍会整文件读入内存并整体原子写回。这在一次性迁移中可以接受，但不适合本项目已经出现接近 GiB 历史的高频切换。

本项目中，热同步的既有 live thread 不应只为 provider 改写 JSONL；关闭态 Remote 过滤又要求 SQLite 与活动 JSONL provider 一致，因此采用受管旁路槽位，而不是修改原文件。这样既满足 Remote 文件名/provider 合同，也不改变原 JSONL 的 mtime。

### 3. 不复用 CC-Switch 的单根数据模型

CC-Switch 主要围绕一个 Codex config directory 和 provider bucket 工作。本项目有：

- current ChatGPT Home；
- shared-sessions；
- 外置 `sqlite_home` 的可能性；
- 双向合并；
- 严格增长与内容分叉；
- runtime 文件切换；
- 两根快照与双根回滚。

直接搬运单根迁移流程会丢失跨根一致性和冲突语义。

### 4. 不把 marker 当成同步游标

marker 适合一次性 schema/data migration，不适合持续双向同步。持续同步必须按 thread ID、JSONL 内容关系、SQLite 行和目标实际状态幂等判断。

## 本项目当前策略

实现位于 [session_sync.rs](../../src-tauri/src/session_sync.rs) 和 [runtime_switcher.rs](../../src-tauri/src/runtime_switcher.rs)。

### JSONL 决策表

| source 与 target 关系 | 行为 | provider 处理 |
| --- | --- | --- |
| hot existing（任意关系） | `PreserveExisting` 在候选 I/O 前跳过 | SQLite/JSONL 均不改 |
| closed 内容相等或 target 更长 | `SelectMostComplete` 保留完整活动文件 | provider 已匹配且 Remote 名合法则复用；否则在 32 个候选中先找完整 provider 槽位 |
| closed source 严格扩展 target | 先选择更完整 source | 只发布最终 provider immutable successor + marker，不覆盖旧槽位、不额外写 raw/provider 双份 |
| closed 内容真实 Divergent | 保留 current 活动分支；独立保存完整分叉 | 为避免数据丢失可单独保留 raw 分支，再按有界候选发布需要的 provider 槽位；不覆盖现有或未知槽位 |
| target 不存在 | 目标 provider 已知时直接原子 no-clobber 发布最终 provider 文件 | 不先落 raw 副本再生成 provider 副本 |

`sync_sessions_in_transaction` 先执行完整度选择，再由 provider-aware 路径对最终来源规划槽位。槽位名符合 Remote 的 `rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl`，由 thread/provider/sequence 稳定决定；严格 UUID 与 canonical containment 在任何 create 前验证。规划器最多检查 32 个候选：遇到 `Equal` 或完整覆盖来源的既有槽位就复用，扫描时只记首个空位，只有有界扫描结束仍无完整槽位时才使用该空位。每个新文件配套原子创建不超过 16 KiB 的 sidecar provenance marker，记录 `createdBytes` 与该前缀 SHA-256；创建后合法 append 不会使所有权失效，但前缀、marker、session ID、provider 或 containment 漂移都会 fail closed。

### 已有回归证明

- `hot_sync_does_not_rewrite_an_existing_live_jsonl_for_provider_changes`
  - 热同步不为 provider 变化修改 live JSONL。
- `provider_switch_publishes_a_stable_remote_rollout_without_rewriting_original_history`
  - 原 JSONL 的 bytes 和 mtime 保持不变；
  - 重跑复用同一完整 provider 槽位；
  - SQLite provider/path 与活动文件一致。
- `owned_marker_accepts_append_but_rejects_a_changed_created_prefix`
  - `createdBytes` 之后的合法 append 仍接受；
  - 创建前缀变化时 marker 不再证明工具所有权。
- `shared_long_current_short_plans_and_writes_only_the_final_provider_slot`
  - 首次/增长只落最终 provider 文件，不形成 raw/provider 双份。
- `owned_growth_uses_a_successor_and_gc_reclaims_only_an_unreferenced_stable_predecessor`
  - 增长创建 immutable successor；
  - durable terminal 后只有两库无引用且 successor 完整覆盖时才删除 predecessor。
- `provider_gc_retains_a_predecessor_appended_after_candidate_creation`、`provider_gc_fails_closed_when_a_candidate_marker_is_malformed` 与 `provider_gc_retains_a_predecessor_when_any_managed_database_still_references_it`
  - 清理计划后发生 append、marker 损坏或任一数据库仍引用时，旧槽位保留。
- `provider_slots_stay_bounded_across_growth_and_repeated_round_trips`
  - 连续 provider 往返的文件数在有界候选与证明式 GC 下保持有界。

## 为什么切换时不重写未变化 JSONL

### 性能

问题基线中，单次切换为了 provider 变化重写了 532 个 JSONL、约 876.43 MiB。这个成本与“本次真实变化”无关，只随累计历史增长。

### ChatGPT 索引压力

文件内容即使只改变一个元数据字段，mtime 和文件身份变化仍可能被 ChatGPT、索引器、杀毒软件或备份软件视为大批历史更新。诊断中 ChatGPT 在最后一次批量重写后约 5.6 秒启动，批量 mtime 变化是切后卡顿的最强本地解释。

### 故障面

每多写一个文件，就多一个磁盘满、权限、并发修改、杀毒拦截和回滚恢复点。高频切换不应把数百个与本次选择无关的历史文件纳入写集合。

### 语义

会话正文是否完整与 runtime provider 是两个不同维度。比较历史完整性时先忽略 provider 字段，避免把相同正文误判为冲突；活动历史确定后，Remote-visible 槽位再把 JSONL provider 与 SQLite provider/path 对齐。这样 provider 过滤不会倒逼原文件改写。

## 如果未来需要一次性历史归一

可以新增独立的“迁移旧历史 provider”工具，但必须满足：

1. 用户显式启用，不从普通切换自动触发。
2. 预览将修改的文件数、行数和字节数。
3. marker 包含 schema version、canonical Home、来源 provider 集和目标 provider。
4. 只选中实际匹配 provider 的 JSONL 和 SQLite 行。
5. JSONL 写前记录并两次复检 mtime + size；变化即中止该文件。
6. JSONL 和每个 SQLite 数据库分别进入可验证账本。
7. SQLite 使用 Online Backup，更新使用 transaction。
8. marker 仅在全部目标成功且用户意愿仍有效时写入。
9. 恢复严格依赖账本，不从混合后的 provider 猜来源。
10. 该迁移完成后，普通 runtime switch 仍不得扫描和重写无变化历史。

## v0.2.2 candidate 验收边界

当前开发分支进入 PR/发布门禁前至少应以真实测试证明；本节不填写尚未完成的
PR、CI、tag、Release 或资产 hash：

- 连续 account -> relay -> account 切换后，预存等价 JSONL bytes 与 mtime 不变；
- SQLite `threads.model_provider`、`rollout_path` 与活动 JSONL provider 同时匹配；
- 32 个候选中前空后完整时仍复用完整槽位；没有完整槽位时只使用扫描记录的首空位，全占时 fail closed；首次/严格增长只发布最终 provider successor + ≤16 KiB marker，不覆盖 predecessor、不形成 raw/provider 双份；
- marker 的 `createdBytes` 前缀 SHA 接受创建后 append，拒绝创建前缀改变或截断；
- 真正 Divergent 可保留独立 raw 分支，但不能覆盖或替换 current 活动历史；
- durable terminal 后只有完整 successor 且 current/shared 两库均不引用时才 GC predecessor，任一证据漂移都保留；
- pre-close 初始保守容量和 post-close 权威重算均覆盖 ordinary/provider rollout、marker、SQLite/index 与窄 checkpoint peak；晚期 plan 漂移不做 live mutation；
- UI 分开显示持久会话 added/reclaimed/net 与 transient checkpoint reclaimed；
- 切换后的前端刷新不调用会话和备份全量扫描；
- 所有验证使用临时 Home，不操作承载当前任务的真实 ChatGPT Home。

## 最终取舍

借鉴 CC-Switch 的选择性、账本、并发复检、Online Backup 和事务纪律；不照搬其一次性整文件 provider migration 到高频 runtime switch。

本项目更合适的路径是：hot existing 零写；closed 先按内容关系和完整度选择活动历史，再复用完整 provider 槽位或发布 immutable successor，让 SQLite 与活动 JSONL provider 满足 Remote 过滤；任何既有 JSONL 都不覆盖。marker 只证明创建前缀，旧槽位清理由 durable terminal、双库无引用和 successor 完整覆盖共同授权。这个策略既吸收了 CC-Switch 的选择性、账本、并发复检、Online Backup 和事务纪律，也避免把一次性 migration 变成高频全量 rewrite。
