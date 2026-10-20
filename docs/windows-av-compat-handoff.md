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
2. 交给客户端按 `docs/windows-av-compat-testing.md` 测试（先 fixed117 → fixed133 → 标准包），回收 `startup.log` 与收集脚本 zip。
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
