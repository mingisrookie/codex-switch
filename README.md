# Codex Switch

Codex Switch 是一个 Windows 桌面小工具，用来在 **Codex 账号态** 和 **一个 OpenAI-compatible API 中转站态** 之间安全切换，同时保持本地会话可同步。

![Codex Switch 截图](docs/assets/screenshot.png)

## 能做什么

- 保存当前 Codex 账号登录态，后续可一键切回。
- 配置一个 API 中转站：填写 Base URL、模型名和 API Key。
- 切换运行态时自动备份并替换本机 Codex 的 `auth.json` / `config.toml`。
- 单独执行会话热同步，把本地 SQLite 会话索引和 `sessions/*.jsonl` 合并到共享会话池。
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
4. 把共享会话写回当前 Codex home。

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

这个操作只同步会话，不切换登录态；Codex 正在运行时也可以执行。如果 SQLite 被占用导致失败，稍后重试即可。

## 文件位置

Codex Switch 默认操作当前用户的 Codex home：

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
