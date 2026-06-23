<div align="center">

# Codex Switch

**把 Codex 账号态与 API 中转站态做成可备份、可切换、可同步的本地运行态工作台。**

保存当前账号登录态；配置一个 OpenAI-compatible API 中转站；切换时自动备份 `auth.json` / `config.toml`；会话按 Codex 本地 `state_5.sqlite` + `sessions/**/*.jsonl` + `session_index.jsonl` 合并到共享会话池。

[快速使用](#快速使用) · [下载 Release](https://github.com/mingisrookie/codex-switch/releases/latest) · [更新日志](CHANGELOG.md) · [安全说明](#安全说明) · [开发](#开发)

![release](https://img.shields.io/badge/release-v0.1.2-f97316)
![license](https://img.shields.io/badge/license-MIT-16a34a)
![platform](https://img.shields.io/badge/platform-Windows-2563eb)
![stack](https://img.shields.io/badge/Tauri-2.x-24c8db)
![runtime](https://img.shields.io/badge/Codex-runtime-111827)

<br />
<br />

<img src="docs/assets/screenshot.png" alt="Codex Switch 截图" width="920" />

</div>

## 项目定位

Codex Switch 是一个 Windows 桌面小工具，用来在 **Codex 账号态** 和 **一个 OpenAI-compatible API 中转站态** 之间安全切换，同时保持本地会话可同步。

## 开发过程

本项目把 DXM 大项目协作规范也放进仓库，方便外部查看需求澄清、开发边界、链路说明和 PR 流程：

- [AGENTS.md](AGENTS.md)：Codex / AI 协作入口规则。
- [项目开发规范（AI协作）.md](项目开发规范（AI协作）.md)：开发、测试、文档同步和交付标准。
- [项目完整链路说明.md](项目完整链路说明.md)：运行态切换、会话同步和数据流说明。
- [项目文件结构说明.md](项目文件结构说明.md)：文件职责和维护边界。
- [开发者AI开发与PR提交流程.md](开发者AI开发与PR提交流程.md)：GitHub / PR / 发布流程。

## 能做什么

- 保存当前 Codex 账号登录态，后续可一键切回。
- 配置一个 API 中转站：填写 Base URL、模型名和 API Key。
- 切换运行态时自动备份并替换本机 Codex 的 `auth.json` / `config.toml`。
- 自动识别 Codex 的 `sqlite_home` / `CODEX_SQLITE_HOME`，避免把会话库固定写死在 `%USERPROFILE%\.codex`。
- 单独执行会话热同步，把本地 SQLite 会话索引、`sessions/**/*.jsonl` 正文和 `session_index.jsonl` 合并到共享会话池，并按当前运行态修正会话 provider 元数据。
- 敏感信息只加密存储，不在界面、日志、README 或导出内容里展示。

## 下载与运行

1. 打开 GitHub Releases。
2. 下载 Windows 版 `codex-switch.exe`。
3. 双击运行。

当前版本面向 Windows + Codex Desktop / Codex CLI 用户。

## 快速使用

### 1. 保存当前 Codex 账号态

先确保你当前的 Codex 能正常使用，然后点击：

```text
保存当前账号态
```

工具会读取当前本机 Codex 配置并加密保存，方便之后切回。

### 2. 配置 API 中转站

点击：

```text
配置 API 中转站
```

依次填写：

- Base URL：例如 `https://your-relay.example.com/v1`
- 模型名：例如你的中转站支持的模型名
- API Key：只会加密保存，不会显示在界面上

说明：

- 如果 Base URL 没写 `http://` 或 `https://`，工具默认按 `https://` 处理。
- Codex CLI 当前不接受在 provider 配置里直接写 `api_key` 字段；本工具会把 Key 写入切换后的 `auth.json`，`config.toml` 只保存 provider 连接参数。

### 3. 切换到中转站

点击：

```text
切换到中转站
```

如果检测到 Codex 正在运行，工具会提示关闭 Codex。确认后会：

1. 同步当前会话到共享会话池。
2. 备份当前 Codex 文件。
3. 替换 `auth.json` 和 `config.toml`。
4. 把共享会话写回当前 Codex home，并把 `threads.model_provider` / JSONL `session_meta.payload.model_provider` 归一到目标运行态。

完成后重新打开 Codex CLI / Codex Desktop，就会使用中转站 API。

### 4. 切回 Codex 账号态

点击：

```text
切换到 Codex 账号
```

流程同样会先备份和同步会话，然后恢复之前保存的账号态 `auth.json` / `config.toml`。

### 5. 会话热同步

点击：

```text
立即同步
```

这个操作只同步会话，不切换登录态；Codex 正在运行时也可以执行。同步策略是 **JSONL-first**：

- 以 `sessions/**/*.jsonl` 中的 `session_meta.payload.id` 作为可靠会话来源。
- 只合并存在正文 JSONL 的会话；只有 SQLite 行但找不到 JSONL 正文的孤儿记录会跳过，避免把不可打开的空会话同步出去。
- 合并 `session_index.jsonl`，让不同运行态看到同一批历史会话。
- 修复重复会话的缺失 JSONL / 错误 `rollout_path`，并只更新 JSONL 的 `session_meta.payload.model_provider` 元数据，不改用户或助手正文。

如果 SQLite 被占用导致失败，稍后重试即可。

## 文件位置

Codex Switch 默认操作当前用户的 Codex home。解析顺序：

1. 如果设置了 `CODEX_HOME`，优先使用它。
2. 否则使用当前 Windows 用户目录下的 `.codex`。

```text
C:\Users\<你>\.codex
```

工具自身数据保存在：

```text
%APPDATA%\codex-switch
```

主要包含：

- 加密后的运行态
- 切换/同步前备份
- 共享会话池

Codex 会话存储说明：

- 官方会话索引默认是 `state_5.sqlite`，但可能被 `config.toml` 的 `sqlite_home` 或环境变量 `CODEX_SQLITE_HOME` 改到别的位置。
- 会话正文位于 Codex home 下的 `sessions/**/*.jsonl`。
- `session_index.jsonl` 是会话索引增量文件；本工具会一起合并。
- `sqlite/codex-dev.db` 不是当前同步算法依赖的会话来源。

## 安全说明

- 不要把自己的 `auth.json`、API Key、备份目录或 `%APPDATA%\codex-switch` 上传给别人。
- 本工具不会在 UI 中展示真实 Token 或 API Key。
- 每次切换前都会创建备份，但建议重要环境先自行备份 `.codex` 目录。

## 开发

```bash
npm install
npm run tauri -- dev
```

常用检查：

```bash
npm test -- --run
npm run typecheck
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri -- build
```

## License

MIT
