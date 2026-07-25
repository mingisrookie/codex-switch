# ChatGPT Switch 产品与可靠性审查

日期：2026-07-26

## 结论

当前分支已经把问题从“长时间无响应的黑盒切换”改造成“后台执行、页面内展示真实阶段、失败可归因”的任务流，并针对切换后的磁盘争用修复了两个主要放大器：无变化历史 JSONL 的批量重写，以及切换结束后的全量会话和备份扫描。

本审查的本地候选结论是 **GO for PR**。完整前端/Rust 门禁、隔离临时目录回归、三视口真实渲染、依赖与文本审计、elevated/non-elevated updater 安全测试以及 Windows release 产物合同均已通过。`v0.2.0` 仍不能在远端交付闭环完成前发布：PR/CI、合并、tag CI、Release 重下载校验和真实 `v0.1.9 -> v0.2.0` 一键更新 smoke 是最后的阻断门禁。

这里的“已解决”表示当前工作树已有对应实现并通过本地门禁，不表示 GitHub Release 已经包含这些变化。五处版本 manifest 已统一为 `0.2.0`；本地产物仅用于证明构建链，最终发布结论仍以合并后的 commit、tag CI artifact 和重新下载的 Release 资产为准。

## 审查依据

- 运行证据记录在 [Trellis runtime switch evidence](../../.trellis/tasks/07-25-fix-relay-switch-hang/research/2026-07-25-runtime-switch-evidence.md)。
- 当前主链路为 [App.tsx](../../src/App.tsx) -> [api.ts](../../src/api.ts) -> [commands.rs](../../src-tauri/src/commands.rs) -> [runtime_switcher.rs](../../src-tauri/src/runtime_switcher.rs)。
- 会话合并策略位于 [session_sync.rs](../../src-tauri/src/session_sync.rs)，备份与恢复合同位于 [backup.rs](../../src-tauri/src/backup.rs)。
- Windows 进程边界位于 [process_control.rs](../../src-tauri/src/process_control.rs)。
- 凭据绑定与回滚分别位于 [runtime_store.rs](../../src-tauri/src/runtime_store.rs) 和 [skill_manager.rs](../../src-tauri/src/skill_manager.rs)。
- GitHub [PR #2](https://github.com/mingisrookie/codex-switch/pull/2) 审查对象为 head commit [`8c026f61fcaafc2e7f398a769c4afa743ec470cc`](https://github.com/marvellam/codex-switch/commit/8c026f61fcaafc2e7f398a769c4afa743ec470cc)。

## 问题基线

| 用户感知 | 已捕获证据 | 根因判断 | v0.2 成功信号 |
| --- | --- | --- | --- |
| 点击切换后像卡死 | 修复前 Windows WER 有两次 `AppHangB1` | 同步 Tauri command 在 UI 敏感执行路径中做网络、DPAPI、SQLite 和大量文件 I/O | 切换在 blocking worker 中执行，窗口持续响应，开始后立即出现真实任务状态 |
| 一次成功切换仍耗时 137.72 秒 | current 备份约 921.88 MiB / 57 秒，shared 备份约 480.86 MiB / 29.5 秒 | 完整安全快照本身成本高，但过去没有状态反馈 | 阶段和耗时可见，长任务不等同于应用无响应 |
| 切换后 ChatGPT 明显变卡 | 532 个 JSONL、约 876.43 MiB 在切换尾段被重写；约 5.6 秒后启动 ChatGPT | 只为 provider 变化触碰全部历史文件，可能触发 ChatGPT 全量重索引 | 无变化 JSONL 的内容与 mtime 保持不变，只更新必要 SQLite 行和真正复制的文件 |
| 切换完成后仍有额外磁盘压力 | 备份目录约 3.879 GiB，旧前端会立刻做完整 Dashboard 刷新 | 会话、管理列表和备份校验在 mutation 后同步重扫 | 切换后仅刷新运行态域；会话和备份按需加载 |
| 弹窗和控制台闪窗造成困惑 | 旧生产 UI 使用浏览器确认框、prompt、原生 dialog；Windows 子进程未隐藏窗口 | 操作授权与状态被拆散在系统级 UI 中 | 所有授权、配置、进度和错误都留在页面内，Windows helper 无可见控制台 |

上述数字是问题诊断基线，不是新版本性能结果。最终发布必须重新采样，不能直接用基线证明修复有效。

## 当前已解决

### 产品与客户体验

1. **点击即进入任务态**
   - `App.handleSwitch` 在调用后端前同步创建 `switchFlow`。
   - [RuntimeSwitchProgressPanel.tsx](../../src/RuntimeSwitchProgressPanel.tsx) 只消费后端 `RuntimeSwitchProgress` 阶段，不伪造连续百分比。
   - 任务面板位于页面分支之外，切换导航页不会丢失当前任务。
   - 成功、写入前失败、已回滚和回滚失败有不同终态文案。

2. **不再依赖系统弹窗**
   - 账号覆盖、会话 dry-run、备份恢复、会话删除和 Skill 配置改为页面内二阶段确认或展开面板。
   - 名称仍为 `RelayRuntimeDialog` 的兼容组件实际渲染 `<section>`，不是原生 `<dialog>`。
   - 切换进行中拦截窗口关闭请求，但不弹出确认框。

3. **用户可见品牌改为 ChatGPT**
   - 主控制台、账号态、会话加载和任务步骤使用 ChatGPT 文案。
   - `.codex`、`CODEX_HOME`、`codexHome`、`plus` 和 Tauri command 名属于兼容合同，继续保留。
   - 仓库名、包名和 README 仍需在发布门禁中统一审阅，不能把内部兼容标识机械替换掉。

4. **界面语言统一**
   - 生产 React 组件统一从 `lucide-react` 引入图标。
   - 当前生产源码扫描没有 emoji、手写 SVG、`window.confirm`、`window.prompt` 或原生 `<dialog>`。
   - 新样式采用低圆角、中性色基础与 teal/amber/red/blue 状态色，不使用装饰性渐变球体。

### 可靠性与数据安全

1. **阻塞工作移出 UI 路径**
   - `commands.rs::switch_runtime` 是 async wrapper，完整 mutation 在 `spawn_blocking` worker 中执行。
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
   - current 和 shared 在写入前分别创建完整、已校验、DPAPI 加密快照。
   - SQLite 使用 Online Backup，而不是直接复制可能处于 WAL 状态的数据库。
   - manifest 校验 payload 路径、大小、SHA-256 和加密标志。
   - 写入失败后同时恢复 current/shared；前端用 typed outcome 区分回滚结果。

5. **凭据绑定到服务来源**
   - relay 或 Skill 的 URL origin 改变时，空 Key 不再继承旧来源凭据。
   - 同 origin 只修改 path 或 model 时可保留既有 Key。
   - runtime 和 Skill 配置的成对写入失败会恢复并验证旧文件；回滚失败会明确升级错误。
   - 远程 relay 禁止明文 HTTP，只有 loopback HTTP 例外。

6. **破坏性路径 fail closed**
   - `CODEX_HOME` 和 `sqlite_home` 必须是绝对路径，拒绝 `.`、`..` 和相对配置。
   - backup/current/shared 根必须互不相同且互不包含。
   - 会话硬删除同时清理 `state_5.sqlite`、`goals_1.sqlite`、`memories_1.sqlite` 和可识别的 `logs_2.sqlite` thread 关联，并在事务中验证目标行已删除。

### 性能

1. **切换不再批量触碰未变化历史**
   - `session_sync.rs::copy_rollout_file` 对相等文件不复制。
   - provider 变化可以更新 SQLite `threads.model_provider`，不要求重写已经存在且正文相同的 JSONL。
   - 只有真正复制、新建或增长替换的目标文件才做一次 `session_meta.payload.model_provider` 归一。
   - `provider_switch_updates_sqlite_without_rewriting_unchanged_existing_jsonl` 明确断言内容和 mtime 不变。

2. **昂贵 Dashboard 域按需加载**
   - `loadRuntimeDashboard` 只读 Home 摘要、runtime、active status 和最近操作。
   - `loadSessionDashboard` 只在进入会话页或明确刷新时扫描。
   - `loadBackupDashboard` 只在用户点击加载备份时执行校验。
   - 切换成功后刷新 runtime 域并标记 session/backup stale，不再立即读取 GiB 级历史和备份。

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

没有吸收自动 retention。后续实现必须先有显式 `operationGroupId + sourceRoot + verified` 账本，再按完整操作组保留，而不是从路径数量猜测。

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
| G11 交付闭环 | PR CI 绿并合并；tag `v0.2.0`；Release 资产重下载校验；两个 live GitHub 合同测试；真实 v0.1.9 一键更新；PR #2 留说明并关闭 | 待远端执行，仍是发布阻断 |
| G12 文本卫生 | 生产源码无 emoji、native dialog、手写 SVG、乱码和异常替换字符；`git diff --check` 通过 | 通过 |

### elevated self-update 为什么仍是发布门禁

updater 会把 helper、更新资产和 plan 放在系统临时目录，再由 helper 替换安装位置。高完整性进程若使用普通低权限可写 staging，会形成不应接受的权限边界。

当前源码已经增加 `process_is_elevated`、系统 CSPRNG 随机目录名、创建时受限 DACL 和 `StagingGuard`。elevated staging 的 ACL 只允许 `SYSTEM` / `Administrators`，普通 staging 只允许 `SYSTEM` / owner；安全描述符、目录创建、canonical 校验或句柄获取任一步失败都会中止。正常 helper 仍以显式参数闭合完成或回滚；elevated 无参数启动不会扫描普通 `%TEMP%` 自动发现并执行恢复计划，token elevation 查询失败同样 fail closed。

Windows 定向测试已实际创建受保护 staging 和子文件：父目录 DACL 受保护并具有预期 `OICI`，子文件只保留受限主体，当前进程仍可正常读写；target lease 也通过跨进程排他测试。该证明覆盖本次实现边界，但不替代 Authenticode 或离线签名信任根。

## 产品经理与客户视角

### 核心产品承诺

用户购买的不是“瞬时切换”，而是“我知道现在正在做什么，失败时本次写集仍能恢复”。因此覆盖实际写集的可验证快照不能为了表面速度被静默移除；manifest v3 应用 `scope` / `trackedDatabases` 排除无关 I/O，而 hard delete、手工 full backup 和 restore safety 仍覆盖四库。正确优化顺序是：

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

- 完整备份仍可能需要几十秒。当前只有阶段和总耗时，没有字节进度或阶段内 ETA。
- 切换点击即授权关闭 ChatGPT，这符合无弹窗目标，但按钮附近应持续保留简短、非弹窗式的行为说明。
- 切换期间禁止关闭窗口但没有取消。进入写入阶段后不应提供强制取消；未来只能在任何写入前提供安全取消。
- 仓库和可执行资产仍使用 `codex-switch` 兼容名，而界面叫 ChatGPT Switch。短期保留可避免破坏 updater URL 和用户路径，但发布说明必须解释这是兼容命名。
- 自动 retention 暂缓后，长期使用仍会增长备份目录。容量预检防止本次操作填满磁盘，但不等于生命周期管理。

## 后续路线

### v0.2.1: 可证明的备份生命周期

- 为每次 mutation 写入不可歧义的 `operationGroupId`。
- 每个快照记录 `sourceRootKind`、canonical source root、完成状态和 verify 状态。
- 只有 current/shared 两个预期来源均存在且校验成功，才能把该组标为可恢复完整组。
- 至少保留最近完整成功组、最近失败/回滚组和用户 pin 的组。
- 删除前再次校验保护集；保护集不完整时 fail closed，不执行任何 prune。
- UI 展示预计释放空间、保留原因和清理结果，不做静默清理。

### v0.2.1: 性能可观测性

- 在 operation record 中记录每阶段开始、结束、输入文件数和字节数。
- 只保存本机脱敏统计，不记录 API Key、会话正文或 relay 响应。
- 增加“导出诊断摘要”，默认只含版本、阶段耗时、错误类别和计数。
- 用固定的合成大 Home 做基准，防止再次出现全量 JSONL mtime 抖动。

### v0.2.x: updater 与供应链

- 在受限 DACL 已验证的基础上继续收紧 helper/asset 的不可变句柄与替换时身份复检；无法证明时保持 elevated self-update fail closed。
- 引入 Windows Authenticode 签名；签名前 Release 必须同时发布 SHA-256 并明确“未签名”。
- 固定依赖版本、保留 lockfile，建立依赖漏洞的可利用性审查记录。

### v0.3: 更快但不削弱恢复能力

- 研究内容寻址、分块或增量的加密备份，目标是减少重复 GiB 级 payload。
- SQLite 继续使用 Online Backup；不能用普通文件 copy 换速度。
- JSONL 去重方案必须考虑 DPAPI 密文非确定性，不能把明文内容哈希暴露到公共日志。
- 任何增量方案都必须能从单个 manifest 验证并完整恢复，不能依赖猜测目录状态。

## 发布建议

本地 G1-G10 与 G12 已完成，可以提交 PR；只有完成 G11 后才可以把 `v0.2.0` 宣告为已发布。发布说明应明确：

- 已修复 UI 卡死和切后全量历史触碰；
- 切换仍会创建完整安全快照，因此大 Home 的总耗时不会变成零；
- 任务轨道显示真实阶段，不是估算百分比；
- 不再使用系统确认弹窗；
- 备份自动清理尚未启用；
- EXE 是否完成 Authenticode 签名。
