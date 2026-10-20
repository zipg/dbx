# DBX Windows 启动失败（360 / 奇安信 / 卡巴斯基 干扰 WebView2）技术调研报告

> 范围：Tauri v2 (tauri 2.11.5 / wry 0.55.1 / webview2-com 0.38.2) + Evergreen WebView2，Windows 10 中文企业环境。
> 目标：给出**按优先级排序的启动 profile 阶梯**、**明确无效的做法**、**360/奇安信/卡巴的用户侧信任设置**、**可落地的诊断字段**。
> 说明：本会话的 `web_search` / `web_fetch` 工具后端不可用（endpoint `apitokenzz.com` fetch failed），全部资料通过 shell `curl` + 自建抓取/搜索脚本获取；少数中文站点只能通过搜索结果页间接命中，已在正文标注来源可信度。

---

## 结论摘要（Top 5，按"成功率 × 成本"排序）

| # | 方案 | 为什么它最可能有效 | 置信度 |
|---|---|---|---|
| 1 | **固定版本 WebView2 运行时（Fixed Version Runtime，109 / 117）** + `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`（或以 API 形式传入 `browserExecutableFolder`） | 这是目前**唯一有现场证据**能让 DBX 起来的路径：老 Win7 安装包内置 109 固定运行时即可启动。机制上也自洽——渲染器代码完整性（RendererCodeIntegrity）在 WebView2 **118 起被强制、无法关闭**，109/117 仍可关闭；中文 DLP/加密软件排查实录同样得出"旧版 Runtime 正常，新版 FAIL_FAST" | 高 |
| 2 | **把参数从"环境变量"改为"API / HKLM 策略"传入**（`additional_browser_args` 或 `HKLM\Software\Policies\Microsoft\Edge\WebView2\AdditionalBrowserArguments`） | 微软明确：**进程提权（High IL）时所有 `WEBVIEW2_*` 环境变量覆盖被忽略**，HKCU 策略也被忽略，只有"代码里传的 AdditionalBrowserArguments"和"HKLM 策略"生效。中文企业用户大量"以管理员身份运行"，这一条可以解释"企业兼容模式完全没效果"。DBX 的 wry 已有 patch 把 env 转成 API 参数，但仍需确认 elevated 分支 | 中高 |
| 3 | **AV 侧信任：360 信任区 + 核晶防护例外 / 奇安信天擎"添加信任程序" / 卡巴"受信任的应用程序"**（对 `dbx.exe` 与 `msedgewebview2.exe` 同时加白） | 微软官方文档把"注入 DLL / 阻断子进程 / 收紧 ACL"列为 WebView2 白屏、冻结、初始化失败的头号原因，并明确要求"允许 renderer/GPU/network/Crashpad 子进程、不要注入 DLL、按签名信任 `msedgewebview2.exe`"。天锐绿盾案例证明：**同版本 Edge 正常、只有 WebView2 应用崩**，因为 Edge 在白名单里 | 中高（治本，但依赖用户/IT 操作） |
| 4 | **`AppCompatFlags\Layers` 把 `msedgewebview2.exe` 设为 "Windows 8" 兼容模式** | WebView2Feedback #4094 最新回复（2026-07）实测："set the compatibility mode of msedgewebview2.exe to Win8 works for me"。成本极低、可脚本化、可回滚；缺点是每次运行时更新会重建 `msedgewebview2.exe`，注册表项可能被覆盖，需要在启动时重新写入 | 中（单一来源，但可 5 分钟验证） |
| 5 | **修正判定信号 + 预热**：`visible=Err(FailedToReceiveMessage)` 说明**原生窗口已被销毁**（不是"隐藏"），此时不应继续查询可见性，而应销毁并重建窗口；同时用"预热子进程 + 20s 延迟"把 AV 首次云扫描窗口让过去 | 源码级确认：`FailedToReceiveMessage` = `send_user_message` 成功但响应通道被丢弃，即运行时窗口表里已没有该 window id → 原生窗口已死。继续 `is_visible()` 只会得到同样错误，永远等不到窗口。预热/延迟对"首次扫描型"拦截（而非注入型）有效 | 中 |

---

## 一、可直接尝试的启动层级（按优先级排序的 profile 清单）

> 现有 `src-tauri/src/startup_recovery.rs` 的阶梯已有 9 个 profile：`isolated-no-sandbox` → `renderer-code-integrity-off` → `subprocess-sandbox-off` → `temp-user-data` → `av-scan-delay` → `warmup-preflight` → `elevated-relaunch` → `temp-relocated` → `single-process-experimental`。
> 下面的 P0–P12 是**对该阶梯的修订建议**：把"已被证据证明会失效"的降级，把"已被证据证明有效"的提前，并补上缺失类别。

### 1.0 环境变量 / API 选项速查（阶梯里反复用到）

| 名称 | 作用 | 关键注意点 |
|---|---|---|
| `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` | 追加 Chromium 开关 | 官方文档说会**追加**到 API 值；但 wry **总是**设置 `AdditionalBrowserArguments`，实测该 env 常被忽略（DBX 已在 `vendor/wry` 打补丁合并，见 `merge_browser_arguments`）。**提权进程下该 env 一律被忽略** |
| `WEBVIEW2_USER_DATA_FOLDER` | 覆盖 UDF | 同样在提权时被忽略；切 profile 必须换 UDF（否则 `0x8007139F`） |
| `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` | 指定固定版本运行时目录 | 路径**不能**含 `\Edge\Application\`；提权时被忽略（改用 HKLM 策略/API） |
| `WEBVIEW2_CHANNEL_SEARCH_KIND` | `0`=最稳定优先（默认），`1`=最不稳定优先 | 与下一项配合 |
| `WEBVIEW2_RELEASE_CHANNELS` | 限定可用通道集合（新变量） | 例：`beta` / `dev` / `canary` |
| `WEBVIEW2_RELEASE_CHANNEL_PREFERENCE` | **旧版**等价物，`1` = 取最不稳定通道 | 仅作兼容保留，新代码用上面两个 |
| `WEBVIEW2_DEFAULT_BACKGROUND_COLOR` | 设置 WebView 默认背景色（如 `0xFFFFFFFF`） | **不修复任何拦截**，只消除白/黑闪；可用作"窗口已创建但内容未渲染"的可视化判据 |
| `WEBVIEW2_WAIT_FOR_SCRIPT_DEBUGGER` / `WEBVIEW2_PIPE_FOR_SCRIPT_DEBUGGER` | 调试用（启动即暂停等待调试器） | **千万不要**在生产镜像里设置，会造成"永远起不来" |
| `CoreWebView2EnvironmentOptions.ExclusiveUserDataFolderAccess` | 独占 UDF，阻止其他进程共享同一浏览器进程 | 可避免"别的 WebView2 进程占着 UDF 导致参数不匹配"；默认 `false` |
| `--user-data-dir`（Chromium 开关） | ❌ 不是 WebView2 的 UDF 机制 | 见第二节 |

来源：[CreateCoreWebView2EnvironmentWithOptions](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)、[Develop secure WebView2 apps](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/security)、[wry `create_environment` 实现](https://github.com/tauri-apps/wry/blob/dev/src/webview2/mod.rs)

### P0（先做，非"修复"）：把判定信号换成"窗口重建 + WebView2 事件"，并开日志

- **参数**：`--enable-logging --v=1 --log-file=%LOCALAPPDATA%\com.dbx.app\webview2-debug.log`（放 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 或 profile args）
- **预期作用**：拿到 `msedgewebview2.exe` 自己的启动日志（GL/沙箱/COM 失败点），能区分"环境创建被阻塞" vs "浏览器进程被拦" vs "渲染器被注入杀死"。
- **风险**：日志体积、包含路径信息（需脱敏）。
- **为什么必须先做**：当前 `visible()` 永远返回 `FailedToReceiveMessage`，等于**所有 profile 的成功/失败判定都建立在一个恒为 Err 的信号上**。正确判定应是：`WebView2 ProcessFailed` 事件 + `msedgewebview2.exe` 进程存活/数量 + `IsWindow(hwnd)` + Crashpad dump 目录。
- **来源**：[WebView2 browser flags（`enable-logging`）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/webview-features-flags)、[tauri-runtime-wry getter 实现（`FailedToReceiveMessage` 语义）](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri-runtime-wry/src/lib.rs)

### P1（最高优先级新 profile）：固定版本运行时 109（或 117）

- **参数**：
  - 环境变量：`WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=C:\Program Files (x86)\DBX\WebView2Fixed\109.0.1518.78`（**路径中不能含 `\Edge\Application\`，否则 API 返回 `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`**）
  - 或在代码里：`WebviewWindowBuilder::additional_browser_args(...)` 同级的 `browserExecutableFolder`（wry 已读 `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`，见 `vendor/wry/src/webview2/mod.rs:1924 configured_browser_executable_folder()`）
  - 打包形态：`tauri.conf.json` → `bundle.windows.webviewInstallMode = { "type": "fixedRuntime", "path": "./Microsoft.WebView2.FixedVersionRuntime.109.0.1518.78.x64/" }`（`.cab` 需先解压，字段填解压后的目录）
  - 配套参数：`--no-sandbox --disable-gpu --disable-features=RendererCodeIntegrity,msSmartScreenProtection`
- **预期作用**：绕开 118+ 强制的渲染器代码完整性；老内核容忍第三方注入（现场 109 已成功）。
- **风险**：**没有安全更新**（固定版本由你自己负责升级）、内核老旧（渲染/JS 特性缺失）、安装包 +180MB。仅作为"企业逃生舱"，不要默认开启。
- **验证要点**：固定运行时下 `msedgewebview2.exe` 的路径应指向该目录（用 `Win32_Process.ExecutablePath` 确认，而不是 `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\...`）。
- **来源**：[Evergreen vs. fixed version](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/evergreen-vs-fixed-version)、[Distribution / fixed version](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)、[Tauri WebviewInstallMode（fixedRuntime）](https://tauri.app/reference/config/)、[CreateCoreWebView2EnvironmentWithOptions（browserExecutableFolder 语义）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)

### P2：固定版本运行时 117（"最后一个还能关掉 RendererCodeIntegrity"的版本）

- **参数**：同上，`WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=...\117.0.2045.60`
- **预期作用**：比 109 新 8 个大版本，仍可 `--disable-features=RendererCodeIntegrity`（WebView2Feedback #4094 中报告者确认 "Last working version 117.0.2045.60"）。
- **风险**：同样无安全更新；117 已 EOL 两年。
- **来源**：[WebView2Feedback #4094](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4094)

### P3：用 HKLM 策略注入参数（解决"提权后 env 被忽略"）

- **参数**：注册表
  ```
  HKLM\Software\Policies\Microsoft\Edge\WebView2\AdditionalBrowserArguments
      "<AppId>" = "--no-sandbox --disable-gpu --disable-gpu-sandbox --disable-features=RendererCodeIntegrity,msSmartScreenProtection"
  HKLM\Software\Policies\Microsoft\Edge\WebView2\BrowserExecutableFolder
      "<AppId>" = "C:\...\WebView2Fixed\109.0.1518.78"
  ```
  （`<AppId>` 的取值规则见 "Enterprise management of WebView2"；未打包应用需按文档核对，不要凭猜。）
- **预期作用**：**提权进程也生效**（微软明确 HKLM 策略在 High IL 下仍被尊重），且不受 wry "总是设置 AdditionalBrowserArguments" 的影响。
- **风险**：需要管理员权限写 HKLM；策略是机器级的，会影响同 AppId 的所有实例；卸载残留需清理。
- **来源**：[CreateCoreWebView2EnvironmentWithOptions（registry overrides 段）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)、[Develop secure WebView2 apps（elevated 覆盖表）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/security)

### P4：AppCompat 兼容性层（`msedgewebview2.exe` → Windows 8）

- **参数**：
  ```
  HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers
      "C:\Program Files (x86)\Microsoft\EdgeWebView\Application\<ver>\msedgewebview2.exe" = "~ WIN8RTM"
  ```
  （32/64 位视图都要写；运行时升级后路径变化需重写，建议每次启动自检重写）
- **预期作用**：让 WebView2 走老系统的 API 行为分支，绕过某些只在"现代 Windows"路径上触发的 hook/兼容层；社区实测可解决 `0xC0000428` 类崩溃。
- **风险**：非官方支持手段；可能影响渲染/输入法行为；升级即失效。
- **来源**：[WebView2Feedback #4094 末条评论（2026-07-07，"set the compatibility mode of msedgewebview2.exe to Win8 works for me"）](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4094)、相关 `edge-webview-no-dpi-workaround`（说明 WebView2 自身就有"通过 shell 启动以规避 appcompat shim"的逻辑）：[browser flags](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/webview-features-flags)

### P5：预热 + 延迟（针对"首次云查杀"型拦截）

- **参数**：`DBX_STARTUP_COMPAT_PROFILE` 已有 `warmup-preflight` 与 `av-scan-delay(20s)`；建议**两者叠加**：`start_delay=20s` + `warmup=true`，warmup 子进程用 `--no-sandbox --disable-gpu` 先创建并释放一次 WebView2 环境。
- **预期作用**：让 AV 的"首次见到该 exe → 上传云端 → 返回信誉"这段时间发生在真窗口创建之前；warmup 的副作用是让 `msedgewebview2.exe` 提前进入信任缓存。
- **风险**：启动慢 20s+；对"注入型"拦截无效（注入不看第几次启动）。
- **判据**：若第二次启动（同一台机、不重启）就正常，则属于"首次扫描型"，此 profile 命中。
- **来源**：微软"Slow startup"条款（深度实时扫描 Runtime 与 UDF 导致慢启动）：[Prevent security tools from blocking WebView2 apps](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools)

### P6：GPU 链路整体降级（新增 `--in-process-gpu` 变体）

- **参数**：`--disable-gpu --disable-gpu-sandbox --in-process-gpu`（可再加 `--disable-direct-composition`）
- **预期作用**：把 GPU 进程收回浏览器进程，**少一个会被注入/被拦截的子进程**；`--disable-gpu` 单独使用仍会拉起 GPU 进程（软件渲染），`--in-process-gpu` 才是"真的不单开进程"。WebView2 自身在 Wine 场景就会附加 `--in-process-gpu`，说明该开关被接受。
- **风险**：视频/Canvas 性能下降；个别驱动组合下会黑屏（此时退回 `--disable-gpu`）。
- **来源**：[WebView2Feedback #5720（runtime 会自行附加 `--in-process-gpu --use-gl=swiftshader`）](https://github.com/MicrosoftEdge/WebView2Feedback/issues/5720)、[Chromium switches: `--disable-gpu-sandbox`](https://peter.sh/experiments/chromium-command-line-switches/)

### P7：减少网络/组件/首启动作

- **参数**：`--no-first-run --disable-background-networking --disable-component-update --disable-features=RendererCodeIntegrity,msSmartScreenProtection`
- **预期作用**：首启 profile 初始化、组件更新下载、后台网络是"启动瞬间最容易被云查杀盯上"的动作；关掉可缩短关键路径。
- **风险**：组件更新关闭后字体/宽表等组件不再更新；对注入无效。
- **来源**：[browser flags（`no-first-run` 语义）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/webview-features-flags)、[Chromium switches（`--disable-component-update` / `--disable-background-networking`）](https://peter.sh/experiments/chromium-command-line-switches/)

### P8：每个 profile 使用**独立 UDF** + `ExclusiveUserDataFolderAccess`

- **参数**：`WEBVIEW2_USER_DATA_FOLDER=%LOCALAPPDATA%\com.dbx.app\webview2-compat\<profile-id>`；API 侧 `CoreWebView2EnvironmentOptions.ExclusiveUserDataFolderAccess = true`
- **为什么必须**：**同一 UDF 上已有浏览器进程且参数不同 → 创建直接失败 `ERROR_INVALID_STATE (0x8007139F)`**。切换 flag 而不换 UDF，等于自己制造失败。Tauri 文档也写明："WebViews with different values for settings like `additionalBrowserArgs` … must have different data directories"。
- **风险**：磁盘占用；多份缓存。
- **来源**：[CreateCoreWebView2EnvironmentWithOptions（ERROR_INVALID_STATE 注记）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)、[Tauri config `dataDirectory` 注记](https://tauri.app/reference/config/)、[WebView2Feedback #4174（同 UDF 不同参数导致 0x8007139F 的实证）](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4174)

### P9：换通道（Beta / Dev / Canary）

- **参数**：`WEBVIEW2_RELEASE_CHANNELS=beta`（新变量）+ `WEBVIEW2_CHANNEL_SEARCH_KIND=1`；旧变量 `WEBVIEW2_RELEASE_CHANNEL_PREFERENCE=1`（= 取最不稳定通道）
- **预期作用**：不同通道的完整性/兼容策略落地时间不同（#4094 中 Canary 120 在同样被注入的机器上能初始化，Stable 118 不能）。属于"换一条实现路径"的赌注。
- **风险**：预览通道不稳定、企业机器上通常没装；需要用户装 Beta/Dev 运行时。
- **来源**：[CreateCoreWebView2EnvironmentWithOptions（channel 搜索顺序与覆盖变量）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)、[#4094](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4094)

### P10：换安装位置 / 从 `%PROGRAMDATA%`、`%TEMP%` 运行

- **参数**：复制 exe + 依赖到 `%PROGRAMDATA%\DBX\`（或 `%TEMP%\DBX-<rand>\`）后重启；`temp-relocated` 已实现 temp 分支。
- **预期作用**：绕开"按路径写死的 AV 规则"（针对 `%LOCALAPPDATA%\Programs\DBX\dbx.exe` 的拦截）。
- **风险**：`%TEMP%` 路径本身在部分 AV 里信誉更低（可能更糟）；企业策略可能禁止从 temp 执行。
- **重要副作用**：WebView2 默认 UDF 是 **exe 同目录**（`{exe}.WebView2`），若 exe 在 `Program Files` 且非提权运行，UDF 创建会因权限失败——这本身就能造成"启动即失败"。换目录同时也是在换 UDF 权限。
- **来源**：[CreateCoreWebView2EnvironmentWithOptions（默认 UDF 位置与权限失败）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)、[Prevent security tools…（UDF ACL / LowIL 章节）](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools)

### P11：提权重启（谨慎，且必须走 API 传参）

- **参数**：`ShellExecuteW(..., "runas", ...)`；args 必须通过**代码**传给 WebView2，不能只靠 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`（提权后该变量被忽略）。
- **预期作用**：少数 AV 对"高完整性进程的子进程"放宽；也能解决 Program Files 下 UDF 写权限问题。
- **风险**：提权后 env 覆盖失效（**最容易造成"看起来执行了但参数没生效"的假阴性**）；UAC 弹窗；与"最小权限"安全建议冲突。
- **来源**：[Develop secure WebView2 apps（elevated 覆盖表）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/security)

### P12：登录后延迟启动 / 计划任务

- **参数**：任务计划程序触发器"登录后延迟 2–5 分钟"，或进程内 `start_delay`。
- **预期作用**：避开开机时 AV 的全盘/云扫描高峰（企业机器开机 3 分钟内 CPU/IO 被扫描占满，35s 级阻塞在此窗口内完全可能）。
- **风险**：体验差；不解决注入。
- **来源**：微软"Slow startup"（深度实时扫描）：[Prevent security tools…](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools)

---

## 二、不具备可行性 / 可能无效的做法（别浪费尝试）

| 做法 | 结论 | 依据 |
|---|---|---|
| `--disable-features=RendererCodeIntegrity`（在 Evergreen ≥118 上） | **无效**。Chromium 118 起移除该开关、强制开启完整性；Edge 119 起 `RendererCodeIntegrityEnabled` 策略也被移除/忽略 | 官方：[Code Integrity in WebView2](https://github.com/MicrosoftEdge/WebView2Feedback/blob/main/diagnostics/code_integrity.md)（"Starting versions 118 and 119 the options to disable Code Integrity in WebView2 are deprecated and the guard is always on"）；[RendererCodeIntegrityEnabled 策略](https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/renderercodeintegrityenabled)（"removed in Microsoft Edge version 119 and is ignored if set"）；[#4094](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4094) |
| `--single-process` | **不支持/高风险**。WebView2 是多进程架构（browser/renderer/GPU/network/Crashpad），官方 browser flags 清单里没有该开关；即使被接受也会让"逐进程拦截"变成"整进程一起死" | [browser flags 清单](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/webview-features-flags)；建议把 `single-process-experimental` 从阶梯**移除或最后置** |
| `--no-zygote` | **Windows 上是 no-op**。zygote 是 Linux/ChromeOS 的进程孵化机制，Windows 走的是 `CreateProcess` + job object | [Chromium switches](https://peter.sh/experiments/chromium-command-line-switches/)（无 Windows zygote 语义） |
| `--disable-dev-shm-usage` | **Linux 专用**（`/dev/shm`），Windows 无效 | [Chromium switches: `--disable-dev-shm-usage`](https://peter.sh/experiments/chromium-command-line-switches/) |
| `--user-data-dir=<path>` | **不是 WebView2 的支持方式**。UDF 由 `CreateCoreWebView2EnvironmentWithOptions` 的 `userDataFolder`（或 `WEBVIEW2_USER_DATA_FOLDER`/策略）决定；用 Chromium 开关改 UDF 不被支持 | [CreateCoreWebView2EnvironmentWithOptions](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl) |
| 单独加 `--disable-features=msSmartScreenProtection,msWebOOUI,msPdfOOUI` | **已经是 wry 默认值**，重复添加无收益（但注意：一旦你自己设置 `additional_browser_args`，wry 就**不再**追加默认值，必须自己带上这三个 feature） | 源码：`vendor/wry/src/webview2/mod.rs:296-299` |
| `--disable-web-security` | 与 AV 无关，且削弱安全边界 | [browser flags](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/webview-features-flags)（原文：Use for website testing only） |
| 关掉 Windows Defender / SmartScreen 想借此启动 | **无效**。Tauri #9252 的复现者明确在"SmartScreen disabled + Defender disabled"下依然复现 | [tauri#9252](https://github.com/tauri-apps/tauri/issues/9252) |
| 单纯"重装 WebView2 运行时" | 只对"运行时损坏/被卸载"型有效（`FailedToReceiveMessage` + 用户卸载过 Edge），对注入型无效 | [clash-verge-rev#2227](https://github.com/clash-verge-rev/clash-verge-rev/issues/2227)（用户卸载 Edge 后出现同类错误，重装即恢复） |
| 用 Edge Stable 浏览器内核替代 WebView2 | **微软不支持**：WebView2 应用不允许依赖 Edge Stable 通道 | [Distribution（"Microsoft Edge Stable channel isn't supported for WebView2"）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution) |
| 只加代码签名就指望解决 | 签名解决的是"文件被判毒/拦截安装"，不解决"往渲染进程注入 DLL"。仍需配合信任区/豁免；但签名+信誉会显著降低被拦概率 | 卡巴误报案例见 [tauri#10302](https://github.com/tauri-apps/tauri/issues/10302)；微软要求"按签名信任 `msedgewebview2.exe`"：[security tools 文档](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools) |
| 把 `--disable-breakpad --disable-crash-reporter` 当万能药 | **有反作用风险**。微软明确要求"允许 Crashpad 子进程"，禁止后可能出现"内容永不加载"；且丢掉唯一的崩溃证据（`.dmp`） | [security tools 文档（"Keep child process creation unrestricted; permit Crashpad"）](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools) |
| 期待 `--no-sandbox` 单独解决一切 | 它能绕过"沙箱内注入导致的 FAIL_FAST"，但 DBX 现场已带 `--no-sandbox` 仍失败 → 说明本机拦截点不在渲染器沙箱，而在**更早的环节**（浏览器进程创建/环境初始化）或**注入的是浏览器进程** | DBX issue [#10459](https://github.com/t8y2/dbx/issues/10459)、[#9711](https://github.com/t8y2/dbx/issues/9711) |

---

## 三、360 / 奇安信 / 卡巴斯基：行为与用户侧可操作设置

### 3.1 已知/可确证的机制（按证据强度排序）

1. **注入 + 完整性自检 → 渲染器自杀**（证据最强，中文同构案例）：天锐绿盾（Tipray）通过 Detours 向所有 GUI 进程注入 `LdxHook64.dll` / `LdWaterMarkHook64.dll`，WebView2 渲染进程完整性自检失败 → `FAIL_FAST (0x80000003)`，应用白屏但进程仍在；**同版本 Edge 正常**（因为 Edge 在厂商白名单里）。缓解参数正是 `--no-sandbox --disable-gpu --disable-features=RendererCodeIntegrity`，治本是让 IT 在控制台豁免 `msedgewebview2.exe`。→ 来源：[技术栈《WebView2 应用突然白屏崩溃？…FAIL_FAST（天锐绿盾排查实录）》](https://jishuzhan.net/article/2093159542226604033)
2. **阻断/收紧子进程与 ACL**（微软官方口径）：安全软件阻断 renderer/GPU/network/Crashpad 子进程、修改 Runtime 或 UDF 的 ACL、去掉 LowIL、注入 DLL，都会导致"初始化失败 / 空白 / 冻结 / 内容永不加载"。→ [Prevent security tools from blocking WebView2 apps](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools)
3. **云查杀导致的启动慢**（官方承认的现象类别）："Slow startup due to deep real-time scanning of the WebView2 Runtime and the user data folder (UDF)"，并建议只做**定向排除**（Runtime 目录 + UDF），不要全局排除。→ 同上
4. **误报/拦截安装与注册表写入**（卡巴实证）：卡巴把 Tauri 应用的 `HKLM\...\CurrentVersion\Run` 写入判为 `HEUR:Trojan.Multi.Misslink.a`。→ [tauri#10302](https://github.com/tauri-apps/tauri/issues/10302)
5. **360 的"核晶防护"是内核级 HIPS**：中文资料普遍描述为"内核级主动防御引擎"，**关闭后需重启**才生效（大量模拟器厂商 FAQ 都在教用户关它，因为会拦截虚拟化/注入类行为）。→ [夜神模拟器 FAQ：关闭 360 核晶防护](https://www.yeshen.com/faqs/Bk8HEZQgb)

> **不确定项（不要当结论写进对外文档）**：
> - "360 会主动终止 `msedgewebview2.exe`" —— 没有找到公开可信来源。更可能是"注入 + 拦截进程创建 + 云查杀延迟"。用第四节的进程计数/命令行快照来区分。
> - "35s 一定是 360 云查杀" —— 只是最合理假设之一。另有官方级同构现象：**WebView2 153+ 在特定 shell 场景下 UI 线程会阻塞约 33s**（UIAutomation/`tiptsf.dll` COM 调用），且该报告机器上有 SentinelOne 注入。→ [WebView2Feedback #5730](https://github.com/MicrosoftEdge/WebView2Feedback/issues/5730)。所以 35s 也可能来自"COM/UIAutomation 等锁 + AV hook 放大"，而不是扫描。

### 3.2 360 安全卫士（用户可操作）

| 动作 | 典型路径（中文 UI，版本不同略有差异，需让用户按关键词找） | 说明 |
|---|---|---|
| 加信任（文件/目录） | 360安全卫士 → **木马查杀 → 信任区 → 添加信任**（选择 `dbx.exe` 与 `msedgewebview2.exe`，或整个安装目录） | 最有效的一步；需对 **两个 exe** 都加，只加 `dbx.exe` 不管子进程 |
| 加信任（拦截弹窗时） | 弹窗 → "**允许程序运行**" / "**添加信任**"（不要选"阻止"） | 若用户曾点过"阻止"，规则会持久化，必须在信任区里删掉那条阻止规则 |
| 关闭核晶防护 | 360安全卫士 → **安全防护中心 → 核晶防护 → 关闭**（**需重启**） | 企业机器上可能被策略强制开启；关闭会明显降低整机防护 |
| 关闭"沙箱"运行 | 右键程序 → 不在沙箱中运行 / 安全防护中心 → 沙箱设置 | 沙箱内启动的进程对 WebView2 子进程影响很大 |
| 检查"防护日志" | 安全防护中心 → **防护日志 / 拦截记录**，按时间对齐 DBX 启动时刻 | 这是证明"是不是 360 干的"的直接证据，比猜有效 |
| 退出/游戏模式验证 | 托盘右键 → 退出（临时） | 只用于**定位**，不作为长期方案 |

来源（中文，第三方教程，仅作路径参考）：[360安全卫士如何设置白名单（博客园）](https://www.cnblogs.com/Dongmy/p/18289963)、[关闭 360 核晶防护（夜神 FAQ）](https://www.yeshen.com/faqs/Bk8HEZQgb)

### 3.3 奇安信（天擎 / 终端安全）

- UI 里叫"**信任程序 / 信任区**"，支持按**文件类型（全部）+ exe/bat** 添加；企业版通常由**控制台统一下发**，本机加了也会被策略刷回。
- 用户侧动作：天擎客户端 → 信任区/白名单 → 添加 `dbx.exe`、`msedgewebview2.exe`、安装目录；若为域内终端，需**找 IT 在控制台加白**（本机设置无效是常见坑）。
- 来源：[奇安信天擎 安全软件设置信任程序（CSDN）](https://blog.csdn.net/zengliguang/article/details/136972101)（内容很薄，仅证明"添加信任程序 + 文件类型选全部"这一 UI 存在；具体菜单名请以现场为准）

### 3.4 卡巴斯基（Kaspersky Endpoint Security / Plus）

- 用户侧动作：**设置 → 安全 → 受信任的应用程序（Trusted applications）→ 添加** `dbx.exe` 与 `msedgewebview2.exe`，勾选"不扫描所有流量/不监控子进程活动"；同时在**威胁与排除 → 排除项**里加安装目录与 UDF。
- 误报要报给卡巴（Opentip）而不是只本地加白：官方给的误报提交入口见 tauri#10302 的维护者回复。
- 来源：[tauri#10302（Kaspersky 误报 + 官方误报提交链接）](https://github.com/tauri-apps/tauri/issues/10302)

### 3.5 签名 / 信誉 / MOTW / 安装路径

- **签名**：AV 的"未知文件"判定非常依赖签名与云信誉。**未签名的每个新版本都会被重新怀疑一次**（哈希变了），这是"某些版本能起、升级后不能起"的常见解释。建议 OV/EV 代码签名 + 向 360/奇安信/卡巴提交误报样本。
- **MOTW（Zone.Identifier）**：从浏览器下载的安装包带 Mark-of-the-Web，SmartScreen/AV 会更严；`Get-Item .\DBX_..._setup.exe -Stream Zone.Identifier` 可查。企业分发（GPO/SCCM/Intune）通常不带 MOTW，可作为对照实验。
- **安装路径**：`%LOCALAPPDATA%\Programs\DBX` 是**每用户、非提权、可写**，对 UDF 最友好；`Program Files` 下若非提权运行，默认 UDF（exe 同目录 `{exe}.WebView2`）可能**无写权限 → 创建直接失败**。所以"换安装路径"主要修的是权限类失败，不是注入类失败，但确实值得作为一个 profile（P10）。
- 微软官方还要求：**不要**修改 Runtime 目录的 ACL、不要动 `C:\Windows\System32` 下由系统加载的 WebView2 二进制（AV 隔离这些文件会直接打挂 WebView2）。→ [security tools 文档](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools)

---

## 四、建议记录的诊断信息清单

### 4.1 WebView2 创建与错误码

| 信号 | 取值/含义 | 采集方式 |
|---|---|---|
| `CreateCoreWebView2EnvironmentWithOptions` HRESULT | `E_FAIL 0x80004005`（普遍失败，常因 Runtime 被拦） / `ERROR_INVALID_STATE 0x8007139F`（同 UDF 上已有不同参数的浏览器进程） / `REGDB_E_CLASSNOTREG 0x80040154`（Runtime 未注册/缺失） / `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`（fixed runtime 路径含 `\Edge\Application\`） / `0x80070005`（ACL/写权限） / `CO_E_NOTINITIALIZED`、`RPC_E_CHANGED_MODE`（STA 问题） | wry 里包一层，记录 HRESULT 十六进制 + 耗时 |
| 环境创建耗时 | 与 `setup entered` 的 35s 间隔对账 | 现在日志已能算（`tauri application built` → `setup entered`） |
| `CreateCoreWebView2Controller` HRESULT | 同上；`0x8007139F` 特指参数/UDF 冲突 | 同上 |
| 渲染器退出码 | `-1073740760` = `0xC0000428` **STATUS_INVALID_IMAGE_HASH**（代码完整性拦截）；`0x80000003` **STATUS_BREAKPOINT**（FAIL_FAST，注入型） | `ProcessFailed` 事件 + Crashpad dump |
| `ProcessFailed` 事件 | kind = Renderer / GPU / Browser / Utility / FrameRenderer；`FailureSourceModulePath`（SDK ≥120）直接给出**肇事 DLL 全路径** | 已有 `webview2_recovery.rs`，把 `FailureSourceModulePath` 也记下来 |

来源：[Code Integrity in WebView2](https://github.com/MicrosoftEdge/WebView2Feedback/blob/main/diagnostics/code_integrity.md)（`-1073740760` / `STATUS_INVALID_IMAGE_HASH` 与 `FailureSourceModulePath`）、[CreateCoreWebView2EnvironmentWithOptions](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl)、[#4174（0x8007139F 实证）](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4174)

### 4.2 进程与运行时版本

```powershell
# 子进程是否真的起来了、命令行里参数是否生效（关键：能证明 env/API 参数有没有被吃进去）
Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" |
  Select-Object ProcessId, ParentProcessId, ExecutablePath, CommandLine |
  Format-List

# 运行时版本（机器级 + 用户级）；括号里的 GUID 是 Evergreen Runtime 的固定产品码
Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -Name pv -EA SilentlyContinue
Get-ItemProperty 'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -Name pv -EA SilentlyContinue

# 是否为提权进程（决定 env 覆盖是否被忽略！）
whoami /groups | findstr /i "S-1-16-12288"   # High Mandatory Level
```

- 断言：`CommandLine` 里应出现 `--no-sandbox`、`--user-data-dir=<期望的 UDF>`、`--disable-features=...`。**若没有 → 参数没生效，别怀疑 AV**（常见于提权 + 只设了 env）。
- `ExecutablePath` 可区分 Evergreen 与 Fixed Version（固定版应指向你打包的目录）。

### 4.3 事件日志 / 崩溃转储

- **代码完整性**：事件查看器 → Windows 日志 → **系统** → 筛选来源 `Microsoft-Windows-CodeIntegrity`，找 `msedgewebview2.exe` 相关条目（含被拦文件名与路径）。
- **Crashpad**：`<UDF>\EBWebView\Crashpad\reports\*.dmp`（DBX 的 UDF 在 `%LOCALAPPDATA%\com.dbx.app\...`）。白屏期间是否持续产生 `.dmp`，是"崩溃型"与"卡住型"的分水岭。来源：[绿盾案例的验证清单](https://jishuzhan.net/article/2093159542226604033)
- **应用日志**：`%LOCALAPPDATA%\com.dbx.app\startup.log`（已有）+ `webview2-recovery.log`（已有）。

### 4.4 AV 产品枚举（不需要管理员）

```powershell
Get-CimInstance -Namespace root\SecurityCenter2 -ClassName AntiVirusProduct |
  Select-Object displayName, productState, pathToSignedProductExe, timestamp
```
- `productState` 解码：`0x1000`=开、`0x10`=关（常见 `0x611` 之类组合，建议同时记录原值而不是自己解析）。
- 已知路径兜底：`C:\Program Files (x86)\360\360Safe`、`360sd`、`QiAnXin\TianQing`、`Kaspersky Lab\*\avp.exe`；驱动层：`360Box*.sys`、`klif.sys`、`QAX*.sys`（`fltmc filters` / `driverquery`）。
- 目的：日志里直接写明"本机 AV = X"，避免让用户复述；也便于把同类机器聚类。

### 4.5 判定"窗口到底怎么了"

- `visible=Err(Runtime(FailedToReceiveMessage))` 的**准确含义**：tauri-runtime-wry 的 `window_getter!` 发出消息后 `rx.recv()` 失败——即运行时**窗口表里已没有该 window id**，响应发送端被丢弃。**结论：原生窗口已被销毁**（不是隐藏、不是最小化）。因此"等它变可见"永远不会成功，正确动作是**销毁并重建窗口**（或先创建环境、再创建窗口）。
- 直接验证：`IsWindow(hwnd)`、`GetWindowThreadProcessId`、`GetWindowLong(GWL_STYLE) & WS_VISIBLE`，以及 `EnumWindows` 里是否存在该进程的顶层窗口。
- 与微软"Task Manager 判定法"对齐：宿主进程下应能看到 `msedgewebview2.exe` 及其 renderer/GPU/network/Crashpad 子进程；**一个都没有 → 创建被阻断**；**闪一下就消失 → 被终止/崩溃**。→ [security tools 文档](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools)

### 4.6 时间线字段（定位 35s）

记录：`process start` → `env create begin/end`（HRESULT+耗时）→ 首个 `msedgewebview2.exe` 出现时间 → 首个 `ProcessFailed` → `setup entered`。若 35s 落在 `env create` 内且期间**没有**任何 `msedgewebview2.exe`，更像"进程创建被拦截/挂起"；若已有 browser 进程但 renderer 迟迟不出现，更像"渲染器被注入杀死后重建"。

---

## 五、参考来源

**微软官方**
- [Prevent security tools from blocking WebView2 apps](https://learn.microsoft.com/en-us/deployedge/webview2-security-tools) — AV/EDR/DLP 导致白屏/冻结/初始化失败的全部官方条款（子进程、ACL、LowIL、注入、UDF、慢启动、System32 二进制）
- [Develop secure WebView2 apps（elevated 覆盖表）](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/security) — **提权时 `WEBVIEW2_*` 与 HKCU 策略被忽略，HKLM 策略与 API 参数生效**
- [WebView2 browser flags](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/webview-features-flags) — 官方 flag 清单（含 `no-sandbox` "testing purposes only"、`no-first-run`、`disable-gpu`、`edge-webview-no-dpi-workaround`），以及"提权应用忽略本机设备环境变量"
- [CreateCoreWebView2EnvironmentWithOptions / WebView2 IDL 参考](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl) — 全部环境变量与注册表策略、UDF 默认位置、`ERROR_INVALID_STATE`、fixed runtime 路径限制、channel 搜索顺序
- [Evergreen vs. fixed version of the WebView2 Runtime](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/evergreen-vs-fixed-version) / [Distribution](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution) — fixed runtime 的代价、Edge Stable 不被支持
- [RendererCodeIntegrityEnabled 策略](https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/renderercodeintegrityenabled) — 119 起移除并忽略

**WebView2 团队 / 社区**
- [Code Integrity in WebView2（diagnostics）](https://github.com/MicrosoftEdge/WebView2Feedback/blob/main/diagnostics/code_integrity.md) — 118/119 起无法关闭；`-1073740760`；`FailureSourceModulePath`；事件日志查法
- [WebView2Feedback #4094](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4094) — `RendererCodeIntegrity` 失效全过程、117 是最后可用版本、`--no-sandbox` 是 119 上的逃生口、**Win8 兼容模式有效**
- [WebView2Feedback #4174](https://github.com/MicrosoftEdge/WebView2Feedback/issues/4174) — 同 UDF 不同参数 → `0x8007139F`
- [WebView2Feedback #5730](https://github.com/MicrosoftEdge/WebView2Feedback/issues/5730) — 153+ UI 线程阻塞约 33s（UIAutomation/tiptsf），35s 现象的另一解释
- [WebView2Feedback #5720](https://github.com/MicrosoftEdge/WebView2Feedback/issues/5720) — runtime 会自行附加 `--in-process-gpu`；软件合成路径硬依赖 WARP/DirectComposition
- [WebView2Feedback #5053](https://github.com/MicrosoftEdge/WebView2Feedback/issues/5053) — Intune 部署后部分用户白屏、UDF 里不生成 `EBWebView` 缓存

**Chromium / Tauri / wry**
- [Chromium command line switches（peter.sh）](https://peter.sh/experiments/chromium-command-line-switches/) — `--disable-dev-shm-usage`(Linux)、`--disable-gpu-sandbox`、`--disable-breakpad`、`--disable-component-update`、`--disable-background-networking`、`--disable-site-isolation-trials` 的准确语义
- [tauri#10302 Kaspersky 误报](https://github.com/tauri-apps/tauri/issues/10302) / [tauri#2486 Windows AV 误报总贴](https://github.com/tauri-apps/tauri/issues/2486)
- [tauri#9252 WebviewWindowBuilder fails on Windows 10（`FailedToReceiveMessage`）](https://github.com/tauri-apps/tauri/issues/9252) — 关掉 Defender/SmartScreen 仍复现
- [tauri-runtime-wry 源码（`getter!` / `FailedToReceiveMessage`）](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri-runtime-wry/src/lib.rs)
- [tauri#11144 改 `additional_browser_args` 必须同时换 data directory](https://github.com/tauri-apps/tauri/issues/11144)
- [Tauri config 参考（`webviewInstallMode: fixedRuntime`、`additionalBrowserArgs`、`dataDirectory`）](https://tauri.app/reference/config/)
- [docs.rs `WebviewWindowBuilder::additional_browser_args` / `data_directory`](https://docs.rs/tauri/latest/tauri/webview/struct.WebviewWindowBuilder.html)
- [docs.rs `wry::WebViewBuilder`](https://docs.rs/wry/latest/wry/struct.WebViewBuilder.html)

**DBX 自身**
- [DBX #10459 已尝试企业环境兼容模式，但主窗口仍未显示](https://github.com/t8y2/dbx/issues/10459) — 0.6.26 现场日志（`run_event_count=38`、`main_visible=false`、compat 子进程同样失败）
- [DBX #9711 启动不了](https://github.com/t8y2/dbx/issues/9711) — 0.6.17 现场日志（标准模式即 `FailedToReceiveMessage`）
- [DBX #5051 Fix：自动恢复企业环境下的 Windows WebView2 启动失败](https://github.com/t8y2/dbx/pull/5051) / [#7491 fix(windows): 自动恢复不可见的启动主窗口](https://github.com/t8y2/dbx/pull/7491)

**中文（企业环境实证 / 用户操作）**
- [WebView2 应用突然白屏崩溃？可能是 DLP 加密软件的钩子注入导致的 FAIL_FAST（天锐绿盾排查实录）](https://jishuzhan.net/article/2093159542226604033) — 注入→`0x80000003`→白屏；Edge 正常；`--no-sandbox --disable-gpu --disable-features=RendererCodeIntegrity`；白名单治本；Crashpad 验证法
- [奇安信天擎 安全软件设置信任程序（CSDN）](https://blog.csdn.net/zengliguang/article/details/136972101)
- [360安全卫士如何设置白名单（博客园）](https://www.cnblogs.com/Dongmy/p/18289963)
- [关闭 360 核晶防护（夜神模拟器 FAQ）](https://www.yeshen.com/faqs/Bk8HEZQgb)

**无法访问 / 未能取证**
- `learn.microsoft.com/.../troubleshoot/.../prevent-antivirus-tools-from-blocking-or-crashing-teams`：本次抓取返回 404（Teams 侧 AV 排除清单未取到；其要点已被上面 security-tools 文档覆盖）。
- 知乎 / 掘金 / V2EX 的检索页在本次会话中被反爬（DDG/Brave 均返回 202/空结果），未能取得可引用的原帖 URL；相关结论改由 GitHub issue、微软文档与 jishuzhan 案例支撑。
- 360 官方帮助中心文档未能直接抓取（站点反爬），UI 路径来自第三方教程，**落地时需让用户按关键词核对**。
