# Windows 企业环境启动兼容测试说明（360 / 奇安信 / 卡巴斯基）

本文档配套 `docs/win-av-compat-research.md`（调研与证据来源），说明本次为「安全软件拦截 WebView2、DBX 主窗口无法创建」问题新增的启动兼容阶梯、诊断日志和测试安装包。

## 1. 问题回顾

现场日志（0.6.49，Windows 10 + 360）：

```
startup watchdog after 60s run_event_count=0 main_exists=false main_visible=false
startup main window was not visible; restarting once with enterprise compatibility mode
... enterprise compatibility process spawned ...
process start ... enterprise_compat=true
tauri application built after 13.1376ms
setup entered after 35.8886476s              <- WebView2 环境创建被阻塞约 35 秒
early main window show requested; main_window visible=Err(Runtime(FailedToReceiveMessage)) ...
```

要点（结论来自调研）：

- `FailedToReceiveMessage` 的含义不是"窗口被隐藏"，而是 **原生窗口已经被销毁**，所以"等它变可见"永远不会成功，必须在检测到后换一种启动方式再试。
- 旧的"企业兼容模式"只做了两件事：把 `WEBVIEW2_USER_DATA_FOLDER` 指向独立目录、在 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 里加 `--no-sandbox --disable-gpu`。但 **upstream wry 总是显式设置 WebView2 选项，这两个环境变量会被忽略**，所以旧模式实际上是空转（诺顿能启动只是因为它没拦截）。本次在 vendored wry 中修复了该问题。
- 用户实测：内置 WebView2 109 固定运行时的 Win7 离线安装包可以在这台 360 机器上启动（只是样式老）。这证明"固定版本运行时"是最有希望的路径。
- RendererCodeIntegrity 从 WebView2 118 起被强制开启、无法关闭；117 是最后一个可以关闭的大版本。因此提供 117 与 133 两个固定运行时安装包。

## 2. 本次改动

### 2.1 启动兼容阶梯（`src-tauri/src/startup_recovery.rs`）

标准启动 60 秒内主窗口不可见 → 自动按顺序启动后续 profile（每次都是新进程，通过环境变量传递 profile/历史）：

| # | profile | 做法 |
|---|---------|------|
| 1 | `isolated-no-sandbox` | 独立 WebView2 数据目录 + `--no-sandbox --disable-gpu`（原企业兼容模式） |
| 2 | `renderer-code-integrity-off` | 增加 `--disable-features=RendererCodeIntegrity`（仅 WebView2 < 118 有效） |
| 3 | `subprocess-sandbox-off` | 关闭 GPU 沙箱、首次运行、后台网络等 |
| 4 | `in-process-gpu` | `--in-process-gpu`（规避 GPU 子进程被拦截） |
| 5 | `temp-user-data` | WebView2 数据目录放到 `%TEMP%` |
| 6 | `av-scan-delay` | 先等 20 秒，让杀软首次扫描完成 |
| 7 | `warmup-preflight` | 单独子进程先创建/释放一次 WebView2 环境预热 |
| 8 | `appcompat-win8` | 给 `msedgewebview2.exe` 写 HKCU 的 AppCompat "Windows 8" 兼容层（可回滚） |
| 9 | `elevated-relaunch` | `ShellExecuteW runas` 提权重启（会弹 UAC） |
| 10 | `temp-relocated` | 复制 exe 到 `%TEMP%` 再启动（规避按安装目录拦截的规则） |
| 11 | `single-process-experimental` | `--single-process`（WebView2 不保证支持，最后尝试） |

- 每个 profile 使用 **独立的 WebView2 用户数据目录**（同一目录配不同参数会 `ERROR_INVALID_STATE 0x8007139F`）。
- 成功后弹原生框，明确告知 **第几种方式成功**，并询问是否记住；记住后下次直接使用该方式（写入 `%APPDATA%\com.dbx.app\webview2-enterprise-compat.enabled`）。
- 全部失败弹框列出尝试过的方式、常见原因（把 DBX 和 msedgewebview2.exe 加入信任区、关闭核晶防护）和日志路径。
- 可用环境变量关闭/调试：`DBX_DISABLE_ENTERPRISE_COMPAT=1`、`DBX_DISABLE_STARTUP_COMPAT=1`、`DBX_STARTUP_WATCHDOG_SECS=<秒>`、`DBX_STARTUP_COMPAT_FAST_FAIL_GRACE_MS=<毫秒>`、`DBX_WEBVIEW2_NO_SANDBOX=1`。

### 2.2 诊断（`src-tauri/src/webview2_preflight.rs`、`startup.log`）

每次启动（标准启动成功时日志会被删除，保留需 `DBX_KEEP_STARTUP_LOG=1`；进入兼容阶梯后日志会保留并追加）都会记录：

- `webview2_preflight runtime_version=... runtime_install_kind=evergreen|fixed_runtime|missing`
- `webview2_preflight security_products=360安全卫士; ...`（检测已安装的杀软，最多 8 个）
- `webview2_preflight webview2_process_count=...`（`msedgewebview2.exe` 进程数）
- `webview2_preflight exe_has_zone_identifier=... integrity_level=... windows_build=...`
- 每个 profile 的参数、启动延迟、watchdog、预热探测结果（含 HRESULT）、窗口探测失败、升级到下一个 profile 的决策、成功/失败结果。
- 另有 `webview2-recovery.log` 记录运行期 WebView2 进程失败事件（已有功能）。

### 2.3 wry 补丁（`vendor/wry/src/webview2/mod.rs`）

- 运行期让 `WEBVIEW2_USER_DATA_FOLDER`、`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 真正生效（`--disable-features` 列表做并集合并，而不是替换）。
- `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` 在任意 Windows 版本都生效（让 Win10 也能用固定运行时）。

## 3. 结论（2026-10-21 现场验证）：根因是 WebView2 运行时缺失/损坏

客户装**官网 Win10 离线包**（`DBX_0.6.49_x64-offline-setup.exe`，内嵌 WebView2 安装器）后**可正常启动**，未再出现企业兼容模式。至此确认：

- 之前反复出现的"已尝试企业环境兼容模式，但主窗口仍未显示"**不是 360 等安全软件拦截**，而是 WebView2 运行时缺失/损坏：
  - 官方 30MB 安装包用 `downloadBootstrapper` 模式，安装时需要联网下载 WebView2 引导器；在客户机器上这一步没有真正装成（离线/被拦/装成 per-user 等），或运行时随后被破坏/清理；
  - Tauri 的运行时检测只读注册表，**注册信息还在但运行时文件损坏时检测仍会通过**，随后窗口创建卡 35 秒、窗口被销毁 → 触发 DBX 的 60s watchdog → 弹出误导性的"企业兼容模式"失败提示。
- Win7 包能用，是因为它把 WebView2 109 固定运行时打包进安装目录、完全不依赖系统运行时。
- 我们的 30MB 测试包报"缺少 WebView2"，是因为它同样是 `downloadBootstrapper` 模式，而当时系统已经没有可用的运行时注册/文件。

产品结论：对中文企业环境，**默认提供/推荐离线安装包（offlineInstaller）**，并且 DBX 要在启动前把"运行时缺失/损坏"说清楚，而不是让用户以为被安全软件拦截。

## 4. 先区分两种失败：WebView2 缺失 vs 安全软件拦截

这两种现象完全不同，处理方式也不同（客户 0.6.49 的问题实际是**运行时缺失**，被误读成了 360 拦截）：

| 场景 | 判定依据（修复后日志） | 现象 | 处理 |
|---|---|---|---|
| **WebView2 运行时缺失** | `webview2 runtime_health=missing` | 旧版本弹 Tauri 通用英文框后进程退出（不会进入企业兼容模式）；修复版本弹 DBX 中文提示，可一键打开微软官方下载页 | 安装 WebView2 Runtime（官网 Evergreen Standalone Installer，或 DBX 离线包） |
| **WebView2 注册存在但文件损坏/被删**（本次客户的实际状态） | `webview2 runtime_health=registered_but_damaged` + `runtime_version=<版本>` | 旧版本：版本检测通过 → 窗口创建卡 30s+ / 窗口销毁 → 60s 后进企业兼容阶梯（误导为 AV 拦截）；修复版本弹"WebView2 已损坏，请覆盖安装修复"并给出下载页 | 重新安装/修复 WebView2（覆盖安装即可），或使用 DBX 离线包 |
| **运行时正常但被安全软件拦截** | `runtime_health=available`，但 `startup.log` 有 30s 级卡顿、窗口探测 `FailedToReceiveMessage`、阶梯部分 profile 失败 | 主窗口不显示，60s 后进入启动兼容阶梯 | 信任区加入 `dbx.exe` + `msedgewebview2.exe`；让阶梯报告哪种方式有效 |

为什么"缺失"不会被误判成企业兼容模式：Tauri 在创建窗口前会执行 `wry::webview_version()`，缺失时直接弹它自己的英文对话框并让 `Builder::build()` 失败；DBX 的启动 watchdog（触发企业兼容模式的逻辑）只在构建成功后才启动，所以根本到不了兼容阶梯。

> 修复后的版本（启动前检测）：缺失或"注册但损坏"时弹 DBX 中文指引（"是"打开官方下载页），不再跑 11 级阶梯；运行时正常但被拦截的机器照常走阶梯。可用 `DBX_SKIP_WEBVIEW2_RUNTIME_CHECK=1` 跳过该检测（逃生开关）。

**当前已发出的标准测试包（commit 4951cf2a）不含这个提示改进**，但测试逻辑不受影响：**先让客户从官网安装 WebView2 Runtime，再运行标准包即可**。

## 5. 测试安装包

CI（`.github/workflows/windows-av-compat-build.yml`）产出三个安装包（均未签名，仅用于测试）：

测试顺序：**先确认 WebView2 已安装，再测标准包；fixed 包只用于兜底**。`avcompat-offline`（内置 WebView2 Evergreen 离线安装器）的 CI job 已就绪，需要时用 `[release-build]` 触发，本次未构建。

1. **`avcompat`（先测）**：不内置运行时的常规安装包，走的正是会失败的 Evergreen 路径。它用来验证启动兼容阶梯本身：主窗口起不来时 11 种方式会依次尝试，成功弹框会报出"第 N 种方式"，日志会记录每种方式的参数/阶段/HRESULT/杀软检测——这是判断 360 拦在哪一步、哪种绕过有效的唯一途径。
2. **`avcompat-fixed117`（兜底）**：内置 WebView2 117.0.2045.60 固定运行时（安装时释放到安装目录 `webview2-fixed-runtime/`，启动时 `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` 指向它，绕开系统运行时）。与 Win7 离线包同思路，大概率能直接启动，但无法验证新的兼容阶梯；117 是最后一个还能关闭 RendererCodeIntegrity 的版本，界面样式正常。
3. **`avcompat-fixed133`（兜底，样式最新）**：内置 WebView2 133.0.3065.92 固定运行时。若问题是"渲染器被注入"，133 因强制代码完整性可能仍被拦截，此时会进入启动阶梯继续尝试。

每个包内附：

- `DBX_<version>_<label>_x64-setup.exe` 安装包
- `collect-dbx-startup-logs.ps1`：客户端日志/环境一键收集脚本
- `SHA256SUMS.txt`、`BUILD-INFO.txt`

> 固定运行时安装包体积较大（200MB+）且**不接收安全更新**，只作为企业环境逃生舱；确认可用后应推动把 DBX 与 `msedgewebview2.exe` 加入安全软件信任区，再换回常规版本。

## 6. 客户端测试步骤

1. 先卸载旧的 DBX（保留数据），安装 `avcompat` 标准包（或直接解压便携版运行）。
2. 打开 DBX（首次可能要等几分钟，窗口暂时不显示是阶梯在逐级尝试，属预期现象）：
   - 正常显示主界面 → 说明标准路径可用，直接收集 `startup.log` 发回即可。
   - 若弹出"DBX 已通过第 N 种兼容启动方式成功显示主界面" → **记录 N 和方式名称（截图）**，选择"是"以便下次直接启动，并把 `startup.log` 发回。这个 N 是本次适配最关键的信息。
   - 若弹出"已依次尝试…仍未显示" → 运行收集脚本，把生成的 zip 发回。
3. 如果标准包在当前机器上无法启动且暂时需要先用起来，再安装 `avcompat-fixed117`（内置 WebView2 117 固定运行时，与 Win7 离线包同思路，样式正常）；仍不行再试 `avcompat-fixed133`。
4. 收集日志：右键 `collect-dbx-startup-logs.ps1` →「使用 PowerShell 运行」，脚本会在桌面生成 `DBX-startup-logs-<机器名>-<时间>.zip`，包含 startup.log、webview2-recovery.log、WebView2 版本、杀软列表、进程信息、签名信息等。

## 7. 回滚 / 清理

- 安装包可直接覆盖安装或从"应用和功能"卸载；卸载不会删除 `%APPDATA%\com.dbx.app` 下的数据。
- 兼容阶梯可能写入以下内容，均可用以下方式清理：
  - WebView2 兼容模式标记：删除 `%APPDATA%\com.dbx.app\webview2-enterprise-compat.enabled`
  - AppCompat Windows 8 层：`HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers` 下删除指向 `msedgewebview2.exe` 且数据为 `~ WIN8RTM` 的项（阶梯失败时会自动清理）。
  - 兼容数据目录：`%LOCALAPPDATA%\com.dbx.app\webview2-enterprise-compat`、`%TEMP%\dbx-webview2-compat`、`%TEMP%\dbx-startup-compat`。
- 常用关闭开关：`DBX_DISABLE_STARTUP_COMPAT=1`（不再自动尝试兼容方式）。

## 8. 已知限制

- 测试安装包未签名（fork CI 无代码签名证书）。360 对未签名文件的策略可能更严格；正式版应使用签名构建。
- 固定运行时版本不再接收安全更新，属于临时方案。
- 阶梯最长可能持续数分钟（每次尝试约 30–45 秒），期间窗口不显示是预期现象。
- `single-process`、`in-process-gpu` 属于实验性参数，不保证 WebView2 支持。
