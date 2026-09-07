# Release 验证说明

README 只提供下载入口。具体版本、文件大小、SHA-256、构建来源和更新验证证据应以对应 GitHub Release 为准，而不是长期固定在首页。

## 下载前确认

1. 打开 [Latest Release](https://github.com/mingisrookie/codex-switch/releases/latest)。
2. 确认页面是正式 Release，而不是 draft 或 prerelease。
3. 下载发布页列出的唯一 Windows 文件 `codex-switch.exe`。
4. 如需人工校验，使用该 Release 页面或 GitHub 提供的校验信息核对文件，而不是引用旧 README 的历史值。

## 应用内更新

应用内“检查更新”只面向 GitHub 最新正式 Release。发现更新后，应用会在用户点击“立即更新”后下载并替换 Windows 文件；网络或校验失败不应改变当前已安装版本的模式配置。

如果自动更新失败：

1. 保留现有可执行文件和必要的本地备份。
2. 从 Latest Release 手动下载新的 `codex-switch.exe`。
3. 对照 Release Notes 确认版本变化和已知限制。
4. 若问题持续，导出脱敏诊断包并在 Issue 中提供最小重现信息。

## 维护者发布检查

维护者必须以仓库中的 [开发者 AI 开发与 PR 流程](../开发者AI开发与PR提交流程.md) 和 CI 为准，完成版本同步、测试、构建、发布资产校验、公开回读和更新/回滚验证。历史 CI Run ID、UPX 参数、暂存文件数量和旧版本 hash 不应被当成新版本的证据。


## v0.3.5 维护者验证范围

v0.3.5 包含会话搜索／分页／选择优化及发布检查认证修复，不包含 v0.4.0 工作树中的开发内容。版本发布须绑定以下证据：

- npm、Cargo、Tauri 版本一致；前端、Rust 与 release harness 检查通过。
- 旧版槽位 gate 使用 step-scoped 只读 `GH_TOKEN`，不跳过旧版实际运行验证。
- 首页两张图片由 v0.3.5 Windows 程序和隔离数据生成，不包含真实账号或会话。
- 公开 `codex-switch.exe` 与 tag CI artifact 的 SHA-256、字节数和 PE 版本一致，且通过 UPX 和生产 custom protocol 启动检查。
- 真实 `v0.3.4 → v0.3.5` 更新、替换锁失败回滚及测试进程／临时目录回收完成。

以上是本版检查要求，不是运行结果替代品；实际通过状态以对应版本的 GitHub Actions、公开资产和发布验收记录为准。
