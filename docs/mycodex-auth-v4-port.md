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
| `runtime/prepare` | 保留（第一阶段结论见下文“第一阶段发现”） |
| `provider/*`、`account/*`（非 `gui/` 前缀）、`mcp/import`、离线 `request` 命令 | 已确认前端不使用，第一阶段已删除；`provider/switch` 作为第一阶段临时入口保留，第二阶段由 `gui/provider/apply` 取代 |

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

## 第一阶段发现

分支 `port/v4`：合并提交 `5b31fcfc`（v4.0.5 提交 `2db86e94`，标签对象 `a98207e2`），之后的提交让宿主在 v4 上编译并跑通最小链路。

- **合并冲突只有 7 个文件**：`codex_config.rs`、`mcp/codex.rs`、`services/provider/live.rs`、`services/provider/mod.rs`、`services/proxy.rs` 取上游；两个 CI 工作流保留本仓库只能手动触发的版本，与上游 CI 的差异另行核对。`config.rs`、`settings.rs`、`http_client.rs`、`server.rs`、`codex_oauth_auth.rs`、`Cargo.toml` 的补丁自动合并成功。
- **只启动 Codex**：`mode::controller::startup` 遍历所有应用，对没有模式记录的应用写入直连配置。宿主改用新增的 `startup_single`，只处理 Codex；中断写入的补完仍用 `mode::operation::recover_on_startup`。
- **设备目录**：`DeviceStore::for_device()` 和 `drain_legacy_backup` 已重定向到 `--data-dir`。测试用空“家目录”证明宿主不写目标以外的位置；v4 的 `live-state.json` 写在私有存储内。
- **第三方路由表固定为 `custom`**：v4 把所有第三方供应商写成 `model_provider = "custom"` 和 `[model_providers.custom]`，3.x 用的是供应商自己的 ID。MyCodex 列会话时传 `modelProviders: []`，不按供应商过滤，旧会话不会消失；Codex CLI 等按当前供应商过滤的入口会把 3.x 的会话和新会话分开。第二阶段核对 v4 的会话历史统一功能是否覆盖这种情况，并在迁移说明里写明。
- **`runtime/prepare` 仍需要**：v4 把 `disable_response_storage` 列为关键字段（`live/project/codex.rs` 的 `ROW_TOP_FIELDS`），供应商行里有这个键时切换会写回 `config.toml`，而这正是此前用 `normalize_live_config` 清除的遗留设置。第二阶段在宿主投影中排除它，并保留启动前清理（含 MCP 表里的 `type`）。
- **登录导入**：`extract_codex_managed_oauth_account_id` 在 v4 中是私有函数，且只接受显式的 `auth_mode = "chatgpt"`；已恢复为 `pub(crate)` 并接受旧式隐式 ChatGPT 登录形状。浏览器登录和导入代码暂时没有调用方（账号模块在第一阶段未编译），第二阶段接回。
- **迁移验证（合成数据）**：用校验过的 0.2.3 发布版（SHA-256 `785c44dc…`）生成 3.x 私有库和 `config.toml`，再加入用户自己的顶层设置、注释和 MCP 表；v4 宿主启动后列表和当前供应商正确，直连切换只替换关键字段，用户内容逐字节保留。见 `src-tauri/tests/mycodex_host_v4.rs`，设置 `MYCODEX_V3_HOST` 指向 0.2.3 程序后运行，否则跳过。
- **迁移验证（真实数据副本，2026-10-10，经维护者同意）**：在临时目录复制 Windows 目标的私有库和 `config.toml`、`auth.json`，只改副本里绑定的目标路径；副本中的刷新令牌全部换成无效占位值，并给测试宿主设置不可达的代理，避免任何刷新令牌被轮换；验证结束后删除副本。数据：4 个供应商（2 个 ChatGPT 托管账号，其中一个为当前；2 个第三方原生 Responses），一份 5,722 字符的通用配置片段，无路由接管和旧接管备份。结果：
  - 启动后列表、当前供应商、直连模式正确，`config.toml` 和 `auth.json` 逐字节不变（未接管时 Codex 启动只记录模式状态，不写客户端文件，也不取托管账号令牌）。
  - 在断网条件下切到第三方供应商成功：用户的 15 项顶层设置和表（`mcp_servers`、`hooks`、`plugins`、`projects`、`features`、`memories`、`marketplaces` 等）全部不变，只替换 `model`、新增 `model_provider = "custom"`、`[model_providers.custom]` 和 `model_catalog_json`。通用配置片段的内容本来就在 `config.toml` 里，没有丢失。
  - 从托管账号切到第三方时 `auth.json` 被移除，托管登录标记文件被清理；登录暂存、`live-state.json`、首次写入备份、数据库备份都写在私有存储内，“家目录”保持为空。
  - 切回托管账号需要用刷新令牌换取访问令牌（托管账号库只存刷新令牌）；在上述保护下按预期失败，且失败后客户端文件不变。正常环境下这一步需要联网。
- **尚未完成**：旧接管备份残留和路由模式下的迁移样本（用 0.2.3 常驻模式生成合成数据），第二阶段与路由重建一起验证。原有的 `tests/mycodex_host_core.rs`、`tests/mycodex_gui.rs` 针对 v3 协议，在本分支上预期失败，第二阶段重写。

## 第二阶段实现与决定

适配层已在 v4 上重建（`hostVersion` 0.3.0，`protocolVersion` 3），与方案的差异和新增决定如下：

- **模式自动选择**：前端没有模式概念。需要转换的供应商用 `mode::controller::enter_with_route` 进入代理模式并以自己为路由；其余供应商先 `exit` 再 `ProviderService::switch` 直连（`lifecycle::activate`）。保存当前供应商时，若其是否需要转换与当前模式不一致，先存行再切换模式，不让 v4 按直连写入转换供应商。
- **路由令牌**：v4 生成代理路由表（`proxy_route_table`）时为宿主加入 `x-mycodex-route-token` 请求头；宿主只在 `config.toml` 选中 `custom` 表且该表恰好带本存储令牌时认为路由属于自己。
- **路由端口**：监听端口保持 0（系统分配），宿主重启后重新绑定并重写客户端契约。Codex 每次请求重读 `config.toml`，无需重启 Codex。3.x 会沿用原端口，这一点有变化。
- **启动顺序**：先补完中断写入；若模式记录为已接上但 `config.toml` 已不属于本宿主（外部切换），只落定直连模式，不写客户端文件；再执行只针对 Codex 的 `startup_single`；最后做一次全局身份捕获（与 3.x 启动时先捕获再恢复路由一致）。外部切换到别的供应商时客户端文件逐字节保留；仍指向本宿主代理占位符的残留（例如令牌被改动）由 v4 退为清空路由关键字段，避免 Codex 指向拒绝它的代理。
- **全局身份捕获**：用 `CodexProjection::of` 把供应商行投影成 v4 会写入的关键字段，再与 live 比较身份和是否完全一致；新建的供应商行只保存关键字段。外部登录导入、多账号积累、诊断日志保持不变。
- **当前供应商的模型**：v4 不回填 live 的关键字段，而 MyCodex 会话设置会写 `model` 和 `model_reasoning_effort`。直连使用中、且 live 身份属于该供应商时，表单显示和保存以 live 的 `model`、`model_reasoning_effort`、`plan_mode_reasoning_effort`、`review_model` 为准，读取时不写回行（与 3.x 快照语义一致）。
- **新增与复制**：路由模式下 v4 的直连指针为空，`ProviderService::add` 会把任何新行当作第一个供应商写入 live。未生效的新增和复制只存行，不经过原生激活。
- **配置编辑**：`gui/config/provider/preview` 返回供应商行自己的 TOML（关键字段）；全局设置留在 `config.toml`。v4 的“切换后完整 `config.toml` 预览 + 三方冲突选择”需要前端设计冲突界面，列为后续项，本次未接入。
- **遗留设置**：宿主运行时 `disable_response_storage` 不作为关键字段管理；`runtime/prepare` 的清理（含 MCP 表 `type`）移入 `mycodex_host/runtime_config.rs`，删除了上游文件补丁 `codex_config_compat.rs`。
- **MCP**：重新打上导入时完整保留嵌套未知字段、JSON→TOML 无损转换的补丁（`mcp/codex.rs`）；只有 `url` 的条目按 HTTP、不写 `type` 已由 v4 自身修复；v4 的 `write_text_file` 已是原子写入。
- **预设**：用 `scripts/Export-MyCodexPresets.mjs` 从 v4 的 `codexProviderPresets.ts` 重新导出。
- **测试**：`tests/mycodex_host_core.rs`（16 个，含两个需要 `MYCODEX_V3_HOST` 的 3.x 直连/路由迁移用例）和 `tests/mycodex_gui.rs`（22 个）按协议 3 重写；删除离线探测、原生 provider/account 方法和通用配置相关用例；`tests/mycodex_host_v4.rs` 已并入 core。适配层单元测试 18 个。发布脚本版本与协议已同步，`python -B scripts/Test-MyCodexAuthRelease.py` 通过。

协议 3 相对协议 2 的接口变化（前端第三阶段据此适配）：

| 接口 | 变化 |
| --- | --- |
| `status.capabilities` | 去掉 `commonConfig` |
| `status.route` / `gui/provider/list.route` | 新增 `mode`（`direct`/`proxy`/`null`）、`attached`；`takeover` 表示 Codex 正通过本宿主路由（重启宿主须恢复）；`running`、`accepting`、`port`、`activeConnections` 不变 |
| 供应商摘要 | 去掉 `commonConfigEnabled`；保存时忽略该参数 |
| `gui/config/common/get`、`save` | 删除（`method_not_supported`） |
| `gui/config/provider/preview` | 返回供应商行自己的 TOML |
| 非 `gui/` 前缀的 `provider/*`、`account/*`、`mcp/import`，离线 `request` 命令 | 删除 |
| 身份与清单 `upstreamRevision` | 由 3.x 基线 `e0f70019…` 改为 v4.0.5 提交 `2db86e94da13365caae55bb08d09295e31500d21` |

## 第三阶段实现与决定

- **版本校验**：客户端、三种宿主和打包脚本只接受协议 3、`hostVersion` 0.3.0 和 v4.0.5 的 `upstreamRevision`。WSL/SSH 目标上已安装的 0.2.3 包（协议 2、上游 `e0f70019`）仍判为“有可用更新”，以便原地替换；协议与上游不配套的清单一律不兼容。安装器维护脚本继续接受协议 2 和 3 的已安装包，用于停止旧常驻进程。
- **通用配置**：前端删除客户端接口、协调器、DTO 字段、主窗口“通用配置”按钮、编辑器里的共享复选框、相关资源键和错误文案；提供商 TOML 编辑器提示改为“只包含该提供商自己的字段，全局设置保留在 `config.toml`”。
- **集成测试**：需要真实宿主的显式测试改用 `gui/*` 接口（保存、应用）代替已删除的原生 `provider/*` 方法；插件/技能测试改为断言全局设置在每个供应商下都保留；往返测试改为断言 v4 不回填 live 关键字段。
- **未做**：三方冲突编辑器（见第二阶段）；Windows 内置包与远端固定值（标签、哈希、源码提交）随第四阶段发布更新。

## 第四阶段发现：Unix 管理端点被 `/tmp` 清理（0.3.1）

Mac SSH 目标验收时“更新后台”失败，界面只显示通用失败。该问题自 0.2.x 起就存在，和 v4 移植无关：

- Unix 常驻进程把 `management.lock` 和 `management.sock` 放在 `/tmp/mycodex-auth-core-<uid>-<哈希>/`。macOS `tmp_cleaner`（以及 systemd-tmpfiles）会删除几天未访问的文件；常驻数天后端点消失，`rpc` 返回 `backend_unavailable`。
- 客户端随即启动新的 `serve`：它取得新建的实例锁，先替换 socket，再因数据目录锁仍被旧进程持有而以 `store_busy` 退出，留下无人监听的 socket。旧常驻从此无法查询、更新或停止。
- 0.3.1 修复：`serve` 先取数据目录锁再动 socket，竞争进程以 `backend_already_running` 退出且不影响常驻；常驻每 10 秒刷新端点文件时间，并在锁或 socket 被删除、替换时自行重建。原生测试 `resident_restores_endpoint_files_removed_from_tmp` 在 macOS 和 Linux 上覆盖该场景。
- 已陷入此状态的 0.2.x/0.3.0 常驻无法经协议恢复，需要在目标上结束旧的 `mycodex-auth-host` 进程（或重启）后再更新；前端为 `store_busy` 给出这一提示。

## 后续：完整配置编辑器（0.3.2）

- 新增能力 `fullConfigEditor`：`gui/config/provider/view` 返回切换到该供应商后的完整 `config.toml` 和不随切换生效的字段（v4 `editor_view`）；`gui/config/provider/preview` 带 `editorBase` 时按 v4 `plan_save` 拆分，返回供应商行 TOML 和 `globalChanges`。
- `gui/provider/save` 带 `editor{base,text,onConflict}` 时先按三方比较写全局改动（`codex_editor::write_live`，只写用户改过的键）；默认拒绝冲突并返回 `editorConflict{keys}`，此时行和 `config.toml` 都不写；`keepMine`/`keepTheirs` 由前端冲突对话框选择后重试。不带 `editor` 的保存与之前完全相同。
- 上游补丁只有两处：三个编辑器模块改为 `pub(crate)`；`TomlEdits::paths` 不再仅限测试。
- 实测：推理摘要、详细程度、网页搜索在普通、路由、MiMo 之间切换都保留（回归测试 `mycodex_official_settings_survive_provider_switches`）；SudoCode.us 预设自带 `model_verbosity`，切过去会覆盖、切走会删除（上游行为）。
- 修复（未发布，待下个版本）：适配层 `shadowed_settings` 在供应商覆盖独有字段前记下用户的值（数据目录 `shadowed-settings.json`），切走后若被 v4 删除则按三方写回；被覆盖期间用户改过的值优先，不恢复旧值。回归测试 `exclusive_settings_a_provider_overrides_return_to_the_users_value`。

## 后续：合并上游 v4.0.7（0.3.3）

- 合并 v4.0.6、v4.0.7（`790ed800`），只有 `src-tauri/Cargo.toml` 的 `windows-sys` 功能列表冲突（保留本仓库的 `Win32_Security_Authorization` 和上游新增的 `Win32_Foundation`）；身份 `upstreamRevision` 改为 `790ed8009df809bedaf6c31b5ced05da46bb1f1f`，前端仍把 v4.0.5 基线的 0.3.x 安装识别为可更新。
- 与本后端相关的上游修复：第三方（转换路由、Copilot）模型目录改用内置模板，不再复制本机 `models_cache.json` 的 gpt-5.5 条目（该条目已带 `tool_mode: code_mode_only`、隐藏标记，并于 2026-10-14 退役）；同一用户的不同 ChatGPT 工作区按用户加工作区区分；顶层 `base_url` 的旧写法归一到 `[model_providers.custom]`；删除托管账号时解绑引用它的供应商（宿主的删除接口调用同一函数，自动生效）。
- 预设重新导出（上游去掉 Copilot 的代表性 `apiFormat`）。
- 新增回归测试：`third_party_catalog_does_not_inherit_the_codex_model_cache`（合并前失败：第三方条目继承了 `code_mode_only`）、`workspaces_of_one_user_are_separate_accounts_and_switching_rewrites_auth`（本宿主的托管账号路径合并前后都通过，作为防回归）。

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
| 2026-10-10 | 规划 | 本方案定稿 |
| 2026-10-10 | 第一阶段 | 建 `port/v4` 并合并 v4.0.5；宿主在 v4 上编译；设备目录重定向；只启动 Codex；最小链路和合成数据迁移测试通过；`runtime/prepare` 评估为保留 |
| 2026-10-10 | 第一阶段 | 真实私有库副本迁移验证通过（结果见“第一阶段发现”）；`port/v4` 推送到 GitHub |
| 2026-10-10 | 第二阶段 | 适配层在 v4 上重建，协议 3、版本 0.3.0；本地原生进程测试 38 个和适配层单元测试 18 个通过（见“第二阶段实现与决定”）；四平台云构建待运行 |
| 2026-10-10 | 第二阶段 | 四平台云构建 run `38024569264`（`e6a1eb99`）：四个平台构建（含原生进程测试）和完整发布校验全部通过；未打标签，发布步骤按设计跳过 |
| 2026-10-10 | 第三阶段 | 前端适配完成（见“第三阶段实现与决定”）；身份 `upstreamRevision` 改为 v4.0.5。前端 Debug/Release 构建无警告；完整测试 4662 个中只有已记录的既有失败 `MainFormShutdownDiagnosticsTests` 1 个；用本地 0.3.0 包运行需要真实宿主的显式测试 6 个（直连/路由切换、MCP、插件与技能、退出与重启）全部通过；后端原生进程测试 38 个通过 |
| 2026-10-10 | 第四阶段 | 发布 `auth-core-v0.3.0`（源码 `d8177278`，run `38029582629`），GitHub Release 10 个附件；GitCode 镜像校验通过（镜像脚本改为接受协议 2 和 3）；前端 `port/v4` 固定 0.3.0 的 Windows 包和三个 Unix 包，Dev/Release 构建通过。待维护者本机 Windows 实测；WSL/SSH 升级验收暂缓 |
| 2026-10-10 | 第四阶段 | 维护者实测：本机 Windows、WSL 通过；Mac SSH 更新失败，定位为 Unix 端点被 `/tmp` 清理（见“第四阶段发现”）。结束 Mac 上失联的旧常驻后升级到 0.3.0 正常 |
| 2026-10-10 | 第四阶段 | 发布 `auth-core-v0.3.1`（源码 `11733dd7`，分支构建 `38032997732`、发布 run `38034172428`），GitHub Release 10 个附件，GitCode 镜像校验通过；前端 `port/v4` 固定 0.3.1（`b75d37cb`），完整测试只有既有失败 1 个 |
| 2026-10-10 | 完成 | 维护者实测 Windows、WSL、Mac SSH 从 0.3.0 升级到 0.3.1 通过；前后端 `port/v4` 快进合并到 `main`；GitHub Latest 设为 `auth-core-v0.3.1` |
| 2026-10-10 | 后续 | 维护者实测完整配置编辑器通过；发布 `auth-core-v0.3.2`（源码 `df931d26`，run `38040355249`），GitHub Release 10 个附件，GitCode 镜像校验通过，Latest 设为 0.3.2；前端 `main` 固定 0.3.2（`4383d170`），完整测试只有既有失败 1 个，正式包显式集成测试 8 个通过 |
