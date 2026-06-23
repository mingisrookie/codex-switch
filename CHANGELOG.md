# Changelog

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
