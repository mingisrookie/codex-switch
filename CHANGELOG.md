# Changelog

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
