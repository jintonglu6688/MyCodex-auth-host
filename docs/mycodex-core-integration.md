# MyCodex 原版服务接入验证

2026-09-26：已完成 Windows 原版服务、常驻账号管理、转换路由和 GUI RPC 适配的隔离验证。GUI 必须明确选择协议 2，不能将本产物直接替换旧协议后台；尚非发行版。

## 固定基线与位置

- 上游：`farion1231/cc-switch`，`e0f70019b2758f5b6b9a04dd60e4689481a0c0ac`（3.20.4）。建分支前通过官方远端及 GitHub API 核验。
- 分支：`feature/authentication-core-upstream`；本地工作树 `E:\MyCodex-auth-core`。
- 旧工作树 `E:\MyCodex-auth-host` 保持原功能分支，作为平台适配参考；不迁移旧开发数据。
- 供应商保存/切换、OAuth 和协议转换业务实现沿用上游，保留全部历史及许可。第二阶段仅增加下述 HTTP 门禁接缝和两处内部方法可见性，不复制核心业务。

## 实际调用链

`mycodex-auth-host serve` 持有原版 `AppState` / `Database`，`rpc` 经当前用户 IPC 调用它；`request` 保留为不支持账号/转换的单次离线入口。供应商直接调用 `ProviderService::{list,current,read_live_settings,add,update,switch}`；MCP 导入调用 `McpService::import_from_codex`。

没有复制旧 headless 的供应商状态机、公用片段引擎或文件事务。入口不执行桌面启动恢复，不执行全应用 `sync_current_to_live`。普通启动/查询不额外启用历史当前项。

必要补丁仅为：

| 位置 | 原因 |
|---|---|
| `src-tauri/src/mycodex_host.rs`、`src-tauri/src/bin/mycodex-auth-host.rs` | Codex 专用标准输入入口；原服务参数转换、结果脱敏、写操作互斥、阶段能力限制 |
| `src-tauri/src/mycodex_host/security.rs` | 复用原适配的当前用户目录权限；拒绝未标记的非空目录 |
| `config::get_app_config_dir` | 原版数据库及账号存储定位到指定目标的私有目录 |
| `AppSettings::settings_path` | 原版当前供应商及设置文件也必须隔离，不能落到真实用户目录 |
| `settings::get_codex_override_dir` | Codex 配置和 SkillService 共用同一个显式目标路径 |
| `lib.rs`、`Cargo.toml` | 导出入口，启用已有 windows-sys 依赖的目录权限 API；不增加包依赖 |
| `mycodex_host/ipc.rs`、`security.rs` | 复用旧平台层的 Windows 命名管道/Unix 私有 socket、当前用户权限及所有者校验；新端点命名与旧后台分离 |
| `mycodex_host/accounts.rs` | 直接调用原版 CodexOAuthManager，复用原账号删除时的切换锁；仅映射交互字段和固定错误码 |
| `mycodex_host/lifecycle.rs` | 只对 Codex 启停接管、恢复、直连策略及 HTTP 请求互斥；原 ProviderService 和 ProxyService 继续负责文件/备份业务 |
| `proxy/server.rs` | 仅 MyCodex 入口启用 HTTP middleware；读锁随完整响应 Body 存活，管理写锁拒绝在途请求期间切换，旧 keep-alive 在直连状态不能转发 |
| `CodexOAuthManager::load_from_disk_sync` | 改为 `pub(crate)`，初始化时明确报告损坏存储，避免原构造器仅写日志后继续 |
| `ProxyService::live_takeover_matches_current_proxy` | 改为 `pub(crate)`，复用原判断核验 live 是否仍属于此目标路由；不复制端点匹配算法 |

未启用适配入口时，三个路径函数保持原行为。输入要求两处已存在、绝对且互不包含的目录；解析真实路径后检查，私有库绑定一个 CodexHome，不允许同库改投其他目标。请求最大 1 MiB，凭据只经标准输入传递，响应只输出选定的摘要字段和固定错误码。

## 已验证的原版行为

进程测试使用独立临时目录、虚构 API 域名和合成凭据，不调用真实供应商：

1. 首个供应商新增后自动成为当前项并写入全局文件；后续非当前项保存只改数据库。当前项编辑保存同步写全局。
2. 外部修改默认模型后，查询不会覆盖修改；A→B 时原版回写 A，B→A 恢复修改后的模型。各次操作启动独立进程，验证持久化及当前项记录路径。
3. 共享开启时，原版捕获 `model_reasoning_summary`、插件/Skills 启用设置；关闭的供应商不继承这些设置。共享项删除也会传播，安装文件不因切换删除。
4. MCP 从目标全局文件导入独立数据库，再由原服务在切换时投影；不受供应商公用片段开关控制。首次引入原服务前须处理已有 MCP，不能直接切换后再补导入。
5. 文件型 ChatGPT 合成账号 A→B→A 保持直连且保留 A 更新后的令牌；未创建路由或改成本地代理地址。
6. 转换协议、已有代理接管、损坏 live 配置、错配目标、未授权目录及无效请求被拒绝；拒绝切换不改 live 文件；响应不包含测试凭据。

这修正了旧 GUI 的假设：不能继续统一提示“保存不会应用配置”，也不能说原版取消公用配置后仍全局保留所有插件/Skills 启用设置。

## 复现

Windows 已安装 Rust 1.95，使用项目现有锁文件：

第一阶段结果为 6 项进程测试、17 项原版账号单测通过；第二阶段将进程测试扩展到 **14/14 通过**，并重新验证原版账号单测 **17/17 通过**、原版 HTTP Server 单测 **4/4 通过**。包括真实本机 HTTP mock 的 Chat/Anthropic 转换、后台被终止后的固定端口恢复、完整流式在途保护、旧 keep-alive 阻断、端口占用失败恢复及外部 live 冲突保护。测试全用隔离目录和合成凭据，不调用真实供应商。未运行完整上游测试集或 GUI 测试，未构建其他平台。

```powershell
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin mycodex-auth-host --target-dir D:/Dev/cache/cargo-target-auth-core -j 4
cargo test --locked --manifest-path src-tauri/Cargo.toml --target-dir D:/Dev/cache/cargo-target-auth-core -j 4 --test mycodex_host_core -- --test-threads=1
cargo test --locked --manifest-path src-tauri/Cargo.toml --target-dir D:/Dev/cache/cargo-target-auth-core -j 4 --lib managed_codex_ -- --test-threads=1
cargo test --locked --manifest-path src-tauri/Cargo.toml --target-dir D:/Dev/cache/cargo-target-auth-core -j 4 --lib proxy::server::tests -- --test-threads=1
```

本阶段仍编译原库的 Tauri 依赖，没有为了无界面产物复制核心模块。未来平台打包再处理依赖裁剪。

安全的状态查询示例（只使用新的空目录）：

```powershell
$probeRoot = Join-Path $env:TEMP ('mycodex-core-' + [guid]::NewGuid().ToString('N'))
$probeData = Join-Path $probeRoot 'store'
$probeCodex = Join-Path $probeRoot 'codex'
New-Item -ItemType Directory -Path $probeData, $probeCodex | Out-Null
'{"jsonrpc":"2.0","id":1,"method":"status"}' | & D:/Dev/cache/cargo-target-auth-core/debug/mycodex-auth-host.exe request --data-dir $probeData --codex-home $probeCodex
```

入口支持 `status`、`provider/list`、`provider/live`、`provider/add`、`provider/update`、`provider/switch`、`mcp/import`。供应商输入沿用原版 `Provider` JSON，协议格式使用原版的 `openai_responses`、`openai_chat`、`anthropic`，拒绝重复 ID 的 add（编辑须调用 update）。不是旧后台 RPC 契约，禁止直接替换 GUI 配置里的后台路径。

常驻入口沿用示例里的隔离目录：

```powershell
# 启动测试后台；此终端持续运行，不需要启动 CC Switch GUI。
& D:/Dev/cache/cargo-target-auth-core/debug/mycodex-auth-host.exe serve --data-dir $probeData --codex-home $probeCodex
# 另一个终端设置同样的绝对目录后，通过 rpc 查询。
'{"jsonrpc":"2.0","id":1,"method":"status"}' | & D:/Dev/cache/cargo-target-auth-core/debug/mycodex-auth-host.exe rpc --data-dir $probeData --codex-home $probeCodex
```

常驻模式额外支持 `account/list`、`account/login/start`、`account/login/poll`、`account/login/cancel`、`account/remove`、`account/default`、`backend/shutdown`。原生开始登录返回设备码、用户码、浏览器地址和轮询间隔；调用者按间隔轮询同一后台，后台重启后未完成登录必须重新开始。OAuth 网络请求、凭据刷新和落盘全部执行原版；GUI 使用下述 `gui/account/*` 投影，不接收原生设备码。

`serve` 不随一次 `rpc` 或 GUI 客户端退出而结束。管理请求串行执行，OAuth 网络等待期间其他管理请求排队；路由 HTTP 独立运行。IPC 回应等待上限 90 秒；超时不代表写操作未发生，调用者须查实际当前状态，不能盲目重发。GUI 切换回执仅保存在本次后台内存中，详见下节。

## GUI 投影契约

常驻模式增加以下方法，原生 RPC 保持不变。`gui.rs` 仅编排原版服务，`gui_provider.rs` 映射表单，`gui_accounts.rs` 映射原生登录任务，`gui_catalog.rs` 读取最新模板及模型服务，没有复制旧事务、公用配置或 OAuth 引擎。

| 方法 | 行为 |
|---|---|
| `gui/provider/list,get` | 列表报告原版当前项；编辑当前直连项读取最新 live 并调用原版公用片段剥离，代理接管态读取数据库，与原版编辑器一致；版本同时覆盖存档和所编辑快照 |
| `gui/provider/save` | `expectedVersion` 校验后调用原版 add/update；保留未表示字段和留空密钥；首项新增及当前项保存可能写全局，以 `globalApplied` 告知调用方重连 |
| `gui/provider/copy,delete` | 调用原版新增/删除；当前项禁止删除；供应商删除不联动删除账号，`accountRemoved=false` |
| `gui/provider/preflight,apply` | 核对版本、目标文件及原公用片段指纹；通过后执行原版 switch，返回 `status=applied` |
| `gui/operation/get` | 查本次后台最近 128 次切换回执；不存在、被淘汰、后台重启或写后错误一律 `operation_unknown`，不推断操作未发生 |
| `gui/preset/list,model/fetch` | 95 个当前模板入口；原版模板 auth/config/meta/modelCatalog 原样作为新增基础，再覆盖用户编辑字段；模型服务只映射实际返回数据，不虚构上下文等能力 |
| `gui/account/list,login/start,login/get,login/cancel,delete` | GUI loginId 映射原生设备任务；状态包含失败、取消、过期；最多保留 32 个任务，优先淘汰已结束或过期项；独立账号删除执行原版 remove_account |

首项写入前先执行原版 `McpService::import_from_codex`，新增后补原版单 Codex MCP 投影，避免空数据库覆盖目标已有 MCP。插件/Skills 及公用偏好继续遵从原版语义，不保留旧版强制全局共享例外。查询不会自动识别或归档外部配置。

API 高级 headers/query 值在输出中遮蔽，模型字段按模型 ID 恢复遮蔽值；模型获取只有同供应商类型及相同 URL 才可继承已存凭据。编辑不同目标必须显式输入新凭据才能获取模型。headless HTTP 客户端禁止跨源重定向，避免自定义认证头外泄。

版本或指纹冲突在写入前拒绝；原版写入后遇到错误返回 `save_outcome_unknown` / `operation_unknown`。保存没有另造持久化回执；客户端应关闭编辑器并刷新实际状态，不能根据超时自动重试。账号删除遵从上游：可能清除该账号拥有的全局认证，不为已有供应商引用增加阻止规则，相关供应商可能需要重新登录。

GUI 进程测试 **2/2**、模型/模板及字段投影单测 **8/8**、HTTP 重定向安全测试 **2/2** 通过。进程覆盖首次保存保留已有 MCP、外部默认模型编辑、陈旧版本拒绝、未知字段/留空密钥保留、预检与切换回执、复制删除、离线账号失败、任务淘汰及重启后未知结果。模型列表来自固定上游 TypeScript，`node scripts/Export-MyCodexPresets.mjs --check` 已通过。

统一编译测试产物后可直接运行测试 exe，避免每次 Cargo 调用刷新身份导致重复编译：

```powershell
cargo test --locked --manifest-path src-tauri/Cargo.toml --target-dir D:/Dev/cache/cargo-target-auth-core -j 4 --lib --test mycodex_host_core --test mycodex_gui --no-run
```

## MCP GUI 原版存档接入

协议 2 常驻模式另提供 `guiMcpManagement` 能力，使用当前目标的原版 MCP 数据库和 `McpService`：

- `gui/mcp/list` 返回 `{servers:[{id,name,config,enabled}],version}`，包含已禁用的存档；查询不导入、不改 live。`config` 使用 Codex 字段名，例如 `http_headers`，通过私有 IPC 传递完整编辑配置，禁止记录这些内容。
- `gui/mcp/save` 接收 `serverId/config/expectedVersion` 和可选 `enabled`。顶层启用值优先于 config.enabled，缺省保留原值，新项默认启用。启用归一到原版 apps.codex；禁用保留数据库条目并从 live 移除，重新启用恢复原存档。返回 `{saved:true}`。
- `gui/mcp/delete` 接收 `serverId/expectedVersion`，原版删除数据库及对应 live 项，返回 `{deleted:true}`。
- `gui/mcp/import` 接收 `expectedVersion`，仅显式调用原版 Codex 导入并返回 `{imported:新建数量}`。已有 ID 保留原存档参数并启用 Codex，不连续监控文件，不以外部参数覆盖存档，也不反向写 live。

版本覆盖整个 MCP 数据库和 live 的两种 MCP 表（标准及历史格式）；外部 MCP 修改和陈旧存档均拒绝覆盖，普通非 MCP 偏好变化不导致冲突。live 已存在但尚未导入的同名项拒绝保存（`mcp_not_imported`），须先显式导入。已经禁用的存档若被外部重新写入 live，显式保存禁用仍调用原版 toggle_app 清理该项。写操作使用现有 lifecycle 门禁，配置校验/冲突在写前返回固定错误；服务执行后错误保守返回 `operation_unknown`。若存档含其他应用启用标记则拒绝写入，避免原版多应用服务触及其他真实目录。

Codex 原 importer 对缺失 type 的条目默认 stdio，本次修复为存在 url 时推断 http；不改其他客户端协议。原未知扩展转换只保留浅层值，本次改为保留嵌套 TOML 支持值及空容器，仍复用原导入/投影流程。保存时预先拒绝 null、非法 env/header 类型和矛盾 transport，避免原服务落库后静默丢字段。导入到库的 `enabled=false` 会在 GUI 显示禁用；显式重新启用时移除这项旧 spec 标记，以原版应用启用状态为准。

自动验证使用独立目录和不会执行的 MCP 命令，覆盖 URL-only HTTP、header/env 及嵌套扩展保留、显式导入不写 live、A/B 切换后存档持续生效、编辑/禁用/启用/删除及陈旧版本拒绝。未连接真实 MCP 服务器，不修改用户手测目录。独立包使用 `artifacts/auth-core-mcp-v2-reviewed`，不替换正在运行的后台。

## 第二阶段路由策略

- 官方 ChatGPT、官方 API 和第三方原生 Responses 直连；只对 Chat Completions/Anthropic 第三方启动原版转换路由。官方卡上的转换声明直接拒绝，托管账号只接受 Codex OAuth，自动故障转移不开放。
- 只监听 `127.0.0.1`，第一次启动使用系统分配端口，原版持久化实际端口。只调用单 Codex 接管/恢复，禁止全应用启动/恢复影响 Claude 等真实目录。
- HTTP middleware 只开放 Codex Responses、模型列表和健康检查；不开放其他智能体及额外透传端点。转换算法保持原版。门禁写锁覆盖整个配置改变，读锁覆盖完整 HTTP 响应流，不以状态计数作为并发屏障。
- 路由使用独立持久化的随机目标凭据，以专用头写入本目标 live；入站核验后剥离，禁止该保留头进入供应商存档和 GUI。匿名请求及其他目标的凭据不能调用转换。单次 `request` 也持有目标锁，不能通过另一个 store 绕过常驻后台互斥。
- 切直连先关闭转发许可、用原版单应用方法恢复接管，停止监听，然后执行原版供应商操作。旧连接即使仍存活也不能转发直连供应商。状态分别报告实际监听 `running` 和转发许可 `accepting`，不把降级误报为正常。
- 后台异常终止后，仅在 live 与保存的本目标代理端点一致时恢复路由；若已恢复成与 backup 一致的原配置，则完成清理且不重新接管。若 backup 已删除且 live 不含代理标记，仅清残留 enabled 元数据，不改文件。仍有备份但外部 live 与备份/代理端点均不匹配，或 live 损坏时，保留文件与备份并报告冲突，不强行应用历史当前项。
- 驻留期间遇到上述外部冲突，管理切换/关闭被拒，转发关闭；启动时遇到冲突则不报告 ready。此阶段尚无 GUI 冲突解决入口，需要后续接线时补齐可操作的恢复交互。
- `backend/shutdown` 是显式维护关闭：无在途请求时恢复 Codex 并关闭接管；普通 GUI 关闭不应调用它。测试 kill 用来验证异常重启，不代表正式 OS/安装器验收。

## 编译身份和独立打包

新入口的产物及内部 IPC 均使用协议 **2**，Host 版本 **0.2.0**。`--version-json` 不需要目录参数，不打开数据库；返回 `hostVersion`、`protocolVersion`、`upstreamRevision`、`sourceRevision`、`sourceDirty`、`target`、`sha256`。`ready` 和 `status` 包含相同身份，另带 `codexHome` 和 `dataDir`。固定上游仍是本文开头的提交。

`build.rs` 在编译时读取 Git revision/dirty 和 Cargo TARGET 并写入产物；运行时不查询仓库，也不接受环境变量覆盖。构建脚本每次刷新身份，避免未跟踪文件或 worktree 变化留下旧的 clean 标记；不能读取 Git 时报告 unknown/dirty，打包脚本拒绝这类产物。SHA256 在首次查询时计算当前可执行文件并缓存。身份和校验和用于发现错包或不一致，不是发行签名。

```powershell
& D:/Dev/cache/cargo-target-auth-core/debug/mycodex-auth-host.exe --version-json
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/Package-MyCodexAuthCore.ps1 -BinaryPath D:/Dev/cache/cargo-target-auth-core/debug/mycodex-auth-host.exe -OutputDirectory E:/MyCodex-auth-core/artifacts/auth-core-v2
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/Package-MyCodexAuthCore.ps1 -OutputDirectory E:/MyCodex-auth-core/artifacts/auth-core-v2 -VerifyOnly
```

打包仅接受全新或空目录，复制可执行文件和上游 LICENSE，生成 `mycodex-auth-host.manifest.json`（schemaVersion 1）。清单身份来自指定产物，脚本独立计算 SHA256 与产物自报值比对，不从当前 checkout 补写旧产物的身份。`VerifyOnly` 校验清单、哈希及 LICENSE，不启动后台。脚本不构建、不部署、不覆盖现有目录；其他平台需在能够运行相应产物的主机生成清单。

本轮 Windows 进程测试 **15/15 通过**，新增身份测试验证：任意工作目录和运行时伪造环境变量不能改变编译身份，`--version-json` 无数据目录副作用，产物哈希正确，单次请求与常驻状态身份一致。打包与独立校验通过；篡改哈希、缺少 LICENSE、覆盖已有包均确认被拒绝。尚未运行其他平台产物。

本轮补充路由认证、同目标单次入口锁及清理回归后，核心进程测试 **18/18 通过**。当前独立 GUI 包为 `artifacts/auth-core-gui-v2`，协议 2，包含 manifest 与 LICENSE；打包及 `VerifyOnly` 均通过。该目录不覆盖旧 runtime，GUI 部署由 MyCodex 仓库入口负责。

## 尚未验证及接续边界

- `request` 保持原单次限制；`serve/rpc` 已持有原版账号状态和路由，GUI 协议已接入；没有为测试加入令牌注入接口。实际 WinForms 界面的测试和部署证据记录在 MyCodex 仓库。
- 文件型合成账号的进程测试与原版托管账号单元测试是两层证据，均不等于真实 OAuth 登录、客户端对话或端到端直连验收。
- 新后台账号开始/轮询/取消方法已连到原服务，自动测试覆盖账号归档读取/默认账号/删除及无效登录任务；没有真实浏览器授权验证。托管账号真实刷新与目标会话仍需人工验收。
- Windows MCP 管理已通过原版 MCP 数据库服务；Plugins/Skills 保持 App Server 的安装、文件与启停入口，其配置依照原版公用片段开关共享，不另造插件/Skills 存档。实际 App Server 联调和用户验收记录见 MyCodex 仓库。
- 本后台任务未修改真实账号数据，未部署 WSL/Linux/Mac SSH 或验证安装更新。Unix IPC 源码复用已接入，但本轮未编译或部署 Unix 产物；后续统一工具入口并重新验收各平台。
- 本阶段不提供额外跨进程回滚保证；原版操作结果和并发冲突行为仍需在常驻/远程集成阶段逐项验证。

后续沿用这里证明的原服务调用链，完成 WinForms 手测、统一工具入口与冲突恢复交互，使用干净数据做真实账号与多平台验收，最后删除旧重复业务流程。

MCP GUI integration verification: GUI process tests 3/3, MCP mapping 1/1, existing Codex MCP tests 5/5 passed. The final reviewed package additionally passed the MCP process regression for unimported-ID rejection and explicit disabling after external recreation. Package verification passed; no real MCP process or user backend was started/stopped by these tests.

## Windows 工具配置收尾（2026-09-26）

GUI 当前供应商保存、重复启用及切换前，先完成版本/指纹与字段检查，再复用原版 `sync_common_config_snippet_from_live` 捕获当前显式应用公用配置的 live 改动。读取列表/编辑快照不写库，陈旧请求不捕获；显式清空公用片段保持原版例外。关闭公用开关前按旧启用状态捕获，再剥离共享内容；从关闭重新开启时仍恢复既有共享内容，不把关闭期间的私有偏好反写共享。

转换期间从已验证归属的真实 live 提取工具偏好，沿用原提取器排除模型供应商路由、凭据与 MCP；公用片段改变时调用原版 `update_live_backup_from_provider` 刷新接管备份。这样切直连先恢复备份时不会恢复旧插件/Skills 状态，转换间切换和后台重启也使用相同原服务。没有另造共享配置合并或备份引擎。

另修复原版 TOML 公用配置剥离对 `[[skills.config]]` 数组表的遗漏：沿用按值匹配语义移除共享条目、保留私有项和不同值，并正确处理重复项及空表。否则已删除的禁用项可能留在供应商私有存档，切回时重新出现。

GUI 进程回归 5/5 通过，覆盖当前改名/重复启用、直连/转换、开关双向变化、过期请求与只读查询、显式清空，以及转换之间切换、异常重启、切直连和工具配置删除不复活。测试均为独立目录和合成配置，不执行合成工具，不访问外部供应商，也不改用户账号。

原生命周期进程回归 18/18 通过，公用配置单元检查 19/19 通过。新独立包位于 `artifacts/auth-core-windows-tools-reviewed`，已通过 `VerifyOnly`；未覆盖运行中的旧包，GUI 部署与真实 App Server 联调由 MyCodex 仓库完成。
