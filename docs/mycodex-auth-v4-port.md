# MyCodex 认证后台 v4 移植方案

本文记录把认证后台从 CC Switch 3.x 基线移植到 v4 内核的范围、决定和分阶段计划，供维护者和后续工作线程从这里继续。发布操作仍按 [发布手册](mycodex-auth-release.md) 执行；本文不授权提交、推送或发布。

“后端”指本仓库的认证中心部分（`src-tauri/src/mycodex_host*` 及其依赖的上游服务），“前端”指 `jintonglu6688/MyCodex` 仓库的认证中心部分（`src/MyCodex.Infrastructure/AuthenticationCenter/`、WinForms `Authentication*` 窗体、`tools/authentication-center/`）。

## 现状（2026-10-10 核对）

| 项目 | 值 |
| --- | --- |
| 后端 `main` | `2147737c`，`hostVersion` 0.2.3，`protocolVersion` 2 |
| 上游基线 | `e0f70019`（v3.20.1 之后 146 个提交） |
| 上游最新 | `upstream/main` = `b4a07943`（v4.0.5 之后 7 个提交） |
| 基线到上游最新 | 283 个提交、896 个文件；Rust 部分 165 个文件 |
| 工具链 | 未变：`rust-toolchain.toml` 相同，`rust-version` 1.85.0，Tauri 2.8.2；依赖只新增 Linux `gtk` |

已验证的事实：

- 后端直接调用约 66 个上游函数，其中 18 个在 v4 中不存在（路由接管、live 备份、通用配置片段相关）。
- 挑选的 17 个 Codex 相关上游修复对 `main` 做 `git apply --check`，全部冲突；不能在 3.x 基线上零星回移。
- v4 把切换改为只替换关键字段（`live/patch`、`live/project/codex.rs`、`live/engine.rs`），用直连/路由模式控制器（`mode/`）取代接管和整份备份，删除了通用配置片段。
- v4 新增的设备级文件（登录暂存、写入意图、首次写入备份等）经 `DeviceStore::for_device()` 固定写 `~/.cc-switch`，不经过 `get_app_config_dir()`，后端现有的数据目录重定向覆盖不到。
- v4 没有 Codex 浏览器登录；后端的 Windows 浏览器登录和导入已有登录是自有补丁。
- OAuth 客户端身份常量只用于托管账号经代理转换的请求；后端对官方账号一律直连，常量变化对后端无影响。

## 已确认的决定

1. **整体移植到 v4 内核**，不在 3.x 基线上回移单个修复。
2. **保留全局身份捕获**（`auto_capture`），按 v4 的关键字段行重写匹配逻辑；外部登录自动出现、多账号自动积累、显示真实当前状态这三项体验不变。官方与第三方之间往返时 `auth.json` 的去留交给 v4 的登录暂存（`codex_login`）。
3. **聚合（Stack）模式不在本次范围**，以后单独立项。只在聚合模式下生效的第三方压缩桥接、官方模型列表抓取随内核进入但不启用。
4. **去掉通用配置片段**，前端同步去掉通用配置界面。共享设置保留在 `config.toml` 本身。
5. “切到第三方时保留官方登录”开关保持默认关闭，本次不在前端提供。
6. 前端已完成的模型参数补全（预设 + models.dev，提交 `31b16937`、`ee83a065`）与本移植无依赖，继续沿用。

## 范围

| 包含 | 不包含 |
| --- | --- |
| v4 关键字段写入、写入引擎、直连/路由模式控制器 | 聚合模式及其压缩桥接、官方模型列表 |
| 随内核获得的转换路由修复、GPT 官方目录条目、预设更新 | Copilot、xAI 托管账号（后端只接受 ChatGPT 托管账号） |
| v4 供应商编辑器预览与冲突选择 | Claude、Gemini、Grok、OpenCode 等其他应用 |
| 全局身份捕获重写、浏览器登录与导入补丁重打 | 会话管理、用量统计、托盘、v4 新界面 |
| 协议 3、前端适配、四平台发布与迁移验收 | “保留官方登录”开关的前端入口 |

## 目标结构

### 后端适配层（`src-tauri/src/mycodex_host*`）

| 模块 | 处理 | v4 对应能力 |
| --- | --- | --- |
| `mycodex_host.rs` 入口与分发 | 保留；删除协议 3 不再提供的方法 | — |
| `identity.rs` | 保留；`hostVersion` 0.3.0，`PROTOCOL_VERSION` 3 | — |
| `ipc.rs`、`security.rs`、`paths.rs` | 保留，不依赖被删除的上游接口 | — |
| `lifecycle.rs` | 重写为 v4 模式控制器之上的薄策略层：保留路由令牌（`x-mycodex-route-token`）、只放行的路径和请求门控；删除接管、备份、`detach_external` | `mode::controller`、`mode::operation` |
| `auto_capture.rs` 及 `identity`、`diagnostics` 子模块 | 重写：按关键字段比较供应商，只写私有存储；保留 `auto-capture.jsonl` 诊断 | `live::project::codex`、`codex_login` |
| `gui_provider.rs` | 改为 v4 供应商行与编辑器的投影 | `codex_editor`、`codex_direct` |
| `gui.rs` | `save`、`preflight`、`apply` 改走 v4 切换；版本和指纹改用 v4 引擎的外部改动检测；保留 `operationId` 幂等回执 | `codex_direct`、`live::engine` |
| `gui_config.rs` | 删除 `common/*`；`provider/preview` 改用 v4 编辑器预览 | `codex_editor`、`editor_toml` |
| `gui_catalog.rs` | 用 `scripts/Export-MyCodexPresets.mjs` 从 v4 的 `codexProviderPresets.ts` 重新导出 `gui_presets.json`；获取模型逻辑不变 | `services/model_fetch.rs`（v4 未改） |
| `gui_mcp.rs` | 对照 v4 的 `mcp/codex.rs` 重新核对 | `mcp::codex` |
| `accounts.rs`、`gui_accounts.rs` | 保留；与 v4 的刷新令牌同步划清职责 | `CodexOAuthManager::adopt_account_refresh_token` |
| `runtime_config.rs`（`runtime/prepare`） | 第一阶段评估：关键字段写入后是否仍需清理遗留设置；不需要则删除并去掉对应能力 | — |

### 后端对上游文件的补丁

| 文件 | 现有用途 | 处理 |
| --- | --- | --- |
| `config.rs`、`settings.rs` | 数据目录、`settings.json`、CodexHome 重定向到宿主参数 | 保留；**新增** `DeviceStore::for_device()` 重定向到 `--data-dir` |
| `proxy/providers/codex_oauth_browser.rs` | Windows 本机浏览器登录 | 重新移植 |
| `proxy/providers/codex_oauth_import.rs`、`codex_oauth_auth.rs` | 导入已有登录、代次冲突判断 | 重新移植，与 v4 刷新令牌同步去重 |
| `proxy/http_client.rs` | 无界面宿主跨源重定向时剥离私有请求头 | 重新移植 |
| `proxy/server.rs` | 挂载路由令牌门控中间件 | 按 v4 代理服务结构重新挂载 |
| `services/proxy.rs` | 判断 live 是否属于本宿主的接管 | 删除，改用 v4 模式状态 |
| `services/provider/live.rs`、`mod.rs` | TOML 子集比较；切换前校验 live 所有者 | 按 v4 写入引擎重新评估，所有者校验并入捕获层 |
| `codex_config.rs`、`codex_config_compat.rs` | 托管账号识别；清理已忽略的遗留字段 | 对照 v4 重写后的 `codex_config.rs` 逐项判断 |
| `mcp/codex.rs` | MCP 同步调整 | 对照 v4（该文件 +259 行）重新评估 |
| `lib.rs`、`bin/mycodex-auth-host.rs`、`build.rs`、`Cargo.toml` | 模块注册、入口、构建身份 | 保留 |

### 协议 3 接口草案

| 方法 | 协议 3 |
| --- | --- |
| `status` | 保留；能力列表去掉 `commonConfig`，`runtimeConfigPreparation` 视评估结果；`route` 从 `takeover` 语义改为模式语义（直连/路由、运行、接受请求、端口、在途连接） |
| `gui/provider/list`、`get`、`save`、`copy`、`delete`、`preflight`、`apply`、`gui/operation/get` | 保留形状；版本和指纹改由 v4 引擎计算 |
| `gui/config/common/get`、`save` | 删除 |
| `gui/config/provider/preview` | 改为 v4 编辑器预览：返回切换后的 `config.toml` 和冲突信息 |
| `gui/preset/list`、`gui/model/fetch`、`gui/mcp/*`、`gui/account/*` | 保留 |
| `backend/shutdown` | 保留；`forUpdate` 改为保留模式状态，供替换进程恢复 |
| `runtime/prepare` | 视第一阶段评估 |
| `provider/*`、`account/*`（非 `gui/` 前缀）、`mcp/import`、离线 `request` 命令 | 前端不使用；第一阶段确认只被测试使用后删除 |

### 前端受影响的位置

- `AuthenticationCenterClient`（含 `.Config`、`.Mcp`）以及 Local、WSL、Remote 三种宿主：协议版本检查、`status` 字段解析、`runtime/prepare` 能力判断。
- `AuthenticationHostArtifact`、`AuthenticationHostRelease`、`tools/authentication-center/*`：版本、标签和哈希固定值。
- `AuthenticationHostStartup`：开机恢复从“接管”改为“路由模式”判断。
- `IAuthenticationCenterClient`、`AuthenticationApplyCoordinator`、DTO：去掉通用配置读写和 `SupportsCommonConfig`、`CommonConfigEnabled`。
- `AuthenticationCenterForm`、`AuthenticationProviderEditForm`、`AuthenticationConfigEditForm`、`MainForm.AuthenticationCenter`：去掉通用配置按钮和复选框，编辑器接入 v4 预览与冲突选择。
- `Strings.resx`、`Strings.zh-CN.resx` 和对应的 WinForms 测试、资源使用检查、快捷键检查。

## 分阶段计划

### 第一阶段：基线合并与可行性验证（后端，不发布）

目标：在 v4 内核上跑通最小的无界面宿主，确认迁移可行，并定稿协议 3。

1. 从 `main` 建分支 `port/v4`，合并选定的上游 v4 标签（开始时取最新 v4 标签）。冲突处理原则：上游核心文件以上游为准，我们的补丁按上表重新打。
2. 让 `mycodex-auth-host` 在 v4 上编译：先暂时隔离依赖已删除接口的适配层代码，被隔离的方法返回 `method_not_supported`；保留入口、身份、IPC、安全、路径解析。
3. 实现 `DeviceStore` 重定向，并加测试证明宿主运行时不在 `~/.cc-switch` 写任何文件。
4. 跑通最小链路：`status`、只读的供应商列表、直连切换，在临时 CodexHome 中验证 `config.toml` 未改动的内容逐字节保留。
5. 迁移验证：用 3.x 宿主生成的私有库（测试夹具，加一份真实库副本，复制到临时目录）启动 v4 宿主，逐项记录结果：数据库升级、通用配置片段的去向、接管备份残留、托管账号、捕获出的供应商、路由令牌。
6. 评估 `runtime/prepare` 是否保留，确认旧接口只被测试使用。

退出标准：v4 上编译通过；最小链路和数据目录隔离测试通过；迁移验证有书面结论且无阻断问题；协议 3 接口表定稿并更新本文。

### 第二阶段：后端适配层重建（协议 3）

1. 用 v4 模式控制器重写 `lifecycle.rs`，保留路由令牌、请求门控和只放行的路径。
2. 按关键字段重写全局身份捕获，与 v4 登录暂存、刷新令牌同步划清职责，保留诊断日志。
3. 供应商的 `save`、`preflight`、`apply`、`copy`、`delete` 改走 v4；保留 `operationId` 幂等回执和“结果不确定时不提示重试”的约束。
4. 删除 `gui/config/common/*`；`gui/config/provider/preview` 改用 v4 编辑器。
5. 重新移植浏览器登录、导入已有登录、`http_client` 补丁。
6. 重新导出预设；核对 MCP 映射。
7. 重写原生进程测试 `tests/mycodex_host_core.rs`、`tests/mycodex_gui.rs`，覆盖直连、路由、捕获、迁移、数据目录隔离；`hostVersion` 升到 0.3.0，`protocolVersion` 升到 3，并按发布手册的清单同步打包和发布脚本中的版本。

退出标准：四平台云构建（分支产物）中原生进程测试全部通过；`python -B scripts/Test-MyCodexAuthRelease.py` 通过。

### 第三阶段：前端适配

1. 客户端和三种宿主接受协议 3，解析新的 `status` 和路由字段。
2. 去掉通用配置相关的接口、DTO 字段和界面；编辑器接入 v4 预览与冲突选择。
3. 开机恢复改为按路由模式判断。
4. 更新资源文件和 WinForms 测试；按前端 `AGENTS.md` 完成构建和测试。

退出标准：前端 Debug、Release 构建通过；完整测试套件中除已记录的既有失败外全部通过；本地 Windows 实测直连、路由、外部登录捕获、旧数据升级。

### 第四阶段：发布与迁移验收

1. 按发布手册打 `auth-core-v0.3.0` 标签、云构建、发布 GitHub Release、镜像 GitCode。
2. 前端更新 Windows 内置包和远端固定值（标签、源码提交、各平台哈希、接受的后端版本），重新构建和打包。
3. 验收：Windows 内置后端、WSL、SSH（Linux、macOS）三类目标从 0.2.3 升级到 0.3.0 后，原认证存档保留、当前供应商正确、ChatGPT 直连和转换路由都能新建会话；多个客户端（Dev 与 Stable）共用后端时生命周期正常。

## 主要风险

| 风险 | 应对 |
| --- | --- |
| 老用户私有库迁移失败或丢数据 | 第一阶段用真实库副本验证；第二阶段把迁移写成原生进程测试；升级前保留原库备份 |
| `DeviceStore` 漏重定向，导致多个目标共用 `~/.cc-switch` | 第一阶段先实现并加测试，后续每次合并上游都检查新增的设备级路径 |
| 捕获逻辑与 v4 登录暂存重复处理同一份登录 | 明确分工：捕获只写私有存储和当前指针，`auth.json` 的去留只由 v4 切换决定 |
| 上游在移植期间继续演进 | 第一阶段固定合入点；完成后再按标签增量合并 |
| 前端和后端版本不一致 | 协议版本硬校验保持不变；两端的版本和哈希来自同一次发布 |

## 进度记录

| 日期 | 阶段 | 内容 |
| --- | --- | --- |
| 2026-10-10 | 规划 | 本方案定稿；尚未开始第一阶段 |
