# MyCodex 认证后台发布手册

维护者和新 Codex 工作线程从这里开始。本文件只记录可重复执行的发布流程；运行环境要求见 [运行说明](mycodex-auth-runtime.md)。阅读手册、修改代码或合并分支，不代表已获准提交、推送或发布；执行对应操作前核对当前任务授权。

## 仓库与入口

| 职责 | 仓库 | 本机目录 |
| --- | --- | --- |
| 后台源码、四平台云构建和 GitHub Release | `jintonglu6688/MyCodex-auth-host`（GitHub） | `E:\MyCodex-auth-core` |
| GitCode 镜像脚本及下载产物 | `gcw_SpGZ48lW/mycodex-auth-host`（GitCode） | `E:\MyCodex-auth-gitcode` |
| GUI、Windows 内置包及远端安装版本固定 | `jintonglu6688/MyCodex`（GitHub） | `E:\MyCodex` |

其他机器按实际目录调整。后台 `origin` 应指向自己的 GitHub 仓库；`upstream` 是 CC Switch 原作者仓库，不向它推送。GitCode 使用独立分发仓库，不把前后台源码推过去，也不在 MyCodex 源码仓库添加 GitCode remote。

入口是后台 `.github/workflows/auth-core-release.yml`，显示名称 **Authentication core**，只支持手动触发。选择分支只生成测试产物；选择 `auth-core-vX.Y.Z` 标签才发布 Release。提交、推送或打标签本身不会触发它。不要选择上游原版桌面程序的 `Release` 工作流或创建 `vX.Y.Z` 标签。

发布目标固定为 Windows x64、Linux x64、Linux ARM64、macOS Apple Silicon；不构建 Intel Mac。已有 GitHub Release 同步到 GitCode 不需要再次构建。

## 1. 准备版本

在后台目录检查 `git status --short --branch`、`git remote -v`，确认待发布分支和改动范围。主分支合并完成后通常从 `main` 发布；合并前应选择含完整认证工作流的分支，不能选择只有注册占位脚本的分支。

后台版本是可执行文件的 `hostVersion`，与 CC Switch 桌面应用版本分开。升版时逐项核对：

| 文件 | 内容 |
| --- | --- |
| `src-tauri/src/mycodex_host/identity.rs` | 后台返回的 `hostVersion` |
| `scripts/Package-MyCodexAuthCore.ps1` | Windows 打包身份校验 |
| `scripts/Package-MyCodexAuthCore.py` | Unix 打包身份校验 |
| `scripts/Release-MyCodexAuthCore.py` | 云发布身份校验 |
| `scripts/Test-MyCodexAuthRelease.py` | 发布测试中的版本和标签 |
| `src-tauri/tests/mycodex_host_core.rs` | 进程测试中的版本断言 |

搜索当前版本确认遗漏，保留有意使用旧版本的兼容性/失败用例。单纯升版不修改 `protocolVersion`、上游基线或 CC Switch 的 Cargo/package 版本；协议或上游变化需要另行验证。

在后台根目录运行离线打包检查：

```powershell
python -B scripts/Test-MyCodexAuthRelease.py
```

逻辑变更还应运行相应回归。云工作流会使用 `rust-toolchain.toml` 的工具链，在四个平台运行 `mycodex_host_core` 和 `mycodex_gui` 原生进程测试，再打包同一份已测试二进制。仅本地改版本不需要在 Windows 交叉编译所有平台。

按授权提交、推送已审查的文件；不要将无关改动一并提交。发布标签必须落在干净、已推送的确定提交上。

## 2. 手动云构建与 GitHub 发布

本地需要 Git、已登录且有仓库权限的 GitHub CLI。检查 `gh auth status`，不要将认证信息写入本文件或日志。

仅需要测试产物时，在后台根目录执行：

```powershell
$repo = 'jintonglu6688/MyCodex-auth-host'
$buildRef = git branch --show-current
gh workflow run auth-core-release.yml --repo $repo --ref $buildRef
```

这会消耗云构建时长，但不会发布分支产物。若直接准备正式发布，无需先重复运行一遍分支云构建；标签构建同样运行四平台测试。

正式发布时，在后台根目录为已经提交、推送并确认干净的源码建立新标签：

```powershell
$repo = 'jintonglu6688/MyCodex-auth-host'
$version = Read-Host '输入已在源码中更新的后台版本，例如 X.Y.Z'
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw '版本格式应为 X.Y.Z' }
$tag = "auth-core-v$version"
$revision = git rev-parse HEAD
git tag $tag $revision
if ($LASTEXITCODE -ne 0) { throw '创建标签失败，请检查是否已经存在' }
git push origin $tag
if ($LASTEXITCODE -ne 0) { throw '推送标签失败，尚未启动云构建' }
gh workflow run auth-core-release.yml --repo $repo --ref $tag
```

GitHub Actions 页面可选择同一工作流及标签手动运行。默认分支必须登记该工作流的 `workflow_dispatch`；实际执行的工作流和源码来自所选 ref。不要移动已发布标签或覆盖同版本产物。

查看对应提交的运行记录，确认 `headSha`、标签和运行时间，记录本次 run ID；同一提交可能存在分支测试和标签发布两次运行：

```powershell
gh run list --repo $repo --workflow auth-core-release.yml --commit $revision --limit 5 --json databaseId,headBranch,headSha,status,conclusion,url
# 将下一行改为本次运行的数字 ID。
$runId = Read-Host '本次云构建 run ID'
gh run view $runId --repo $repo
gh release view $tag --repo $repo --json tagName,isDraft,isPrerelease,assets,url
```

完成条件：四个平台构建和进程测试、汇总校验及发布任务全部成功；Release 已发布，包含 **10 个上传附件**：4 个包、4 个目标 JSON、`release-manifest.json`、`SHA256SUMS.txt`。GitHub 自动提供的源码压缩包不计入这 10 个附件，也不是安装包。云构建通过不等于用户机器部署和会话测试通过。

每个安装包包含二进制、身份 manifest、`LICENSE`、`RUNTIME.md`、`dependencies.txt`。`docs/mycodex-auth-runtime.md` 会复制成包内 `RUNTIME.md`，因此不能直接删除。源码提交、二进制哈希和压缩包哈希分别校验，不可互相替代。

## 3. 同步到 GitCode

在独立分发仓库中操作。本地需要 Python 3；发布时进程环境需设置有目标仓库发布权限的 `GITCODE_TOKEN`。不要把 Token 放进命令参数、仓库、文档或输出日志。

推荐指定本次发布的标签，先只校验 GitHub，再镜像；下列命令沿用上节 `$tag`，新终端需重新设置它：

```powershell
Set-Location E:\MyCodex-auth-gitcode
if ($tag -notmatch '^auth-core-v\d+\.\d+\.\d+$') { throw '请先设置本次发布的完整标签 $tag' }
python -B scripts/mirror_release.py $tag --check-source
if ($LASTEXITCODE -ne 0) { throw 'GitHub 产物校验失败，停止发布' }
if ([string]::IsNullOrWhiteSpace($env:GITCODE_TOKEN)) { throw '请先在当前进程环境中配置 GITCODE_TOKEN' }
python -B scripts/mirror_release.py $tag
if ($LASTEXITCODE -ne 0) { throw 'GitCode 镜像尚未完成，请检查错误' }
```

脚本校验标签提交、全部附件、包内 manifest 和 SHA-256，然后上传原始字节，并从 GitCode 公共下载地址重新下载验证。完成标志为 `Verified GitCode Release <tag>`。不需要 GitCode 云构建，也不需要为每个版本提交一次分发仓库。

一键入口 `Publish-Latest.bat`（或省略标签运行 Python 脚本）读取 GitHub `/releases/latest`，**不会自行查找版本号最大的标签**。认证云工作流当前使用 `--latest=false` 发布，因此新版本不保证成为 Latest。若确实要把该版本设为一键入口的默认来源，在确认发布和镜像结果且获得相应授权后执行：

```powershell
gh release edit $tag --repo jintonglu6688/MyCodex-auth-host --latest
gh api repos/jintonglu6688/MyCodex-auth-host/releases/latest --jq '.tag_name'
```

返回标签应与预期一致。仅同步指定版本时不必修改 Latest。GitCode 自动生成的源码压缩包只包含分发脚本，不是后台源码或后台安装包。

## 4. 让 MyCodex 使用新后台

**后台发布完成不会自动改变已经发布的 MyCodex 所固定的版本。** 前端使用审核过的固定包，只有更新前端版本/哈希配置并重新构建、分发后，用户才会得到对应的内置后台或远端更新提示。

从本次已校验的 `release-manifest.json` / 各平台 JSON 提取值，更新 MyCodex：

| 文件 | 更新内容 |
| --- | --- |
| `tools/authentication-center/Prepare-AuthenticationHostRelease.ps1` | Windows 标签、包名、压缩包 SHA-256、可执行文件 SHA-256、源码提交 |
| `tools/authentication-center/Test-AuthenticationHostPackage.ps1` | 包和安装回执接受的 `hostVersion` |
| `src/MyCodex.Infrastructure/AuthenticationCenter/AuthenticationHostRelease.cs` | 远端标签、源码提交、包名版本，以及三个 Unix 目标各自的包/二进制 SHA-256 |
| `src/MyCodex.Infrastructure/AuthenticationCenter/AuthenticationHostArtifact.cs` | 前端接受的后台版本 |
| `tests/MyCodex.Application.Tests` 中对应认证用例 | 当前版本断言；保留有意用于旧版拒绝、回退和清理的样例 |

元数据中的 `asset.sha256` 是压缩包哈希，`asset.manifest.sha256` 是二进制哈希；`manifest.sourceRevision` 是标签所指向的源码提交。不要填写发布之后产生的新提交 SHA。

Windows 的 `build-dev.bat`、`build.bat` 从已校验的 `runtime/auth-core` 或 GitHub 固定包准备后台并内置。WSL/Linux SSH/Mac SSH 从 GitCode 固定包部署和更新。两端校验值必须来自同一发布，不能关闭哈希、身份或干净源码校验来绕过不一致。

在前端根目录按 `AGENTS.md` 完成恢复/相关检查；版本和下载逻辑的最小回归入口：

```powershell
dotnet test tests/MyCodex.Application.Tests/MyCodex.Application.Tests.csproj -c Debug -p:Platform=x64 --no-restore -v:minimal --filter 'FullyQualifiedName~AuthenticationHostArtifactTests|FullyQualifiedName~AuthenticationHostReleaseTests'
```

再通过正常构建入口准备内置包并执行打包校验；不要覆盖运行中的后台。Windows 更新使用现有安装维护流程，远端从认证中心执行更新。核对原认证存档保留、运行版本/身份匹配，并实际验证 ChatGPT 直连和转换路由的新会话；发布后台本身不要求清空任何用户数据。

## 失败时从哪里继续

| 现象 | 处理 |
| --- | --- |
| 四平台构建或测试失败 | 查看该 run 日志；检查是否选错 ref、遗漏版本校验或缺少系统依赖。未完整成功前不进行镜像。 |
| 上传失败留下 GitHub 草稿 | 先核对标签、草稿和已有附件。当前发布任务会创建草稿，直接重跑可能因同名 Release 已存在而失败；经授权处理残留后再重试。 |
| GitCode 上传中断 | 同一标签重跑脚本；已存在且内容一致的附件会跳过，缺失附件继续上传。 |
| GitCode 同名附件内容不同 | 停止，核对来源和目标。脚本有意拒绝覆盖；不要强制替换已发布版本，优先更正来源或发布新版本。 |
| 一键同步取到了旧版本 | 检查 GitHub Latest；用明确标签同步，或在确认后更新 Latest。 |
| 前端仍不提示新版本 | 核对前端固定版本/哈希、构建输出和实际运行程序。它不追踪任意在线最新版。 |
| 已运行后台与新版 GUI 身份不一致 | 完成登录/在途请求，再按现有更新维护流程处理；不要通过删除认证存档或强杀活动后台解决。 |

交接时留下发布标签、源码提交、Actions run URL、GitHub/GitCode Release URL、前端采用版本的提交及已执行验收范围即可。记录放在对应 Release、提交或工单中，不在本手册累积每次发布流水。
