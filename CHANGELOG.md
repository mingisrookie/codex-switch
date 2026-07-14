# Changelog

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

- 新增最近已验证备份列表和按 `sourceRoot` 恢复入口；列表只对最近 5 个候选做 payload 大小/SHA-256 强校验，恢复时再次强校验，并对 SQLite 执行 `PRAGMA quick_check`。
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
