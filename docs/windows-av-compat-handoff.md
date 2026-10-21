# Windows 安全软件启动兼容 — 交接状态（2026-10-20）

## 分支与提交

- 工作目录：`/Users/staff/dbx/.worktrees/win-av-compat`
- 分支：`codex/windows-av-compat-startup`（已推送到 `zipg/dbx`）
- **测试安装包构建所用的提交：`4951cf2a9`**（提交信息含 `[release-build]`）
- 之后如有后续小提交（诊断超时保护、非 Windows no-op 静默），不影响已构建的安装包。

## CI 运行（正在进行，构建完成后下载）

- Release run：`39385806824`（https://github.com/zipg/dbx/actions/runs/39385806824）
  - `compile-check`：**已通过**（cargo check 6m41s，msvc target；等单测步骤）
  - `windows-installer` → 产物 `DBX-windows-x64-avcompat`（标准安装包 + 便携 zip）
  - `windows-installer-fixed-runtime` matrix →
    - `DBX-windows-x64-avcompat-fixed133`（内置 WebView2 133.0.3065.92）
    - `DBX-windows-x64-avcompat-fixed117`（内置 WebView2 117.0.2045.60，推荐先测）
- 旧的基线 run `39382631026`（df6eb59，无本改动）也在跑，可忽略/仅作对照。

构建完成后下载：

```bash
gh run download 39385806824 -R zipg/dbx -n DBX-windows-x64-avcompat-fixed117 -D /Users/staff/dbx/output/av-compat/fixed117
gh run download 39385806824 -R zipg/dbx -n DBX-windows-x64-avcompat-fixed133 -D /Users/staff/dbx/output/av-compat/fixed133
gh run download 39385806824 -R zipg/dbx -n DBX-windows-x64-avcompat -D /Users/staff/dbx/output/av-compat/standard
```

每个 artifact 内：`DBX_0.6.49_<label>_x64-setup.exe`、`collect-dbx-startup-logs.ps1`、`SHA256SUMS.txt`、`BUILD-INFO.txt`。

## 本地已验证

- `cargo check -p dbx --target x86_64-pc-windows-msvc`（CI）：通过（提交 4951cf2a）。
- `cargo check -p dbx --target x86_64-pc-windows-gnu`（本机，覆盖 `cfg(windows)` 代码）：
  - 主流程通过、0 error；
  - `--lib --tests` 测试目标也通过（单元测试可在 Windows 上运行）。
- `cargo check -p dbx --target aarch64-apple-darwin`（本机）：通过（非 Windows 分支/no-op 路径可用）。
- 三个安装包均为 **未签名** 测试构建。

## 待做

1. CI 三个 installer job 完成后下载、校验 SHA256、解包确认：
   - `dbx.exe` 内含新逻辑（可 `strings` 搜索 `compat profile` / `webview2_preflight`）；
   - fixed 包安装目录含 `webview2-fixed-runtime\msedgewebview2.exe`。
2. 交给客户端按 `docs/windows-av-compat-testing.md` 测试（**先标准包**验证启动阶梯 → fixed117 → fixed133 兜底），回收 `startup.log` 与收集脚本 zip。
3. 若 fixed117 可用：推动客户端把 DBX 与 `msedgewebview2.exe` 加入 360/奇安信信任区、关闭核晶防护，再回退官方签名版本。
4. 后续可考虑：把诊断超时保护（本次后续提交）纳入正式构建；正式版需要代码签名。

## 关键文件

- `src-tauri/src/startup_recovery.rs`：启动兼容阶梯、成功/失败弹框、标记与日志。
- `src-tauri/src/webview2_preflight.rs`：WebView2/杀软环境诊断、环境创建探测、预热子进程。
- `src-tauri/src/windows_appcompat.rs`：AppCompat Windows 8 兼容层（可回滚）。
- `vendor/wry/src/webview2/mod.rs`：让 `WEBVIEW2_*` 运行期覆盖真正生效。
- `docs/win-av-compat-research.md`：调研与证据（固定运行时、RendererCodeIntegrity、提权忽略环境变量等）。
- `docs/windows-av-compat-testing.md`：客户端测试/回滚说明。
- `.github/workflows/windows-av-compat-build.yml`：compile-check + 标准安装包 + fixed133/117 安装包。

## 2026-10-21 结案：根因确认为 WebView2 运行时缺失/损坏

客户安装官网 Win10 离线包（`DBX_0.6.49_x64-offline-setup.exe`，内嵌 WebView2）后正常启动，没有再出现企业兼容模式。结论与后续：

- **不是安全软件拦截**。之前的企业兼容模式失败是"WebView2 注册信息存在但运行时文件损坏/缺失"导致（Tauri 只读注册表做检测，检测通过后环境创建失败）；官方 30MB 包的 downloadBootstrapper 安装方式在客户机器上没能真正装好运行时；Win7 包因为内置固定运行时所以能用。
- 代码侧新增三态检测（`webview2_preflight::runtime_health()`）：
  - `missing` → 中文提示 + 一键打开官方下载页，不再进兼容阶梯；
  - `registered_but_damaged` → 中文提示"WebView2 已损坏，请覆盖安装/用离线包修复"，不再进兼容阶梯；
  - `available` → 正常启动；若随后环境创建失败（真正的 AV 拦截）才走 11 级兼容阶梯。
  - 逃生开关：`DBX_SKIP_WEBVIEW2_RUNTIME_CHECK=1`。
- 对客户/企业用户的推荐：优先发**离线安装包**（offlineInstaller）；普通安装包依赖联网下载 WebView2，在离线/受限网络环境容易装不上。
- 本次未重新构建安装包；后续如要发版，建议构建 `avcompat-offline`（offline 模式 + 新检测），CI job 已就绪（push 带 `[release-build]` 即可）。

## 2026-10-21 文案/求助体验优化（未打包）

- 三个启动失败弹框统一为「一句话结论 + 错误码 + 编号步骤 + 求助区」，错误码：001 缺运行时 / 002 运行时损坏 / 003 安全软件拦截（阶梯耗尽）。
- 失败时自动在桌面写 `DBX-启动诊断-<时间>.txt`（含 runtime_health、WebView2/杀软环境、日志路径、求助渠道），用户直接发送该文件即可；失败弹框"是"可打开日志所在文件夹。
- 成功弹框简化为「已用第 N/11 种安全软件兼容启动方式启动」，只问是否记住方式。
- 新增 `DBX_TRY_COMPAT_ON_BROKEN_WEBVIEW2=1`：运行时缺失/损坏时也可强行跑兼容阶梯（排障用）。
- 说明：AV 兼容阶梯（11 个 profile）一直保留在分支中，最近的运行时检测提交只是"运行时不可用时默认不跑阶梯"，并没有删除阶梯。

## 2026-10-21 文案二轮修改（按产品反馈，未打包）

- 001/002：不点名任何杀软；不再引导微软官网，改为引导到 DBX 官网下载 **Windows 10 离线版**（离线版自带 WebView2）；去掉"信任区"操作指示。
- 003：不点名杀软、不列尝试过的方式（都记录在启动日志里），只给"可暂时退出安全软件重试"和日志路径。
- **取消桌面诊断报告文件**：只保留唯一诊断文件 `startup.log`，弹框直接给完整路径；001/002 的"是"打开 DBX 官网下载页，003 的"是"打开启动日志所在文件夹。
- **成功弹框取消**：兼容方式成功后不再打扰用户，自动记住命中的方式（marker），失败时自动继续尝试后续方式；成功信息只写日志。
