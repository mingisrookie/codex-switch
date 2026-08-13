# 开发者 AI 开发与 PR 流程

> 项目：`codex-switch`
> 根目录：`<repo-root>`
> 初始化日期：`2026-06-23`

AI 在本项目里进行开发、整理改动、发起 PR、更新 PR、补充说明或合并时，必须按本文执行，不能跳步、不能猜、不能自创流程。

<!-- DXM-DOC-RULES:START -->

<!-- DXM-CONTRACT:2 -->

## DXM 文档维护规则

- 本块由 DXM 管理；`--refresh-blocks` 只刷新本块，保留下方项目专属 Git/PR 规则和人工补充。
- GitHub、PR、提交、推送、合并相关结论必须基于真实命令输出和真实 diff。
- 未获用户明确授权时，不得 stage、commit、push、创建 PR、合并 PR、关闭 PR、删分支、强推、创建/推送 tag、创建/编辑 GitHub Release 或修改 Latest。
- PR 文案必须基于真实改动和验证结果，不写机器人腔或空泛结论。

<!-- DXM-DOC-RULES:END -->

## 使用前准备

在让 AI 操作 GitHub 之前，开发者本机必须先安装 GitHub CLI，并完成登录。

最低要求：

```bash
gh --version
gh auth status
git status --short --branch
git remote -v
```

如果 `gh` 不可用、未登录、登录到错误账号、权限不足，AI 必须停止并明确告诉开发者，不准假装已经完成 GitHub 操作。

## 适用场景

本文适用于：

- 发起新的 PR。
- 更新已有 PR。
- 在自己的 PR 下补充说明。
- 在权限允许且用户明确授权时，合并自己的 PR。
- 用户明确要求更新 release、tag 或发布资产。

不适用于：

- 当前目录不是 Git 仓库时强行执行 Git/PR 流程。
- 没有看代码上下文就靠猜测生成 PR 结论。
- 用户没有明确授权时擅自合并、关闭 PR 或删除远端分支。
- 用户没有明确授权时擅自创建 tag、覆盖 release 资产或发布新版本。

## 仓库硬性规则

1. 任何结论都不能猜，必须基于真实命令输出、真实 diff、真实代码上下文。
2. 发起 PR 前必须确认目标分支策略；如果项目有 `dev` 分支，默认 PR 指向 `dev`，否则先询问或按项目文档执行。
3. 发起 PR 前必须同步最新远端提交，确认当前分支已经吸收目标基线。
4. 当前工作区有无法确认归属的脏改动时，必须先停下来告诉开发者，不能偷偷带进本次 PR，也不能擅自删除。
5. PR 标题、正文、评论用自然中文直接表达，不写“自动回复”“AI 分析结果如下”这类机器人腔。
6. 没有开发者明确授权时，不得合并 PR、关闭 PR、删除远端分支或强推。

## 标准执行顺序

### 阶段 1：环境确认与仓库现状检查

必须先执行并阅读：

```bash
git status --short --branch
git remote -v
gh auth status
```

确认：当前仓库、当前分支、远端地址、工作区脏改动、GitHub 登录身份。

### 阶段 2：对齐目标基线

新任务应从最新目标基线拉出功能分支。继续已有分支时，必须判断当前分支是否落后于目标基线；可以先继续开发，但发 PR 前必须补齐最新基线。

### 阶段 3：开发与本地整理

AI 开发时必须遵守 `项目开发规范（AI协作）.md`。PR 中只能包含与本次任务相关的改动，不要带入临时调试代码、无关格式化、构建产物、运行态数据或密钥文件。

提交信息必须描述真实功能结果，不写 `update`、`fix`、`AI 修改` 这类空泛信息。

### 阶段 4：发起 PR 前再次同步

准备发起 PR 前，必须再次拉取远端最新提交，并确认当前分支已经吸收目标基线。冲突解决后必须重新检查 diff，不能只删除冲突标记。

### 阶段 5：创建或更新 PR

创建 PR 前先确认目标分支。示例：

```bash
gh pr create --base <target-branch> --head <feature-branch> --title "<PR标题>" --body-file <PR正文文件>
```

如果 PR 已存在：

```bash
gh pr view <PR_NUMBER> --json number,title,baseRefName,headRefName,state,isDraft,url
```

PR 正文建议包含：

```markdown
## 本次改动
-

## 风险与影响
-

## 测试情况
-
```

正文必须基于真实改动和真实验证结果。

### 阶段 6：只有明确授权时才允许合并

如果开发者明确要求 AI 继续合并自己的 PR，必须先确认：

```bash
gh pr view <PR_NUMBER> --json number,title,baseRefName,headRefName,state,isDraft,mergeable,mergeStateStatus,url
```

合并前必须满足：PR 仍 open、不是 draft、目标分支正确、没有未处理冲突、已完成项目要求的验证、开发者明确授权合并。

合并成功后必须验证 PR 状态和本地目标分支是否更新，不能把“同步 PR 分支”和“合并 PR 到目标分支”混为一谈。

### 阶段 7：Release 发布

用户明确要求更新 release 时，必须基于真实命令输出执行：

1. 确认当前分支、远端、GitHub CLI 登录和工作区状态：

   ```bash
   git status --short --branch
   git remote -v
   gh auth status
   gh release list --limit 10
   ```

2. 同步版本号，至少检查并更新：

   - `package.json`
   - `package-lock.json`
   - `src-tauri/Cargo.toml`
   - `src-tauri/Cargo.lock`
   - `src-tauri/tauri.conf.json`
   - `README.md` release badge / 下载说明
   - `CHANGELOG.md`
   - 受影响的根目录长期文档

3. 发布前必须重新验证：

   ```powershell
   npm test -- --run
   npm run typecheck
   npm run build
   cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
   cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
   cargo test --locked --manifest-path src-tauri/Cargo.toml --all-targets
   npm run tauri -- build --no-bundle
   node scripts/check-release-contract.mjs src-tauri/target/release/codex-switch.exe
   ```

   v0.3.0 还必须在任何 tag/Release 前完成并留证：

   - PRD 全量对抗矩阵：数据关系、全局引用/WAL、并发 TOCTOU、每阶段故障注入与迁移/GC/降级幂等；任一有效消息、工具关系、被引用正文或真实分叉受损都阻断发布。
   - 当前真实主库只做最后一次只读 preflight，并在前后复核源 bytes/hash/mtime；破坏性迁移/GC 只运行本机隔离副本，不上传、不进 Git/Release。
   - v0.2.0–v0.2.7 精确旧版运行时分别验证隔离降级包的列表、恢复和继续会话。下载/本地构建旧版 EXE 每次执行前都需要行动时确认，不得把当前新版 runtime 验证代替旧版验证。
   - `release/codex-switch.exe` 在隔离 `APPDATA` / `CODEX_HOME` 且无 Vite listener 时真实启动、进入 Tauri Ready、显示 v0.3.0 并 graceful close；生产 UI 在 1200×820、900×640、390×844 完成导航/hit-test/reduced-motion/视觉检查。
   - 文档/编码/隐私/secret/path 扫描必须完成，Trellis 最终 `check.md` 必须先达到 PASS 门；此时任务仍保持 active。不得在 tag、公开资产回下载和 updater 成功/回滚证据之前执行 finish/archive 或生成宣称完成的 DXM completion receipt。

4. tag 和 release 必须指向已提交、已推送的 commit。不得用脏工作区产物发布。

5. Windows Release 必须从 raw EXE 生成独立 packed 副本，禁止原地压缩 raw EXE，也禁止从未验证的 `PATH` 任取 UPX：

   - 官方 `UPX 5.2.0` 版本固定不变。
   - `upx-5.2.0-win64.zip` 的 SHA-256 固定为 `B471EBF1B7F20F4A89150264ED9A008A2A5BFD247F3C6D1184A75BB59CA08F5D`。
   - 解包后的 `upx.exe` SHA-256 固定为 `F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D`。
   - 统一调用 copy-only 脚本；raw 输入保持在 `src-tauri/target/release/codex-switch.exe`，唯一待发布输出为 `release/codex-switch.exe`。

   ```powershell
   Test-Path src-tauri\target\release\codex-switch.exe
   Get-FileHash -Algorithm SHA256 "<verified-upx.exe>"
   .\scripts\pack-windows-release.ps1 `
     -UpxPath "<verified-upx.exe>" `
     -SourceExe "src-tauri/target/release/codex-switch.exe" `
     -OutputExe "release/codex-switch.exe"
   ```

   该脚本必须冻结并复核 raw hash，对 raw 与 packed 分别运行 release contract，仅在 staging 副本上执行 `upx --ultra-brute --lzma`，并对 packed 文件执行 `upx -t`。v0.3 关闭 Tauri runtime Brotli asset feature，仅保留 `wry` / Windows `common-controls-v6`，因此必须由最终固定 UPX 层压缩并做 custom-protocol 启动验证。packed 必须保持 PE32+ x64，`ProductVersion` / `FileVersion` 必须与目标 tag 一致，体积必须小于 3 MB 并满足 3,000,000 bytes 硬门禁。发布前还必须实际启动 `release/codex-switch.exe`，确认主窗口、版本和基本切换入口可用；只验证 raw EXE 不算发布验证完成。

   公开 UI 和窗口标题可以使用 ChatGPT Switch，但 Release 资产必须继续唯一命名为 `codex-switch.exe`；既有 updater 固定校验该名称，不能直接改为 `chatgpt-switch.exe`。

6. 创建 tag 后必须等待该 tag commit 对应的 Windows CI 全部通过，并从该 run 下载唯一 packed artifact。不得上传 raw EXE，也不得用本地脏工作区产物替代 tag-CI 产物：

   ```powershell
   $tagCommit = git rev-list -n 1 <tag>
   gh run list --workflow ci.yml --commit $tagCommit
   gh run download <run-id> `
     --name "codex-switch-$tagCommit" `
     --dir "artifacts\<tag>"
   node scripts/check-release-contract.mjs "artifacts\<tag>\codex-switch.exe"
   & "<verified-upx.exe>" -t "artifacts\<tag>\codex-switch.exe"
   ```

   下载后的 tag-CI artifact 必须再次满足 packed contract、`upx -t`、PE32+ x64、双版本、体积门禁，并实际启动成功。CI artifact 只能包含 packed 的裸 `codex-switch.exe`。

7. 创建或更新 GitHub Release 后，必须把公开资产重新下载到独立目录验证，不能只检查网页元数据：

   ```powershell
   gh release view <tag> --json tagName,name,url,assets
   gh api repos/mingisrookie/codex-switch/releases/latest --jq .tag_name
   git ls-remote --tags origin <tag>
   gh release download <tag> `
     --pattern "codex-switch.exe" `
     --dir "release-verification\<tag>" `
     --clobber
   node scripts/check-release-contract.mjs "release-verification\<tag>\codex-switch.exe"
   & "<verified-upx.exe>" -t "release-verification\<tag>\codex-switch.exe"
   Get-FileHash -Algorithm SHA256 "release-verification\<tag>\codex-switch.exe"
   ```

   回下载文件的 SHA-256 和字节数必须与 tag-CI artifact 完全一致，并重新通过 packed contract、`upx -t`、体积门禁和实际启动验证；网页显示存在同名资产不能代替二进制回下载校验。

   `gh release view --json` 不支持 `isLatest` 字段；必须把 `gh api .../releases/latest` 返回的 tag 与目标 `<tag>` 对比，不能仅凭 release 列表顺序推断 latest。

8. 自动更新必须使用已发布 Release 做真实首跳烟测。以 `v0.2.1` 为例，必须从干净隔离环境中的正式 `v0.2.0` 启动，触发一键更新并证明 updater 下载的正是 Release 中的 `codex-switch.exe`，随后完成退出、替换、重启并显示 `v0.2.1`；同时复核中转站配置、会话数据和其他用户状态未被破坏。v0.3 还必须用同一路径覆盖替换锁失败后的自动回滚、旧 EXE 可重新启动以及零 helper/staging 残留。tag-CI、Release 回下载、真实首跳或回滚任一未完成，都不得宣称发布闭环完成。

9. Release 型 Trellis 任务只能在上述公开入口证据全部绑定到精确发布 commit 后收口，固定顺序为：

   ```text
   final check.md PASS
   -> commit / push
   -> tag / tag-CI / public Release / public asset re-download
   -> updater success + rollback evidence
   -> task.py finish
   -> task.py archive <task> --no-commit
   -> schema v2 completion receipt
   ```

   `check.md` PASS 是发布动作的前置质量门，不等于任务已归档。`finish`、`archive` 和 completion v2 不得出现在 public/updater evidence 之前；最终 receipt 必须记录真实 Git、tag、Release、公开回下载和 updater 事实，不能用 pending/计划值冒充完成证据。

Release 文案必须说明用户可见变化、风险/兼容性和本次验证命令，不得用空泛“更新 release”替代。

## 最终反馈必须说明

1. 当前分支与工作区状态。
2. 改了哪些文件。
3. 是否创建或更新 PR。
4. PR 编号、链接和目标分支。
5. 是否提交、提交号是什么。
6. 是否推送、推送到哪个分支。
7. 是否创建或更新 Release；Release tag、链接和资产名称是什么。
8. 运行过哪些测试或检查；如果没跑，要明确说明。
9. 是否已经合并；如果已合并，要说明合并目标、PR 状态和合并提交。
10. Release 型任务是否按 `check PASS -> commit/push/tag/public/updater evidence -> finish/archive/completion v2` 收口。
11. 未完成项、阻塞或风险。
