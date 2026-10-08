# macOS 预览版

`v0.5.0-macos.1` 是 ChatGPT Switch 首个 macOS 预发布版本。它提供 Apple Silicon 和 Intel 原生安装包；Windows 稳定版仍由 [Latest Release](https://github.com/mingisrookie/codex-switch/releases/latest) 提供。

## 下载与安装

打开 [macOS 预发布页](https://github.com/mingisrookie/codex-switch/releases/tag/v0.5.0-macos.1)，按“关于本机”显示的芯片选择：

| Mac 类型 | 下载文件 |
| --- | --- |
| Apple M 系列芯片 | `codex-switch_0.5.0-macos.1_aarch64.dmg` |
| Intel 处理器 | `codex-switch_0.5.0-macos.1_x64.dmg` |

打开 DMG，将 **ChatGPT Switch.app** 拖入“应用程序”，弹出磁盘映像，再打开应用。无需安装 Node.js 或 Rust。安装包的最低系统标记为 macOS 12.0；原生 CI 验证使用 macOS 15，两者不代表已经逐个实测中间系统版本。受管 Codex/ChatGPT 应用的系统要求另行适用。

本预览版使用 **ad-hoc 签名**，没有 Apple Developer ID 签名，也没有经过 Apple 公证。首次打开可能被 Gatekeeper 拦截。核对来源和发布页 SHA-256 后，如确认要继续，可按 [Apple 官方说明](https://support.apple.com/en-us/102445) 在“系统设置 → 隐私与安全性”选择“仍要打开”。不需要关闭整个系统的 Gatekeeper。

## 本版功能

| 功能 | macOS 预览版 |
| --- | --- |
| 保存官方 Account 槽位、保存 Relay 配置 | 支持 |
| Account / Relay 请求配置切换 | 支持，官方 `auth.json` 保持只读 |
| 默认 HTTP Responses、显式启用 WebSocket | 支持；第三方服务能力由实际请求确认 |
| 本机会话查看、搜索、筛选 | 支持 |
| 已兼容 SQLite 结构的会话视图切换 | 支持，包含锁定、校验及中断恢复 |
| 脱敏诊断和 Finder 定位 | 支持 |
| 高级迁移、合并、恢复可见、离线 GC、完整备份恢复 | 本预览版未开放 |
| 内置 Image2 / Grok 技能 | 本预览版未开放 |
| 应用内下载并替换更新 | 本预览版未开放，请下载新 DMG 手动更新 |

界面与后端都限制尚未开放的操作；高级存储仍可提供只读信息。该版本不声称支持全部 Windows 存储维护功能。

## 客户端识别与切换

客户端需安装在 `/Applications` 或 `~/Applications` 下，名称为 `Codex.app` 或当前统一的 `ChatGPT.app`。工具核对 bundle identifier `com.openai.codex` 及 OpenAI 签名团队身份后，才允许正常退出和重新启动。旧版 ChatGPT Classic 的 `com.openai.chat` 不属于受管客户端。识别依据见 [OpenAI Codex macOS 启动实现](https://github.com/openai/codex/blob/main/codex-rs/cli/src/desktop_app/mac.rs)。

切换前结束正在运行的任务。工具通过 AppKit 请求正常退出；应用拒绝退出、超时或仍有写入进程时会停止切换。独立 Codex CLI 会阻止切换，工具不会结束它。非标准安装路径、无法验证的签名或未知数据库结构可能需要人工处理，错误信息会说明阻止原因。

若客户端的 `process_manager/chat_processes.json` 已损坏，Mac 预览版会保留文件并停止切换，暂不执行 Windows 专用的自动修复；正常有效 JSON 和缺失的可选文件不受影响。

首次使用 Account 模式前，先在官方客户端完成登录并保存当前账号态。Relay 模式可单独配置；保存成功只说明本地配置有效，不表示 API Key、模型、费用或远端连接已验证。

## 凭据与本地数据

工具数据默认位于：

```text
~/Library/Application Support/codex-switch
```

Codex 数据仍从 `CODEX_HOME`（如设置）或 `~/.codex` 读取。不要把不同平台的加密槽位目录直接互相覆盖。

macOS 钥匙串保存随机主密钥；工具槽位及需要加密的事务资料使用 AES-256-GCM 保护，并验证完整性。主密钥丢失或访问被拒绝时，解密失败会明确报错，不会生成新密钥覆盖旧数据。钥匙串可能询问是否允许当前应用访问；应用更新后可能再次询问。Windows DPAPI 密文不能在 Mac 上直接解密。

Relay 激活期间，官方客户端必须能读取 API Key，因此 bearer token 仍会出现在受管 `config.toml` 中。新写入的私有文件限定当前用户访问；同一用户权限下的恶意程序仍可能读取运行配置。切回 Account 会移除受管 Relay token。不要分享配置文件、槽位、事务快照或未经检查的诊断包。

macOS 会话视图使用 SQLite 事务锁和 Online Backup，保持数据库文件身份，避免借用 Windows 的文件替换语义。已有目标视图在失败时可恢复到操作前快照。首次创建视图后若配置提交失败，已验证的闲置视图及共同基线会保留用于恢复；这不表示当前路由已经切换成功。事务不复制会话 JSONL 正文；未完成或存在冲突的事务会阻止继续写入。

## 验证和已知边界

发布工作流要求双架构原生编译、Mac 专项凭据/进程/视图事务测试、平台权限和能力合同测试、最终 App 启动及正常退出、DMG 内容和签名验证，以及 Windows 回归和供应链检查。实际结果以该 tag 的 [macOS CI](https://github.com/mingisrookie/codex-switch/actions/workflows/macos-release.yml) 和公开验证文件为准。

测试使用隔离目录、临时钥匙串及合成会话，不读取使用者的真实账号或历史会话，不向第三方 API 发请求。因此，测试结果不代表特定真实账号、服务商或每个 Codex/ChatGPT 客户端版本均已端到端验证。

Release 同时提供 SHA-256、Rust CycloneDX SBOM 与验证记录。GitHub 构建来源证明确认产物与工作流绑定，不能替代 Apple Developer ID 签名或公证。更多边界见 [安全说明](SECURITY.md)。
