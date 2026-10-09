# Release 验证说明

## macOS 预发布验证

Mac 安装包位于 [v0.5.0-macos.1 预发布](https://github.com/mingisrookie/codex-switch/releases/tag/v0.5.0-macos.1)，不进入 Windows Latest 更新通道。按芯片选择 aarch64 或 x64 DMG，并下载同名 SHA-256 文件，在同目录执行 `shasum -a 256 -c <下载的.sha256文件名>` 核对。构建来源可用 `gh attestation verify <下载的.dmg文件名> --repo mingisrookie/codex-switch` 验证。

[macOS 工作流](../.github/workflows/macos-release.yml) 从相同 tag 原生构建两架构，发布前要求 Windows 回归、依赖检查、Mac 专项行为测试、生产 App 隔离启动/正常退出，以及 DMG 内外 App 一致性、Mach-O 架构、版本和签名完整性验证。Release 附带各架构验证 JSON 与 Rust CycloneDX 清单。原生验证不使用真实账号或第三方 API 请求。

仅有 ad-hoc 签名，不等于 Apple Developer ID 或公证；安装方法及限制见 [macOS 预览版](MACOS_PREVIEW.md)。以下 Windows 正式版合同继续适用于 EXE 通道。

Mac 版本映射遵守 Apple bundle 元数据格式：Release tag 为 `v0.5.0-macos.1`，Cargo/npm、应用内显示与安装包文件名保留 `0.5.0-macos.1`；Info.plist 的 `CFBundleShortVersionString` 和 `CFBundleVersion` 使用数字版本 `0.5.0`。发布脚本同时校验完整发行版号及这个明确映射，不以 Finder 的数字版本代替预发布身份。

Mac 安装包还必须携带 `subtle 2.6.1` 的完整 BSD-3-Clause 许可文件；源码、构建 App 和最终挂载 DMG 内的文件均核验固定摘要。生产启动验证另要求最终可执行文件在隔离目录中拒绝 Windows updater helper 参数，以退出码 1 结束且不改变 Codex Home；正常 GUI 生命周期与这个拒绝分支都必须通过。

README 只提供下载入口。具体版本、文件大小、SHA-256、构建来源和更新验证证据应以对应 GitHub Release 为准，而不是长期固定在首页。

## 下载前确认

1. 打开 [Latest Release](https://github.com/mingisrookie/codex-switch/releases/latest)。
2. 确认页面是正式 Release，而不是 draft 或 prerelease。
3. 下载发布页列出的唯一 Windows 文件 `codex-switch.exe`。
4. 如需人工校验，使用该 Release 页面或 GitHub 提供的校验信息核对文件，而不是引用旧 README 的历史值。


## 构建来源与 SBOM 证明

从 v0.4.0 起，GitHub Release 固定包含唯一可执行资产 `codex-switch.exe` 和只读供应链清单 `codex-switch.cdx.json`。tag CI 为最终 EXE 生成 GitHub Artifact Attestation：一份 SLSA/in-toto 构建来源证明，以及一份绑定该 CycloneDX SBOM 的证明。安装 GitHub CLI 后可验证公开下载文件：

```powershell
gh attestation verify .\codex-switch.exe --repo mingisrookie/codex-switch
node ..\scripts\check-sbom.mjs .\codex-switch.cdx.json
```

该证明用于确认文件与 GitHub Actions 发布身份及摘要的绑定关系。当前 EXE 仍未使用商业代码签名证书完成 Windows Authenticode；attestation、Release digest、UPX `-t` 和 PE 版本检查均不能替代 Authenticode 或杀毒软件扫描。

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
