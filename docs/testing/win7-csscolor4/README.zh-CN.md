# Win7 CSSColor4 实验包

本包仍内置 WebView2 109.0.1518.78，没有更换 Chromium 内核，也没有改业务样式。
Win7 构建默认向浏览器请求 `--enable-blink-features=CSSColor4`，验证其对 OKLCH、color-mix() 和 DBX 颜色展示的影响。此开关在上游 Chromium 109 中属于实验功能。

## A/B 对比

先完全退出 DBX（包括托盘），再进行每次启动，避免新进程交给已经运行的单实例：

- 正常启动新安装的 DBX：默认请求 CSSColor4。
- `DBX_DISABLE_WIN7_CSS_COLOR4=1`：明确关闭该功能，恢复旧颜色能力以便对比。
- `DBX_KEEP_STARTUP_LOG=1`：保留启动日志，默认在 `%APPDATA%\com.dbx.app\startup.log`。日志中的 `CSSColor4 requested=true/false` 只说明参数请求，不代表能力已经生效。

例如在临时 PowerShell 窗口中（把 EXE 路径替换成实际位置）：

```powershell
$env:DBX_KEEP_STARTUP_LOG = '1'
$env:DBX_DISABLE_WIN7_CSS_COLOR4 = '1'
& 'C:\path\to\dbx.exe'
```

若要再打开实验，完全退出后移除 `DBX_DISABLE_WIN7_CSS_COLOR4`，或将其设为 `0` 再启动。
如果已有 `--disable-blink-features=CSSColor4` 参数，默认启用会尊重该显式禁用。

重点对比：亮/暗主题、背景/边框/悬停高亮、弹窗、DOM 与 Canvas 表格、滚动与输入法。
它不能补齐该内核没有实现的其他现代 CSS 能力。测试中无需关闭沙箱或 GPU。

## 构建和探针

Fork workflow 会编译真正的 `x86_64-win7-windows-msvc` 目标，保留原有固定运行时和旧加载器，并执行 PE 导入、加载器和安装内容审计。
随后在 Windows Actions runner 上，用真实程序及其内置 WebView2 109 进行开/关探针，输出 CSS.supports 结果和截图。
Actions runner 不是 Win7，仍需要用户在 Win7 实机上验证系统与显示驱动兼容性。

这是测试用安装包，不附带正式发布签名，不生成自动更新发布。
