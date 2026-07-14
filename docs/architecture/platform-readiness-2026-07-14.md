# Codex Switch 跨平台架构准备度审计

> 审计日期：2026-07-14
> 当前发布目标：Windows x64 便携 `codex-switch.exe`
> 目标：代码为未来 macOS/Linux 留出清晰边界，但本轮不构建或宣称支持非 Windows 平台。

## 结论

当前架构不是“换一个 target 就能完整运行”的全平台应用，但核心数据处理已有可复用基础。会话扫描、SQLite 合并、TOML patch、版本检查和大部分文件路径逻辑使用标准 Rust；真正阻塞 macOS/Linux 产品化的是凭据密钥库、进程控制、应用数据目录、跨进程锁和内置 Skill runtime。

本轮更新检查采用平台中立实现，不新增 Windows shell、`APPDATA` 或路径分隔符依赖。同步完成三项低成本边界收紧：

1. `windows-sys` 改为仅 Windows target 依赖。
2. 非 Windows 凭据加密不再使用可逆开发占位，改为明确拒绝，避免未来误发布形成假安全。
3. `process_control` 显式区分 Windows 实现和非 Windows unsupported 结果；release GUI subsystem 属性也只在 Windows 生效。

## 准备度矩阵

| 领域 | 当前状态 | 未来 macOS/Linux 所需工作 | 本轮处理 |
| --- | --- | --- | --- |
| 更新检查 | 平台中立 Rust HTTP + SemVer，Tauri opener 打开固定 Release 页 | 增加对应平台构建验证即可 | 已满足边界 |
| 会话扫描/同步 | `Path`、rusqlite、serde 和标准文件 API 为主 | 用非 Windows Codex 实际目录/schema 做合同验证 | 保持现状 |
| 原子文件替换 | Windows `MoveFileExW`，非 Windows 使用同文件系统 `rename` | 增加 macOS/Linux 崩溃与覆盖语义测试 | 已有平台分支 |
| 凭据/备份加密 | Windows DPAPI | macOS Keychain、Linux Secret Service 或统一安全密钥抽象；需要迁移格式和能力探测 | 非 Windows 明确拒绝 |
| Codex 进程控制 | Windows `tasklist` / `taskkill` | macOS/Linux 进程枚举、身份确认和受控终止实现 | 非 Windows 明确拒绝 |
| 应用数据目录 | 多处直接读取 `APPDATA` | 收敛到 `tauri::path::app_data_dir` 或注入式 `PlatformPaths` | 记录为迁移阻塞项 |
| mutation 跨进程锁 | Windows `OpenOptionsExt::share_mode(0)` | Unix advisory lock 或平台锁抽象，并验证崩溃释放 | 记录为迁移阻塞项 |
| 内置 Skill | PowerShell helper + DPAPI 配置 | 按 OS 提供 runtime/helper、密钥读取和包 manifest | 明确保留 Windows-only |
| UI 文案 | 多处明确写 Windows DPAPI | 由后端 capability 返回平台密钥库名称和可用性 | 记录为迁移项 |
| CI/发布 | `windows-latest`，单个 x64 EXE | macOS/Linux check/test/build matrix、图标/签名/包格式 | 本轮不扩展 |

## 证据位置

- `src-tauri/src/update_check.rs`：固定仓库、SemVer、响应大小上限、禁止重定向、稳定版校验和固定下载页。
- `src-tauri/src/crypto.rs`：Windows DPAPI 与非 Windows unsupported 边界。
- `src-tauri/src/process_control.rs`：Windows 进程控制边界。
- `src-tauri/src/file_ops.rs`：Windows write-through replace 与非 Windows rename。
- `src-tauri/src/commands.rs`、`profile_store.rs`、`runtime_store.rs`：当前 `APPDATA` 依赖。
- `src-tauri/src/skill_manager.rs`、`src-tauri/resources/skills/`：Windows/PowerShell Skill runtime。
- `.github/workflows/ci.yml`：当前仅 Windows 质量门禁。

## 推荐迁移顺序

### Phase 1：平台服务边界

- 新建 `PlatformPaths`，统一 Codex Home、app data、backup、shared sessions、operation log 和 Skill 配置目录。
- 把凭据保护、进程控制、跨进程锁定义为能力接口；Windows 实现保持当前语义。
- 后端状态返回平台 capability，UI 不再硬编码某个密钥库名称。

### Phase 2：非 Windows 安全实现

- macOS 接 Keychain，Linux 接 Secret Service；定义旧 DPAPI 数据在其他 OS 上不可解密的迁移提示。
- 实现并测试 macOS/Linux 进程检测、关闭和跨进程锁。
- 为 Image2/Grok Skill 提供对应 shell/runtime，或在 capability 不满足时明确禁用。

### Phase 3：真实目标验证与分发

- CI 增加 macOS/Linux 的 `cargo check`、测试和 Tauri 构建。
- 用真实 Codex Desktop/CLI 数据验证目录、SQLite schema、会话 JSONL 和进程名。
- 再决定 DMG/AppImage/deb 等分发、签名和自动更新策略；完成前不对外宣称支持。

## 当前产品边界

- 唯一发布资产仍为 Windows x64 `codex-switch.exe`。
- “架构已预留”不等于“已跨平台兼容”。
- 后续新模块若不是平台固有能力，默认不得直接读取 `APPDATA`、调用 Windows shell 或依赖反斜杠字符串。
