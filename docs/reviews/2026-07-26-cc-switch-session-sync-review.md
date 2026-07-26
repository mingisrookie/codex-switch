# CC-Switch 会话同步机制审查

日期：2026-07-26

## 审查对象

- 官方仓库：[farion1231/cc-switch](https://github.com/farion1231/cc-switch)
- 源码审查 commit：[`878c26f31e012ba32b9772bd080bd4fa9e7d495e`](https://github.com/farion1231/cc-switch/commit/878c26f31e012ba32b9772bd080bd4fa9e7d495e)
- 关键文件：[codex_history_migration.rs](https://github.com/farion1231/cc-switch/blob/878c26f31e012ba32b9772bd080bd4fa9e7d495e/src-tauri/src/codex_history_migration.rs)
- 审查时最新正式 Release：[CC Switch v3.18.0](https://github.com/farion1231/cc-switch/releases/tag/v3.18.0)

源码 commit 日期晚于 `v3.18.0` 的发布时间。本审查用 commit 固定源码行为，用 Release URL 说明当时的正式版本背景，不主张该 commit 与 `v3.18.0` tag 完全相同。

## 结论

### 2026-07-26 Remote 可见性更正

手机 Remote 与隔离 `codex app-server thread/list` 实测证明：关闭态只更新 SQLite provider 不足以恢复历史，JSONL 首条 `session_meta.payload.model_provider` 也参与默认枚举过滤。所以下方“每次切换只落 SQLite”的原始合同已被实测推翻。

当前合同改为：hot shared→current 继续不触碰既有 live thread；ChatGPT 已关闭的运行态切换若发现 provider 不一致，则保留旧 JSONL，原子发布 provider 已归一且 Remote 可识别的完整 rollout，并更新 SQLite 活动路径；相同内容/provider 直接复用。CC-Switch 的 source 稳定复检、备份账本和只改匹配记录原则仍值得借鉴，但不能再据此排除关闭态 provider rollout。

CC-Switch 值得借鉴的是迁移纪律，不是把它的 provider bucket 算法直接搬进每次运行态切换。

它把历史统一设计成一次性、显式 opt-in、目录绑定、有 marker、有备份账本的迁移；只修改真正匹配来源 provider 的记录，并在写 JSONL 前复检 mtime 和长度。SQLite 使用 Online Backup 后再在 transaction 中更新匹配行。这些原则适合任何高风险会话迁移。

但 CC-Switch 解决的是“把既有 provider bucket 搬到统一 bucket”，本项目解决的是“current Home 和 shared-sessions 双向合并，并在账号态和 relay 态之间切换”。如果把 CC-Switch 的整文件 provider rewrite 放进每次切换，会重新引入本次已定位的性能回归。

因此本项目的明确合同是：

> 每次运行态切换不得仅因 provider 变化而重写未变化 JSONL。已有等价 JSONL 保持内容与 mtime；provider 路由更新落在 SQLite。只有新增、增长替换或实际复制的 JSONL 才允许必要写入。

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

本项目中，ChatGPT 的可见 provider 路由可以通过 `state_5.sqlite.threads.model_provider` 更新；未变化的 JSONL 正文没有必要仅为了保持字段完全同值而改变 mtime。

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
| 内容相等 | 不复制，不重写 | 只更新 SQLite 行 |
| target 严格扩展 source | 保留 target | 只更新 SQLite 行 |
| source 严格扩展 target，且关闭态允许替换 | 原子复制增长版本 | 复制后按目标 runtime 归一新文件的 `session_meta` |
| 内容分叉 | 保留当前活动 target；source 按内容哈希保存独立副本 | 只处理实际新建副本 |
| target 不存在 | 原子复制 source | 复制后按目标 runtime 归一新文件 |

`sync_sessions_in_transaction` 只有在 `RolloutCopy.copied` 为真时调用 `rewrite_session_metadata_provider`。已有 thread 的 SQLite provider 由 `update_existing_thread` 更新，JSONL 不需要跟随每次 runtime 切换重写。

### 已有回归证明

- `hot_sync_does_not_rewrite_an_existing_live_jsonl_for_provider_changes`
  - 热同步不为 provider 变化修改 live JSONL。
- `provider_switch_updates_sqlite_without_rewriting_unchanged_existing_jsonl`
  - 相等 JSONL 的 bytes 和 mtime 均保持不变；
  - `copied_session_files == 0`；
  - SQLite provider 更新为目标值。
- `updates_a_stale_target_when_the_source_is_a_strictly_growing_jsonl`
  - 只有严格增长时才替换旧版本。
- `divergent_versions_use_content_hashes_instead_of_one_stale_imported_file`
  - 内容分叉保留独立副本，不静默覆盖。
- `runtime_switcher` 的切换测试同时断言旧 rollout bytes 与 mtime 不变。

## 为什么切换时不重写未变化 JSONL

### 性能

问题基线中，单次切换为了 provider 变化重写了 532 个 JSONL、约 876.43 MiB。这个成本与“本次真实变化”无关，只随累计历史增长。

### ChatGPT 索引压力

文件内容即使只改变一个元数据字段，mtime 和文件身份变化仍可能被 ChatGPT、索引器、杀毒软件或备份软件视为大批历史更新。诊断中 ChatGPT 在最后一次批量重写后约 5.6 秒启动，批量 mtime 变化是切后卡顿的最强本地解释。

### 故障面

每多写一个文件，就多一个磁盘满、权限、并发修改、杀毒拦截和回滚恢复点。高频切换不应把数百个与本次选择无关的历史文件纳入写集合。

### 语义

会话正文是否完整与 runtime provider 是两个不同维度。SQLite 负责当前索引和 provider 可见性；JSONL provider 只在文件首次导入目标根时归一即可。比较历史完整性时应忽略 provider 字段，避免把相同正文误判为冲突。

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

## 发布验收

`v0.2.0` 至少应证明：

- 连续 account -> relay -> account 切换后，预存等价 JSONL bytes 与 mtime 不变；
- SQLite `threads.model_provider` 与目标 runtime 匹配；
- 新增或严格增长 JSONL 能正确复制；
- 分叉 JSONL 不覆盖当前活动文件；
- provider 字段差异不造成重复 `-imported-*` 嵌套；
- 切换后的前端刷新不调用会话和备份全量扫描；
- 所有验证使用临时 Home，不操作承载当前任务的真实 ChatGPT Home。

## 最终取舍

借鉴 CC-Switch 的选择性、账本、并发复检、Online Backup 和事务纪律；不照搬其一次性整文件 provider migration 到高频 runtime switch。

本项目更合适的路径是：SQLite 更新 provider，JSONL 按内容关系增量合并，无变化文件零写入。这个策略既吸收了 CC-Switch 的安全原则，也直接解决了本项目已观测到的切后性能问题。
