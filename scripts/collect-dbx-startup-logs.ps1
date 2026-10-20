#requires -version 5.1
<#
.SYNOPSIS
    DBX Windows 启动故障日志收集脚本（只读、无需管理员权限）。

.DESCRIPTION
    在出现 DBX 无法启动（例如 WebView2 被 360 安全卫士等安全软件拦截）的
    Windows 电脑上运行本脚本。脚本会收集：

      - DBX 启动日志：startup.log、webview2-recovery.log、webview2-preflight-*.log
        （默认位于 %APPDATA%\com.dbx.app，可通过 DBX_STARTUP_LOG_DIR 指定）
      - 兼容模式标记 webview2-enterprise-compat.enabled 及兼容模式数据目录清单
      - 系统信息：Windows 版本/内部版本号、区域、提权状态、相关环境变量、磁盘空间
      - WebView2 运行时版本（注册表 EdgeUpdate Clients + EdgeWebView 安装目录）
      - 安全软件：SecurityCenter2 注册的杀毒产品，以及常见安全软件进程/服务/目录探测
      - 进程：msedgewebview2.exe 列表（PID/父进程/命令行）、dbx.exe 进程
      - DBX 安装版本、可执行文件位置、SHA256 与数字签名状态
      - 最近 3 天应用程序事件日志中与 DBX/WebView2 相关的错误（尽力而为）

    所有结果打包为一个 zip，默认保存到当前用户桌面：
        DBX-startup-logs-<计算机名>-<yyyyMMdd-HHmmss>.zip

    本脚本只做读取和复制，不修改系统设置、不删除任何文件、不需要管理员权限。
    收集内容可能包含用户名、计算机名、本地路径、数据库连接名等隐私信息，
    发送前请自行检查。

.PARAMETER OutputDir
    zip 输出目录。默认为当前用户桌面。

.PARAMETER MaxLogBytes
    单个日志文件最大复制字节数，超过时只保留文件末尾部分（默认 5MB）。

.PARAMETER NoPause
    结束时不再等待回车（便于在自动化场景中调用）。

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File .\collect-dbx-startup-logs.ps1

.EXAMPLE
    .\collect-dbx-startup-logs.ps1 -OutputDir D:\dbx-logs

.NOTES
    文件名：collect-dbx-startup-logs.ps1
    随 DBX Windows 测试包一起分发，独立运行，不依赖仓库中的其它文件。
#>
[CmdletBinding()]
param(
    [string]$OutputDir = "",
    [int]$MaxLogBytes = 5 * 1024 * 1024,
    [switch]$NoPause
)

$ErrorActionPreference = "Continue"
$ProgressPreference = "SilentlyContinue"

$script:ScriptVersion = "1.0.0"
$script:AppDataDirName = "com.dbx.app"
$script:WebView2ClientGuid = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
$script:Warnings = New-Object System.Collections.Generic.List[string]
$script:CollectedLogs = New-Object System.Collections.Generic.List[object]
$script:ZipPath = $null

# ---------------------------------------------------------------------------
# 控制台输出与错误处理
# ---------------------------------------------------------------------------

function Write-Step {
    param([string]$Message)
    Write-Host ""
    Write-Host ("==> " + $Message) -ForegroundColor Cyan
}

function Write-Ok {
    param([string]$Message)
    Write-Host ("    [完成] " + $Message) -ForegroundColor Green
}

function Write-Info {
    param([string]$Message)
    Write-Host ("    " + $Message) -ForegroundColor Gray
}

function Write-WarnMsg {
    param([string]$Message)
    Write-Host ("    [警告] " + $Message) -ForegroundColor Yellow
    [void]$script:Warnings.Add($Message)
}

# 执行一个收集步骤；任何异常都只记录中文警告，不中断整体收集。
function Invoke-Capture {
    param(
        [string]$Name,
        [scriptblock]$Action
    )
    try {
        & $Action
    } catch {
        Write-WarnMsg (("{0} 收集失败：{1}" -f $Name, $_.Exception.Message))
    }
}

# ---------------------------------------------------------------------------
# 通用工具
# ---------------------------------------------------------------------------

function Ensure-ParentDir {
    param([string]$Path)
    $parent = Split-Path -Parent $Path
    if ($parent -and -not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
}

function Save-Report {
    param(
        [System.Text.StringBuilder]$Builder,
        [string]$Path
    )
    Ensure-ParentDir $Path
    $utf8Bom = New-Object System.Text.UTF8Encoding($true)
    [System.IO.File]::WriteAllText($Path, $Builder.ToString(), $utf8Bom)
}

function Add-Line {
    param(
        [System.Text.StringBuilder]$Builder,
        [string]$Text = ""
    )
    [void]$Builder.AppendLine($Text)
}

function Add-Header {
    param(
        [System.Text.StringBuilder]$Builder,
        [string]$Title
    )
    [void]$Builder.AppendLine("")
    [void]$Builder.AppendLine("========== " + $Title + " ==========")
}

function Add-KeyValue {
    param(
        [System.Text.StringBuilder]$Builder,
        [string]$Key,
        $Value
    )
    if ($null -eq $Value -or ([string]$Value).Length -eq 0) {
        Add-Line $Builder ("{0} = <未找到>" -f $Key)
    } else {
        Add-Line $Builder ("{0} = {1}" -f $Key, [string]$Value)
    }
}

function Get-RegistryValue {
    param(
        [string]$KeyPath,
        [string]$ValueName
    )
    try {
        if (-not (Test-Path -LiteralPath $KeyPath)) { return $null }
        $property = Get-ItemProperty -LiteralPath $KeyPath -Name $ValueName -ErrorAction Stop
        return $property.$ValueName
    } catch {
        return $null
    }
}

function Format-RegistryProbe {
    param(
        [string]$KeyPath,
        [string]$ValueName
    )
    $value = Get-RegistryValue -KeyPath $KeyPath -ValueName $ValueName
    if ($null -eq $value) {
        return ("  {0}\{1} -> <未找到/未设置>" -f $KeyPath, $ValueName)
    }
    return ("  {0}\{1} = {2}" -f $KeyPath, $ValueName, $value)
}

function Get-EnvironmentVariableSafe {
    param([string]$Name)
    try {
        return [Environment]::GetEnvironmentVariable($Name)
    } catch {
        return $null
    }
}

function Get-HostNameSafe {
    try {
        $name = [Environment]::MachineName
        if ([string]::IsNullOrWhiteSpace($name)) { $name = "unknown-host" }
        return ($name -replace '[\\/:*?"<>|\s]', "_")
    } catch {
        return "unknown-host"
    }
}

function Get-ProgramFilesX86 {
    try {
        $value = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
        if ([string]::IsNullOrWhiteSpace($value)) { return $null }
        return $value
    } catch {
        return $null
    }
}

# Join-Path 的安全版本：任一参数为空时返回空字符串，避免在缺少环境变量时抛出异常。
function Join-PathSafe {
    param(
        [string]$Base,
        [string]$Child
    )
    if ([string]::IsNullOrWhiteSpace($Base) -or [string]::IsNullOrWhiteSpace($Child)) { return "" }
    try {
        return (Join-Path $Base $Child)
    } catch {
        return ""
    }
}

# 计算目录的递归文件数与总大小（用于兼容模式数据目录，避免复制整个浏览器缓存）。
function Get-DirectorySummary {
    param([string]$Path)
    try {
        $files = @(Get-ChildItem -LiteralPath $Path -Recurse -File -Force -ErrorAction SilentlyContinue)
        $total = 0
        foreach ($file in $files) { $total += $file.Length }
        return ("{0} 个文件，共 {1:N2} MB" -f $files.Count, ($total / 1MB))
    } catch {
        return ("无法统计：" + $_.Exception.Message)
    }
}

# ---------------------------------------------------------------------------
# DBX 日志目录定位
# ---------------------------------------------------------------------------

function Get-DbxLogDirs {
    $result = New-Object System.Collections.Generic.List[object]
    $seen = @{}
    $candidates = New-Object System.Collections.Generic.List[object]

    $customDir = Get-EnvironmentVariableSafe "DBX_STARTUP_LOG_DIR"
    if (-not [string]::IsNullOrWhiteSpace($customDir)) {
        [void]$candidates.Add(@{ Path = $customDir; Label = "custom-dbx-startup-log-dir" })
    }
    if ($env:APPDATA) {
        [void]$candidates.Add(@{ Path = (Join-Path $env:APPDATA $script:AppDataDirName); Label = "appdata" })
    }
    if ($env:LOCALAPPDATA) {
        [void]$candidates.Add(@{ Path = (Join-Path $env:LOCALAPPDATA $script:AppDataDirName); Label = "localappdata" })
    }

    foreach ($candidate in $candidates) {
        try {
            $full = [System.IO.Path]::GetFullPath([string]$candidate.Path)
            if (Test-Path -LiteralPath $full) {
                $key = $full.ToLowerInvariant()
                if (-not $seen.ContainsKey($key)) {
                    $seen[$key] = $true
                    [void]$result.Add(@{ Path = $full; Label = $candidate.Label })
                }
            }
        } catch {
            # 单个候选目录异常不影响其它目录
        }
    }
    return $result
}

# 复制单个日志文件；超大文件只保留末尾 MaxLogBytes 字节。
function Copy-LogFileSafe {
    param(
        [System.IO.FileInfo]$Source,
        [string]$Destination
    )
    Ensure-ParentDir $Destination
    if ($Source.Length -le $script:MaxLogBytes) {
        Copy-Item -LiteralPath $Source.FullName -Destination $Destination -Force -ErrorAction Stop
        return "完整复制"
    }
    $bufferSize = $script:MaxLogBytes
    $stream = [System.IO.File]::Open($Source.FullName, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try {
        [void]$stream.Seek(-$bufferSize, [System.IO.SeekOrigin]::End)
        $buffer = New-Object byte[] $bufferSize
        $read = $stream.Read($buffer, 0, $bufferSize)
        $text = ("（文件超过 {0:N1} MB，仅保留末尾 {1:N1} MB）`r`n" -f ($Source.Length / 1MB), ($bufferSize / 1MB)) +
                [System.Text.Encoding]::UTF8.GetString($buffer, 0, $read)
        [System.IO.File]::WriteAllText($Destination, $text, (New-Object System.Text.UTF8Encoding($true)))
        return "截断至末尾部分"
    } finally {
        $stream.Dispose()
    }
}

function Collect-LogFiles {
    param([string]$Staging)

    Write-Step "复制 DBX 启动日志"
    $logsRoot = Join-Path $Staging "logs"
    New-Item -ItemType Directory -Force -Path $logsRoot | Out-Null

    $index = New-Object System.Text.StringBuilder
    Add-Line $index "DBX 启动日志收集清单"
    Add-Line $index ("收集时间：" + (Get-Date -Format "yyyy-MM-dd HH:mm:ss zzz"))
    Add-Line $index ""

    $dirs = @(Get-DbxLogDirs)
    if ($dirs.Count -eq 0) {
        Add-Line $index "未找到任何 DBX 日志目录（APPDATA / LOCALAPPDATA / DBX_STARTUP_LOG_DIR 均不可用）。"
        Write-WarnMsg "未找到 DBX 日志目录，请确认 DBX 是否已启动过。"
    }

    foreach ($dir in $dirs) {
        $label = [string]$dir.Label
        $destDir = Join-Path $logsRoot $label
        New-Item -ItemType Directory -Force -Path $destDir | Out-Null
        Add-Line $index ("目录[{0}]：{1}" -f $label, $dir.Path)

        $files = @()
        try {
            $files = @(Get-ChildItem -LiteralPath $dir.Path -File -Force -ErrorAction Stop |
                Where-Object {
                    $_.Name -like "*.log" -or
                    $_.Name -like "webview2-*" -or
                    $_.Name -like "startup*"
                })
        } catch {
            Write-WarnMsg ("读取日志目录失败：{0}（{1}）" -f $dir.Path, $_.Exception.Message)
        }

        if ($files.Count -eq 0) {
            Add-Line $index "  <未找到日志文件>"
            continue
        }

        foreach ($file in $files) {
            $destination = Join-Path $destDir $file.Name
            try {
                $mode = Copy-LogFileSafe -Source $file -Destination $destination
                $copied = Get-Item -LiteralPath $destination
                $sha = "n/a"
                try {
                    $sha = (Get-FileHash -LiteralPath $destination -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
                } catch {
                    $sha = "<计算失败>"
                }
                [void]$script:CollectedLogs.Add([pscustomobject]@{
                    Source = $file.FullName
                    Destination = $destination
                    Bytes = $copied.Length
                    Sha256 = $sha
                    Mode = $mode
                })
                Add-Line $index ("  已收集：{0}（原始 {1:N1} KB，{2}）" -f $file.Name, ($file.Length / 1KB), $mode)
                Add-Line $index ("    SHA256：{0}" -f $sha)
                Add-Line $index ("    最后修改：{0}" -f $file.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss"))
            } catch {
                Write-WarnMsg ("复制日志失败：{0}（{1}）" -f $file.FullName, $_.Exception.Message)
                Add-Line $index ("  复制失败：{0}（{1}）" -f $file.Name, $_.Exception.Message)
            }
        }
        Add-Line $index ""
    }

    Save-Report $index (Join-Path $logsRoot "log-file-list.txt")
    if ($script:CollectedLogs.Count -gt 0) {
        Write-Ok ("已复制 {0} 个日志文件" -f $script:CollectedLogs.Count)
    } else {
        Write-WarnMsg "没有复制到任何日志文件，请确认复现步骤是否执行过。"
    }
}

# 兼容模式标记文件、兼容模式数据目录、com.dbx.app 目录清单
function Collect-CompatibilityArtifacts {
    param([string]$Staging)

    Write-Step "收集 WebView2 兼容模式标记与 DBX 数据目录清单"
    $logsRoot = Join-Path $Staging "logs"
    $report = New-Object System.Text.StringBuilder
    Add-Line $report "WebView2 兼容模式标记与 DBX 数据目录"
    Add-Line $report ""

    $roots = New-Object System.Collections.Generic.List[object]
    if ($env:APPDATA) {
        [void]$roots.Add(@{ Path = (Join-Path $env:APPDATA $script:AppDataDirName); Label = "appdata" })
    }
    if ($env:LOCALAPPDATA) {
        [void]$roots.Add(@{ Path = (Join-Path $env:LOCALAPPDATA $script:AppDataDirName); Label = "localappdata" })
    }
    $customDir = Get-EnvironmentVariableSafe "DBX_STARTUP_LOG_DIR"
    if (-not [string]::IsNullOrWhiteSpace($customDir)) {
        [void]$roots.Add(@{ Path = $customDir; Label = "custom" })
    }

    $markerCopied = 0
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root.Path)) {
            Add-Line $report ("[{0}] {1} -> 目录不存在" -f $root.Label, $root.Path)
            continue
        }
        Add-Line $report ("[{0}] {1}" -f $root.Label, $root.Path)

        $entries = @()
        try {
            $entries = @(Get-ChildItem -LiteralPath $root.Path -Force -ErrorAction Stop)
        } catch {
            Add-Line $report ("  <目录读取失败：{0}>" -f $_.Exception.Message)
            continue
        }

        $compatEntries = @($entries | Where-Object { $_.Name -like "webview2-enterprise-compat*" })
        if ($compatEntries.Count -eq 0) {
            Add-Line $report "  <未发现 webview2-enterprise-compat* 标记/目录>"
        }

        foreach ($entry in $compatEntries) {
            if ($entry.PSIsContainer) {
                Add-Line $report ("  兼容模式目录：{0}（{1}）" -f $entry.FullName, (Get-DirectorySummary $entry.FullName))
                Add-Line $report "    注意：该目录是 WebView2 的独立用户数据目录，仅记录清单，未复制其内容（避免收集浏览器缓存）。"
                try {
                    $children = @(Get-ChildItem -LiteralPath $entry.FullName -Force -ErrorAction Stop | Select-Object -First 100)
                    foreach ($child in $children) {
                        $kind = "文件"
                        if ($child.PSIsContainer) { $kind = "目录" }
                        Add-Line $report ("      {0}  {1}  最后修改 {2}" -f $kind, $child.Name, $child.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss"))
                    }
                } catch {
                    Add-Line $report ("      目录内容读取失败：{0}" -f $_.Exception.Message)
                }
            } else {
                $destDir = Join-Path $logsRoot ("compatibility-" + $root.Label)
                New-Item -ItemType Directory -Force -Path $destDir | Out-Null
                $destination = Join-Path $destDir $entry.Name
                try {
                    Copy-Item -LiteralPath $entry.FullName -Destination $destination -Force -ErrorAction Stop
                    $markerCopied++
                    Add-Line $report ("  兼容模式标记文件：{0}（已复制，大小 {1} 字节）" -f $entry.FullName, $entry.Length)
                    $content = ""
                    try { $content = [System.IO.File]::ReadAllText($entry.FullName) } catch { $content = "<读取失败>" }
                    Add-Line $report ("    文件内容：{0}" -f $content.Trim())
                } catch {
                    Write-WarnMsg ("复制兼容模式标记失败：{0}（{1}）" -f $entry.FullName, $_.Exception.Message)
                }
            }
        }
        Add-Line $report ""
    }

    # 目录清单（只列顶层，避免遍历巨大的浏览器缓存）
    Add-Header $report "DBX 数据目录顶层清单"
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root.Path)) { continue }
        Add-Line $report ("[{0}] {1}" -f $root.Label, $root.Path)
        try {
            $entries = @(Get-ChildItem -LiteralPath $root.Path -Force -ErrorAction Stop | Sort-Object Name)
            if ($entries.Count -eq 0) { Add-Line $report "  <空目录>" }
            foreach ($entry in $entries) {
                if ($entry.PSIsContainer) {
                    Add-Line $report ("  目录  {0}  最后修改 {1}" -f $entry.Name, $entry.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss"))
                } else {
                    Add-Line $report ("  文件  {0}  {1:N1} KB  最后修改 {2}" -f $entry.Name, ($entry.Length / 1KB), $entry.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss"))
                }
            }
        } catch {
            Add-Line $report ("  <目录读取失败：{0}>" -f $_.Exception.Message)
        }
        Add-Line $report ""
    }

    Save-Report $report (Join-Path $logsRoot "compatibility-marker-and-data-dirs.txt")
    if ($markerCopied -gt 0) {
        Write-Ok ("已复制 {0} 个兼容模式标记文件" -f $markerCopied)
    } else {
        Write-Info "未发现兼容模式标记文件（正常模式下启动时可能不存在）。"
    }
}

# ---------------------------------------------------------------------------
# 系统信息
# ---------------------------------------------------------------------------

function Collect-SystemInfo {
    param([string]$Staging)

    Write-Step "收集系统信息"
    $b = New-Object System.Text.StringBuilder

    Add-Header $b "基本信息"
    Add-KeyValue $b "收集时间" (Get-Date -Format "yyyy-MM-dd HH:mm:ss zzz")
    Add-KeyValue $b "脚本版本" $script:ScriptVersion
    Add-KeyValue $b "计算机名" ([Environment]::MachineName)
    Add-KeyValue $b "当前用户" ([Environment]::UserName)
    Add-KeyValue $b "用户域" ([Environment]::UserDomainName)
    Add-KeyValue $b "PowerShell 版本" ($PSVersionTable.PSVersion.ToString())
    Add-KeyValue $b "PS 进程是否 64 位" ([Environment]::Is64BitProcess)
    Add-KeyValue $b "操作系统是否 64 位" ([Environment]::Is64BitOperatingSystem)
    Add-KeyValue $b "系统目录" $env:SystemRoot
    Add-KeyValue $b "命令" ([Environment]::CommandLine)

    Add-Header $b "Windows 版本"
    Add-KeyValue $b "[Environment]::OSVersion" ([Environment]::OSVersion.VersionString)
    $versionKey = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion"
    $productName = Get-RegistryValue -KeyPath $versionKey -ValueName "ProductName"
    $displayVersion = Get-RegistryValue -KeyPath $versionKey -ValueName "DisplayVersion"
    $editionId = Get-RegistryValue -KeyPath $versionKey -ValueName "EditionID"
    $currentBuild = Get-RegistryValue -KeyPath $versionKey -ValueName "CurrentBuild"
    $currentBuildNumber = Get-RegistryValue -KeyPath $versionKey -ValueName "CurrentBuildNumber"
    $ubr = Get-RegistryValue -KeyPath $versionKey -ValueName "UBR"
    $buildLabEx = Get-RegistryValue -KeyPath $versionKey -ValueName "BuildLabEx"
    $installDate = Get-RegistryValue -KeyPath $versionKey -ValueName "InstallDate"
    $osArch = Get-RegistryValue -KeyPath $versionKey -ValueName "OSArchitecture"
    $releaseId = Get-RegistryValue -KeyPath $versionKey -ValueName "ReleaseId"

    Add-KeyValue $b "注册表 ProductName" $productName
    Add-KeyValue $b "DisplayVersion" $displayVersion
    Add-KeyValue $b "EditionID" $editionId
    Add-KeyValue $b "ReleaseId" $releaseId
    Add-KeyValue $b "CurrentBuild" $currentBuild
    Add-KeyValue $b "CurrentBuildNumber" $currentBuildNumber
    Add-KeyValue $b "UBR（更新版本号）" $ubr
    Add-KeyValue $b "BuildLabEx" $buildLabEx
    Add-KeyValue $b "OSArchitecture（注册表）" $osArch

    $buildNumber = 0
    $buildText = $null
    if ($null -ne $currentBuildNumber) { $buildText = [string]$currentBuildNumber }
    elseif ($null -ne $currentBuild) { $buildText = [string]$currentBuild }
    if ($buildText -and [int]::TryParse($buildText, [ref]$buildNumber)) {
        $versionLabel = "Windows 10 或更早版本（内部版本 {0}）" -f $buildNumber
        if ($buildNumber -ge 22000) { $versionLabel = "Windows 11（内部版本 {0}）" -f $buildNumber }
        Add-Line $b ("系统版本判断：" + $versionLabel)
        if ($ubr) { Add-Line $b ("完整内部版本：" + $buildNumber + "." + $ubr) }
    }
    if ($installDate -and $installDate -is [int]) {
        try {
            Add-KeyValue $b "系统安装时间" ([DateTimeOffset]::FromUnixTimeSeconds([int64]$installDate).LocalDateTime.ToString("yyyy-MM-dd HH:mm:ss"))
        } catch { }
    }

    Add-Header $b "区域与语言"
    try { Add-KeyValue $b "当前区域" ([System.Globalization.CultureInfo]::CurrentCulture.Name) } catch { }
    try { Add-KeyValue $b "当前 UI 语言" ([System.Globalization.CultureInfo]::CurrentUICulture.Name) } catch { }
    try {
        if (Get-Command Get-WinSystemLocale -ErrorAction SilentlyContinue) {
            Add-KeyValue $b "系统区域（Get-WinSystemLocale）" ((Get-WinSystemLocale).Name)
        }
    } catch { }
    Add-KeyValue $b "系统时区" ([TimeZoneInfo]::Local.DisplayName)

    Add-Header $b "提权状态（UAC）"
    $isAdmin = $null
    try {
        $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = New-Object System.Security.Principal.WindowsPrincipal($identity)
        $isAdmin = $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
        Add-KeyValue $b "是否以管理员身份运行" ([string]$isAdmin)
        Add-KeyValue $b "令牌用户" ($identity.Name)
    } catch {
        Write-WarnMsg ("读取提权状态失败：" + $_.Exception.Message)
    }
    $integrity = "未知"
    try {
        $groups = @(& whoami.exe /groups 2>$null)
        if ($groups -match "S-1-16-16384") { $integrity = "System 完整性" }
        elseif ($groups -match "S-1-16-12288") { $integrity = "高完整性（已提权）" }
        elseif ($groups -match "S-1-16-8192") { $integrity = "中完整性（未提权，普通用户）" }
        elseif ($groups -match "S-1-16-4096") { $integrity = "低完整性（受限/沙箱）" }
    } catch { }
    Add-KeyValue $b "进程完整性级别（启发式）" $integrity
    Add-KeyValue $b "UAC EnableLUA（注册表）" (Get-RegistryValue -KeyPath "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" -ValueName "EnableLUA")
    Add-KeyValue $b "UAC ConsentPromptBehaviorAdmin（注册表）" (Get-RegistryValue -KeyPath "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" -ValueName "ConsentPromptBehaviorAdmin")
    Add-KeyValue $b "是否交互式会话" ([Environment]::UserInteractive)

    Add-Header $b "DBX / WebView2 相关环境变量"
    $envNames = @(
        "DBX_STARTUP_LOG_DIR",
        "DBX_KEEP_STARTUP_LOG",
        "DBX_WEBVIEW2_NO_SANDBOX",
        "DBX_STARTUP_COMPAT_RECOVERY",
        "DBX_STARTUP_COMPAT_PARENT_PID",
        "DBX_DISABLE_ENTERPRISE_COMPAT",
        "WEBVIEW2_USER_DATA_FOLDER",
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
        "WEBVIEW2_RELEASE_CHANNEL_PREFERENCE"
    )
    foreach ($name in $envNames) {
        $value = Get-EnvironmentVariableSafe $name
        if ($null -eq $value) {
            Add-Line $b ("{0} = <未设置>" -f $name)
        } else {
            Add-Line $b ("{0} = {1}" -f $name, $value)
        }
    }

    Add-Header $b "磁盘空间"
    try {
        $systemDrive = (Split-Path -Qualifier $env:SystemRoot) + "\"
        $drive = New-Object System.IO.DriveInfo($systemDrive)
        Add-Line $b ("{0} 可用 {1:N2} GB / 总计 {2:N2} GB" -f $drive.Name, ($drive.AvailableFreeSpace / 1GB), ($drive.TotalSize / 1GB))
    } catch {
        Write-WarnMsg ("读取磁盘空间失败：" + $_.Exception.Message)
    }

    Add-Header $b "硬件/虚拟化信息（WMI）"
    try {
        $computerSystem = Get-CimInstance -ClassName Win32_ComputerSystem -ErrorAction Stop
        Add-KeyValue $b "制造商" $computerSystem.Manufacturer
        Add-KeyValue $b "型号" $computerSystem.Model
        Add-KeyValue $b "域/工作组" $computerSystem.Domain
    } catch {
        Add-Line $b "<Win32_ComputerSystem 查询失败：$($_.Exception.Message)>"
    }
    try {
        $operatingSystem = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop
        Add-KeyValue $b "Caption" $operatingSystem.Caption
        Add-KeyValue $b "Version" $operatingSystem.Version
        Add-KeyValue $b "BuildNumber" $operatingSystem.BuildNumber
        Add-KeyValue $b "OSArchitecture" $operatingSystem.OSArchitecture
        Add-KeyValue $b "LastBootUpTime" $operatingSystem.LastBootUpTime
    } catch {
        Add-Line $b "<Win32_OperatingSystem 查询失败：$($_.Exception.Message)>"
    }
    try {
        Add-KeyValue $b "处理器架构（PROCESSOR_ARCHITECTURE）" $env:PROCESSOR_ARCHITECTURE
        Add-KeyValue $b "处理器标识" $env:PROCESSOR_IDENTIFIER
        Add-KeyValue $b "处理器数量" $env:NUMBER_OF_PROCESSORS
    } catch { }

    Save-Report $b (Join-Path $Staging "01-system-info.txt")
    Write-Ok "系统信息已写入 01-system-info.txt"
}

# ---------------------------------------------------------------------------
# WebView2 运行时
# ---------------------------------------------------------------------------

function Collect-WebView2Runtime {
    param([string]$Staging)

    Write-Step "收集 WebView2 运行时版本"
    $b = New-Object System.Text.StringBuilder
    Add-Header $b "EdgeUpdate Clients 注册表项（pv = WebView2 运行时版本）"

    $clientGuid = $script:WebView2ClientGuid
    $keys = @(
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$clientGuid",
        "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$clientGuid",
        "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$clientGuid"
    )

    $foundVersions = New-Object System.Collections.Generic.List[string]
    foreach ($key in $keys) {
        Add-Line $b ("[" + $key + "]")
        if (-not (Test-Path -LiteralPath $key)) {
            Add-Line $b "  <注册表项不存在>"
            continue
        }
        foreach ($valueName in @("pv", "name", "location")) {
            $value = Get-RegistryValue -KeyPath $key -ValueName $valueName
            if ($null -eq $value) {
                Add-Line $b ("  {0} = <未设置>" -f $valueName)
            } else {
                Add-Line $b ("  {0} = {1}" -f $valueName, $value)
                if ($valueName -eq "pv" -and -not [string]::IsNullOrWhiteSpace([string]$value)) {
                    [void]$foundVersions.Add([string]$value)
                }
            }
        }
        Add-Line $b ""
    }

    Add-Header $b "reg.exe 查询（分别按 64 位/32 位注册表视图）"
    foreach ($view in @("/reg:64", "/reg:32")) {
        $queryPath = "HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\$clientGuid"
        $output = @()
        try {
            $output = @(& reg.exe query $queryPath /v pv $view 2>&1)
        } catch {
            $output = @("<reg.exe 调用失败：" + $_.Exception.Message + ">")
        }
        Add-Line $b ("reg.exe query {0} /v pv {1}" -f $queryPath, $view)
        foreach ($line in $output) { Add-Line $b ("  " + [string]$line) }
        Add-Line $b ""
    }

    Add-Header $b "EdgeWebView / EdgeCore 安装目录"
    $candidates = New-Object System.Collections.Generic.List[object]
    $programFilesX86 = Get-ProgramFilesX86
    if ($programFilesX86) {
        [void]$candidates.Add(@{ Path = (Join-Path $programFilesX86 "Microsoft\EdgeWebView\Application"); Label = "Program Files (x86)" })
        [void]$candidates.Add(@{ Path = (Join-Path $programFilesX86 "Microsoft\EdgeCore"); Label = "Program Files (x86) EdgeCore" })
    }
    if ($env:ProgramFiles) {
        [void]$candidates.Add(@{ Path = (Join-Path $env:ProgramFiles "Microsoft\EdgeWebView\Application"); Label = "Program Files" })
    }
    if ($env:LOCALAPPDATA) {
        [void]$candidates.Add(@{ Path = (Join-Path $env:LOCALAPPDATA "Microsoft\EdgeWebView\Application"); Label = "LocalAppData" })
    }

    $runtimeDirFound = $false
    foreach ($candidate in $candidates) {
        if (-not (Test-Path -LiteralPath $candidate.Path)) {
            Add-Line $b ("[{0}] {1} -> 目录不存在" -f $candidate.Label, $candidate.Path)
            continue
        }
        $runtimeDirFound = $true
        Add-Line $b ("[{0}] {1}" -f $candidate.Label, $candidate.Path)
        try {
            $versionDirs = @(Get-ChildItem -LiteralPath $candidate.Path -Directory -Force -ErrorAction Stop | Sort-Object Name -Descending)
            if ($versionDirs.Count -eq 0) { Add-Line $b "  <目录下没有版本子目录>" }
            foreach ($versionDir in $versionDirs) {
                $exePath = Join-Path $versionDir.FullName "msedgewebview2.exe"
                $exeInfo = ""
                if (Test-Path -LiteralPath $exePath) {
                    try {
                        $fileVersion = (Get-Item -LiteralPath $exePath).VersionInfo.FileVersion
                        $exeInfo = ("，msedgewebview2.exe 文件版本 {0}" -f $fileVersion)
                    } catch {
                        $exeInfo = "，msedgewebview2.exe 存在但读取版本失败"
                    }
                } else {
                    $exeInfo = "，未找到 msedgewebview2.exe"
                }
                Add-Line $b ("  版本目录：{0}{1}" -f $versionDir.Name, $exeInfo)
            }
        } catch {
            Add-Line $b ("  <目录读取失败：{0}>" -f $_.Exception.Message)
        }
        Add-Line $b ""
    }
    if (-not $runtimeDirFound) {
        Write-WarnMsg "未找到 EdgeWebView 运行时安装目录。"
    }

    Add-Header $b "结论"
    if ($foundVersions.Count -gt 0) {
        $uniqueVersions = @($foundVersions | Select-Object -Unique)
        Add-Line $b ("检测到 WebView2 Evergreen 运行时，版本：" + ($uniqueVersions -join ", "))
        Write-Ok ("WebView2 运行时版本：" + ($uniqueVersions -join ", "))
    } elseif ($runtimeDirFound) {
        Add-Line $b "注册表中未找到 Evergreen 运行时版本，但检测到 EdgeWebView 安装目录（可能为固定版本或注册信息不完整）。"
        Write-WarnMsg "注册表中未找到 WebView2 Evergreen 版本。"
    } else {
        Add-Line $b "未检测到 WebView2 运行时（Evergreen 注册表项与 EdgeWebView 安装目录均不存在）。"
        Add-Line $b "这是 DBX 无法启动的常见原因：需要确认 WebView2 Runtime 是否已安装、是否被安全软件拦截/卸载。"
        Write-WarnMsg "未检测到 WebView2 运行时，这很可能是 DBX 无法启动的原因。"
    }

    Save-Report $b (Join-Path $Staging "02-webview2-runtime.txt")
    Write-Ok "WebView2 运行时信息已写入 02-webview2-runtime.txt"
}

# ---------------------------------------------------------------------------
# 安全软件
# ---------------------------------------------------------------------------

function Collect-SecurityProducts {
    param([string]$Staging)

    Write-Step "收集安全软件信息"
    $b = New-Object System.Text.StringBuilder
    Add-Header $b "SecurityCenter2 已注册的杀毒产品（Get-CimInstance root\SecurityCenter2 AntiVirusProduct）"

    $securityCenterOk = $false
    try {
        $avProducts = @(Get-CimInstance -Namespace "root\SecurityCenter2" -ClassName "AntiVirusProduct" -ErrorAction Stop)
        $securityCenterOk = $true
        if ($avProducts.Count -eq 0) {
            Add-Line $b "<SecurityCenter2 未返回任何杀毒产品（可能由域策略或第三方安全软件接管）>"
        }
        foreach ($product in $avProducts) {
            Add-Line $b ("显示名称：{0}" -f $product.displayName)
            $stateValue = 0
            try { $stateValue = [uint32]$product.productState } catch { $stateValue = 0 }
            Add-Line $b ("  productState：0x{0:X8}（{0}）" -f $stateValue)
            $heuristic = "未知"
            try {
                $onAccess = (($stateValue -band 0x1000) -ne 0)
                $upToDate = (($stateValue -band 0x10) -ne 0)
                $heuristic = ("实时防护(启发式)={0}，特征库(启发式)={1}" -f $onAccess, $upToDate)
            } catch { }
            Add-Line $b ("  状态解读：" + $heuristic)
            Add-KeyValue $b "  instanceGuid" $product.instanceGuid
            Add-KeyValue $b "  pathToSignedProductExe" $product.pathToSignedProductExe
            Add-KeyValue $b "  pathToSignedReportingExe" $product.pathToSignedReportingExe
            Add-KeyValue $b "  timestamp" $product.timestamp
            Add-Line $b ""
        }
    } catch {
        $securityCenterOk = $false
        Add-Line $b ("读取 SecurityCenter2 失败：" + $_.Exception.Message)
        Write-WarnMsg ("无法通过 SecurityCenter2 读取安全软件列表：{0}" -f $_.Exception.Message)
    }

    Add-Header $b "常见安全软件安装目录探测（SecurityCenter2 不可用时的回退）"
    $programFilesX86 = Get-ProgramFilesX86
    $knownProducts = @(
        @{ Name = "360 安全卫士"; Paths = @(
            (Join-PathSafe $programFilesX86 "360\360Safe"),
            (Join-PathSafe $env:ProgramFiles "360\360Safe"),
            (Join-PathSafe $env:ProgramFiles "360\360safe")
        ) },
        @{ Name = "360 杀毒"; Paths = @(
            (Join-PathSafe $programFilesX86 "360\360sd"),
            (Join-PathSafe $env:ProgramFiles "360\360sd")
        ) },
        @{ Name = "360 天擎/企业版"; Paths = @(
            (Join-PathSafe $programFilesX86 "360\360Safe\Enterprise"),
            (Join-PathSafe $programFilesX86 "360safe\EntClient")
        ) },
        @{ Name = "360 Total Security"; Paths = @(
            (Join-PathSafe $programFilesX86 "360\Total Security"),
            (Join-PathSafe $env:ProgramFiles "360\Total Security")
        ) },
        @{ Name = "火绒安全"; Paths = @(
            (Join-PathSafe $env:ProgramFiles "Huorong\Sysdiag"),
            (Join-PathSafe $programFilesX86 "Huorong\Sysdiag")
        ) },
        @{ Name = "腾讯电脑管家"; Paths = @(
            (Join-PathSafe $programFilesX86 "Tencent\QQPCMgr"),
            (Join-PathSafe $env:ProgramFiles "Tencent\QQPCMgr")
        ) },
        @{ Name = "Kaspersky"; Paths = @((Join-PathSafe $programFilesX86 "Kaspersky Lab")) },
        @{ Name = "ESET"; Paths = @((Join-PathSafe $programFilesX86 "ESET\ESET Security")) },
        @{ Name = "Avast"; Paths = @((Join-PathSafe $env:ProgramFiles "Avast Software")) },
        @{ Name = "McAfee"; Paths = @((Join-PathSafe $env:ProgramFiles "McAfee")) },
        @{ Name = "Windows Defender（系统自带）"; Paths = @((Join-PathSafe $env:ProgramFiles "Windows Defender")) }
    )
    $foundKnownDirs = 0
    foreach ($product in $knownProducts) {
        $productFound = $false
        $existingPaths = @()
        foreach ($candidatePath in $product.Paths) {
            try {
                if ($candidatePath -and (Test-Path -LiteralPath $candidatePath)) {
                    $productFound = $true
                    $existingPaths += $candidatePath
                }
            } catch { }
        }
        if ($productFound) {
            $foundKnownDirs++
            Add-Line $b ("{0}：检测到安装目录 -> {1}" -f $product.Name, ($existingPaths -join "; "))
        }
    }
    if ($foundKnownDirs -eq 0) {
        Add-Line $b "<未在常见安装目录中发现已知安全软件；请结合下面的进程/服务探测结果判断>"
    }

    Add-Header $b "常见安全软件进程探测"
    $knownProcesses = [ordered]@{
        "360tray"      = "360 安全卫士托盘"
        "360safe"      = "360 安全卫士主体"
        "360sd"        = "360 杀毒"
        "ZhuDongFangYu" = "360 主动防御"
        "QHSafeTray"   = "360 安全卫士托盘（企业版）"
        "HipsTray"     = "火绒安全托盘"
        "HipsDaemon"   = "火绒安全服务"
        "HipsMain"     = "火绒安全主程序"
        "usysdiag"     = "火绒系统组件"
        "QQPCRTP"      = "腾讯电脑管家实时防护"
        "QQPCTray"     = "腾讯电脑管家托盘"
        "QQPCMgr"      = "腾讯电脑管家"
        "Avp"          = "Kaspersky"
        "avpui"        = "Kaspersky UI"
        "kavfs"        = "Kaspersky"
        "ekrn"         = "ESET"
        "egui"         = "ESET UI"
        "MsMpEng"      = "Microsoft Defender 引擎"
        "MpCmdRun"     = "Microsoft Defender 命令行"
        "SecurityHealthService" = "Windows 安全中心"
        "avguard"      = "Avira"
        "avgnt"        = "Avira"
        "SavService"   = "Sophos"
        "mcshield"     = "McAfee"
        "navapsvc"     = "Norton"
        "bdservicehost" = "Bitdefender"
        "TMBMSRV"      = "Trend Micro"
    }
    $processFound = 0
    foreach ($processName in $knownProcesses.Keys) {
        try {
            $running = @(Get-Process -Name $processName -ErrorAction SilentlyContinue)
            foreach ($process in $running) {
                $processFound++
                $path = ""
                try { $path = $process.Path } catch { $path = "<无权限读取>" }
                Add-Line $b ("{0}（{1}）：PID {2}，路径 {3}" -f $knownProcesses[$processName], $process.ProcessName, $process.Id, $path)
            }
        } catch { }
    }
    if ($processFound -eq 0) {
        Add-Line $b "<未发现已知安全软件进程>"
    }

    Add-Header $b "常见安全软件服务探测"
    $servicePattern = '(?i)360|huorong|hips|qqpc|tencent|defender|windefend|sense|kaspersky|avp|eset|sophos|avast|mcafee|norton|bitdefender|trend|symantec|securityhealth'
    $serviceFound = 0
    try {
        $services = @(Get-Service -ErrorAction SilentlyContinue | Where-Object { $_.Name -match $servicePattern -or $_.DisplayName -match $servicePattern })
        foreach ($service in $services) {
            $serviceFound++
            Add-Line $b ("{0}（{1}）：状态 {2}" -f $service.DisplayName, $service.Name, $service.Status)
        }
    } catch {
        Add-Line $b ("<服务枚举失败：{0}>" -f $_.Exception.Message)
    }
    if ($serviceFound -eq 0) {
        Add-Line $b "<未发现已知安全软件服务>"
    }

    Add-Header $b "Microsoft Defender 状态（如可用）"
    try {
        if (Get-Command Get-MpComputerStatus -ErrorAction SilentlyContinue) {
            $mpStatus = Get-MpComputerStatus -ErrorAction Stop
            Add-KeyValue $b "AMRunningMode" $mpStatus.AMRunningMode
            Add-KeyValue $b "AntivirusEnabled" $mpStatus.AntivirusEnabled
            Add-KeyValue $b "RealTimeProtectionEnabled" $mpStatus.RealTimeProtectionEnabled
            Add-KeyValue $b "AntivirusSignatureVersion" $mpStatus.AntivirusSignatureVersion
            Add-KeyValue $b "AntivirusSignatureLastUpdated" $mpStatus.AntivirusSignatureLastUpdated
            Add-KeyValue $b "IsTamperProtected" $mpStatus.IsTamperProtected
        } else {
            Add-Line $b "<Get-MpComputerStatus 不可用>"
        }
    } catch {
        Add-Line $b ("<Get-MpComputerStatus 读取失败：{0}>" -f $_.Exception.Message)
    }
    try {
        if (Get-Command Get-MpPreference -ErrorAction SilentlyContinue) {
            $mpPreference = Get-MpPreference -ErrorAction Stop
            Add-KeyValue $b "EnableControlledFolderAccess" $mpPreference.EnableControlledFolderAccess
            Add-KeyValue $b "DisableRealtimeMonitoring" $mpPreference.DisableRealtimeMonitoring
            $exclusionPath = @()
            try { $exclusionPath = @($mpPreference.ExclusionPath) } catch { }
            Add-KeyValue $b "ExclusionPath" (($exclusionPath | Where-Object { $_ }) -join "; ")
            $exclusionProcess = @()
            try { $exclusionProcess = @($mpPreference.ExclusionProcess) } catch { }
            Add-KeyValue $b "ExclusionProcess" (($exclusionProcess | Where-Object { $_ }) -join "; ")
        }
    } catch {
        Add-Line $b ("<Get-MpPreference 读取失败（部分系统需要管理员权限）：{0}>" -f $_.Exception.Message)
    }

    Save-Report $b (Join-Path $Staging "03-security-products.txt")
    if ($securityCenterOk) {
        Write-Ok "安全软件信息已写入 03-security-products.txt"
    } else {
        Write-Ok "安全软件信息（回退探测）已写入 03-security-products.txt"
    }
}

# ---------------------------------------------------------------------------
# 进程信息
# ---------------------------------------------------------------------------

function Collect-ProcessInfo {
    param([string]$Staging)

    Write-Step "收集 msedgewebview2.exe 与 dbx.exe 进程信息"
    $b = New-Object System.Text.StringBuilder

    $parentNames = @{}
    try {
        Get-Process -ErrorAction SilentlyContinue | ForEach-Object {
            try { $parentNames[[int]$_.Id] = $_.ProcessName } catch { }
        }
    } catch { }

    Add-Header $b "msedgewebview2.exe 进程（带 PID/父进程/命令行）"
    $webviewProcesses = @()
    try {
        $webviewProcesses = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = 'msedgewebview2.exe'" -ErrorAction Stop)
    } catch {
        Write-WarnMsg ("WMI 读取 msedgewebview2.exe 失败，改用 Get-Process：" + $_.Exception.Message)
        try {
            $webviewProcesses = @(Get-Process -Name msedgewebview2 -ErrorAction SilentlyContinue)
        } catch { }
    }

    if ($webviewProcesses.Count -eq 0) {
        Add-Line $b "未发现运行中的 msedgewebview2.exe 进程。"
    }
    $index = 0
    foreach ($process in $webviewProcesses) {
        $index++
        $processId = $null
        $parentId = $null
        $executablePath = ""
        $commandLine = ""
        $creationDate = ""
        $processName = "msedgewebview2.exe"
        try { $processId = $process.ProcessId } catch { }
        try { $parentId = $process.ParentProcessId } catch { }
        try { $executablePath = $process.ExecutablePath } catch { }
        try { $commandLine = $process.CommandLine } catch { }
        try { $creationDate = $process.CreationDate } catch { }
        if ($null -eq $processId) {
            try { $processId = $process.Id } catch { }
            try { $executablePath = $process.Path } catch { }
            try { $processName = $process.ProcessName } catch { }
        }
        Add-Line $b ("[{0}] PID {1}，启动时间 {2}" -f $index, $processId, $creationDate)
        if ($null -ne $parentId) {
            $parentName = "<未知>"
            if ($parentNames.ContainsKey([int]$parentId)) { $parentName = $parentNames[[int]$parentId] }
            Add-Line $b ("  父进程：PID {0}（{1}）" -f $parentId, $parentName)
        }
        Add-Line $b ("  可执行文件：{0}" -f $executablePath)
        if ($executablePath -and (Test-Path -LiteralPath $executablePath)) {
            try {
                $versionInfo = (Get-Item -LiteralPath $executablePath).VersionInfo
                Add-Line $b ("  文件版本：{0}，产品版本：{1}" -f $versionInfo.FileVersion, $versionInfo.ProductVersion)
            } catch { }
        }
        if ($commandLine) {
            $displayCommandLine = [string]$commandLine
            if ($displayCommandLine.Length -gt 1500) { $displayCommandLine = $displayCommandLine.Substring(0, 1500) + " ...（已截断）" }
            Add-Line $b ("  命令行：{0}" -f $displayCommandLine)
        }
        Add-Line $b ""
    }
    if ($webviewProcesses.Count -gt 0) {
        Add-Line $b "提示：如果 DBX 主程序已退出但仍有 msedgewebview2.exe 残留，说明 WebView2 子进程可能被安全软件挂起或未能退出。"
    }

    Add-Header $b "dbx.exe 进程"
    $dbxProcesses = @()
    try {
        $dbxProcesses = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = 'dbx.exe'" -ErrorAction Stop)
    } catch {
        try { $dbxProcesses = @(Get-Process -Name dbx -ErrorAction SilentlyContinue) } catch { }
    }
    if ($dbxProcesses.Count -eq 0) {
        Add-Line $b "未发现运行中的 dbx.exe 进程。"
    }
    foreach ($process in $dbxProcesses) {
        $processId = $null
        $executablePath = ""
        $commandLine = ""
        $creationDate = ""
        try { $processId = $process.ProcessId } catch { }
        try { $executablePath = $process.ExecutablePath } catch { }
        try { $commandLine = $process.CommandLine } catch { }
        try { $creationDate = $process.CreationDate } catch { }
        if ($null -eq $processId) {
            try { $processId = $process.Id } catch { }
            try { $executablePath = $process.Path } catch { }
        }
        Add-Line $b ("PID {0}，启动时间 {1}" -f $processId, $creationDate)
        Add-Line $b ("  可执行文件：{0}" -f $executablePath)
        if ($commandLine) {
            $displayCommandLine = [string]$commandLine
            if ($displayCommandLine.Length -gt 1500) { $displayCommandLine = $displayCommandLine.Substring(0, 1500) + " ...（已截断）" }
            Add-Line $b ("  命令行：{0}" -f $displayCommandLine)
        }
        Add-Line $b ""
    }

    Save-Report $b (Join-Path $Staging "04-processes.txt")
    Write-Ok ("进程信息已写入 04-processes.txt（msedgewebview2.exe 数量：{0}）" -f $webviewProcesses.Count)
}

# ---------------------------------------------------------------------------
# DBX 安装信息与签名
# ---------------------------------------------------------------------------

function Get-SignatureReportLines {
    param([string]$ExePath)
    $lines = New-Object System.Collections.Generic.List[string]
    try {
        $signature = Get-AuthenticodeSignature -FilePath $ExePath -ErrorAction Stop
        [void]$lines.Add("  数字签名状态：" + $signature.Status + "（" + $signature.StatusMessage + "）")
        if ($signature.SignerCertificate) {
            [void]$lines.Add("  签名者：" + $signature.SignerCertificate.Subject)
            [void]$lines.Add("  颁发者：" + $signature.SignerCertificate.Issuer)
            [void]$lines.Add("  证书指纹：" + $signature.SignerCertificate.Thumbprint)
            [void]$lines.Add("  证书有效期：" + $signature.SignerCertificate.NotBefore.ToString("yyyy-MM-dd") + " ~ " + $signature.SignerCertificate.NotAfter.ToString("yyyy-MM-dd"))
        } else {
            [void]$lines.Add("  未包含签名者证书：该文件很可能未签名（测试构建通常为未签名）。")
        }
        if ($signature.TimeStamperCertificate) {
            [void]$lines.Add("  时间戳签名者：" + $signature.TimeStamperCertificate.Subject)
        }
    } catch {
        [void]$lines.Add("  读取数字签名失败：" + $_.Exception.Message)
        Write-WarnMsg ("读取数字签名失败：{0}（{1}）" -f $ExePath, $_.Exception.Message)
    }
    return $lines
}

function Collect-DbxInstallInfo {
    param([string]$Staging)

    Write-Step "收集 DBX 安装信息与可执行文件签名"
    $b = New-Object System.Text.StringBuilder
    $installLocations = New-Object System.Collections.Generic.List[string]

    Add-Header $b "运行中的 dbx.exe"
    $runningPaths = New-Object System.Collections.Generic.List[string]
    try {
        $dbxProcesses = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = 'dbx.exe'" -ErrorAction Stop)
        foreach ($process in $dbxProcesses) {
            if ($process.ExecutablePath) { [void]$runningPaths.Add([string]$process.ExecutablePath) }
        }
    } catch { }
    if ($runningPaths.Count -eq 0) {
        try {
            Get-Process -Name dbx -ErrorAction SilentlyContinue | ForEach-Object {
                try { if ($_.Path) { [void]$runningPaths.Add([string]$_.Path) } } catch { }
            }
        } catch { }
    }
    if ($runningPaths.Count -eq 0) {
        Add-Line $b "未发现运行中的 dbx.exe 进程。"
    }
    foreach ($runningPath in ($runningPaths | Select-Object -Unique)) {
        Add-Line $b ("路径：" + $runningPath)
        try {
            $file = Get-Item -LiteralPath $runningPath -ErrorAction Stop
            Add-Line $b ("  文件大小：{0:N2} MB" -f ($file.Length / 1MB))
            Add-Line $b ("  产品版本：" + $file.VersionInfo.ProductVersion)
            Add-Line $b ("  文件版本：" + $file.VersionInfo.FileVersion)
            Add-Line $b ("  产品名称：" + $file.VersionInfo.ProductName)
        } catch { }
        foreach ($line in (Get-SignatureReportLines -ExePath $runningPath)) { Add-Line $b $line }
        Add-Line $b ""
    }

    Add-Header $b "卸载注册表项（DisplayName 含 DBX）"
    $uninstallRoots = @(
        "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"
    )
    $uninstallFound = 0
    foreach ($root in $uninstallRoots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        try {
            $subKeys = @(Get-ChildItem -LiteralPath $root -ErrorAction Stop)
        } catch {
            continue
        }
        foreach ($subKey in $subKeys) {
            try {
                $properties = Get-ItemProperty -LiteralPath $subKey.PSPath -ErrorAction Stop
                $displayName = [string]$properties.DisplayName
                if ([string]::IsNullOrWhiteSpace($displayName)) { continue }
                if ($displayName -notmatch "(?i)dbx") { continue }
                $uninstallFound++
                Add-Line $b ("显示名称：{0}" -f $displayName)
                Add-KeyValue $b "  DisplayVersion" $properties.DisplayVersion
                Add-KeyValue $b "  Publisher" $properties.Publisher
                Add-KeyValue $b "  InstallLocation" $properties.InstallLocation
                Add-KeyValue $b "  DisplayIcon" $properties.DisplayIcon
                Add-KeyValue $b "  UninstallString" $properties.UninstallString
                Add-KeyValue $b "  InstallDate" $properties.InstallDate
                if (-not [string]::IsNullOrWhiteSpace([string]$properties.InstallLocation)) {
                    [void]$installLocations.Add([string]$properties.InstallLocation)
                }
                Add-Line $b ""
            } catch {
                # 单个卸载项读取失败不影响整体
            }
        }
    }
    if ($uninstallFound -eq 0) {
        Add-Line $b "<未找到 DBX 卸载注册表项（可能为便携版或安装信息不完整）>"
    }

    Add-Header $b "候选安装路径探测"
    $candidatePaths = New-Object System.Collections.Generic.List[string]
    $candidateBases = New-Object System.Collections.Generic.List[object]
    if ($env:LOCALAPPDATA) {
        [void]$candidateBases.Add(@{ Base = (Join-Path $env:LOCALAPPDATA "Programs\DBX"); Label = "LocalAppData Programs" })
        [void]$candidateBases.Add(@{ Base = (Join-Path $env:LOCALAPPDATA "DBX"); Label = "LocalAppData" })
    }
    if ($env:ProgramFiles) {
        [void]$candidateBases.Add(@{ Base = (Join-Path $env:ProgramFiles "DBX"); Label = "Program Files" })
    }
    $programFilesX86 = Get-ProgramFilesX86
    if ($programFilesX86) {
        [void]$candidateBases.Add(@{ Base = (Join-Path $programFilesX86 "DBX"); Label = "Program Files (x86)" })
    }
    foreach ($location in ($installLocations | Select-Object -Unique)) {
        [void]$candidateBases.Add(@{ Base = $location; Label = "卸载信息 InstallLocation" })
    }
    foreach ($candidateBase in $candidateBases) {
        foreach ($exeName in @("DBX.exe", "dbx.exe")) {
            $candidate = Join-Path $candidateBase.Base $exeName
            if (-not ($candidatePaths -contains $candidate)) { [void]$candidatePaths.Add($candidate) }
        }
    }
    # Tauri NSIS 默认也可能把程序安装到 %LOCALAPPDATA%\Programs\<产品名>，做一次有限枚举。
    if ($env:LOCALAPPDATA) {
        try {
            $programsDir = Join-Path $env:LOCALAPPDATA "Programs"
            if (Test-Path -LiteralPath $programsDir) {
                Get-ChildItem -LiteralPath $programsDir -Directory -ErrorAction Stop |
                    Where-Object { $_.Name -match "(?i)dbx" } |
                    ForEach-Object {
                        foreach ($exeName in @("DBX.exe", "dbx.exe")) {
                            $candidate = Join-Path $_.FullName $exeName
                            if (-not ($candidatePaths -contains $candidate)) { [void]$candidatePaths.Add($candidate) }
                        }
                    }
            }
        } catch { }
    }

    $existingExe = $null
    $foundCandidateCount = 0
    foreach ($candidatePath in $candidatePaths) {
        try {
            if (Test-Path -LiteralPath $candidatePath) {
                $foundCandidateCount++
                Add-Line $b ("找到：" + $candidatePath)
                if (-not $existingExe) { $existingExe = $candidatePath }
            }
        } catch { }
    }
    if ($foundCandidateCount -eq 0) {
        Add-Line $b "<未在候选路径中找到 DBX.exe/dbx.exe>"
        Add-Line $b "（便携版可执行文件位置请以实际解压目录为准。）"
    }

    Add-Header $b "已安装 DBX 可执行文件详情"
    if ($existingExe) {
        try {
            $file = Get-Item -LiteralPath $existingExe -ErrorAction Stop
            Add-KeyValue $b "路径" $file.FullName
            Add-KeyValue $b "大小(MB)" ("{0:N2}" -f ($file.Length / 1MB))
            Add-KeyValue $b "最后修改时间" $file.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss")
            Add-KeyValue $b "产品名称" $file.VersionInfo.ProductName
            Add-KeyValue $b "产品版本" $file.VersionInfo.ProductVersion
            Add-KeyValue $b "文件版本" $file.VersionInfo.FileVersion
            Add-KeyValue $b "公司名称" $file.VersionInfo.CompanyName
            Add-KeyValue $b "文件说明" $file.VersionInfo.FileDescription
            try {
                $sha256 = (Get-FileHash -LiteralPath $existingExe -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
                Add-KeyValue $b "SHA256" $sha256
            } catch {
                Add-KeyValue $b "SHA256" "<计算失败>"
                Write-WarnMsg ("计算 DBX.exe 的 SHA256 失败：" + $_.Exception.Message)
            }
        } catch {
            Add-Line $b ("读取文件信息失败：" + $_.Exception.Message)
        }
        foreach ($line in (Get-SignatureReportLines -ExePath $existingExe)) { Add-Line $b $line }
    } else {
        Add-Line $b "<未找到已安装的 DBX 可执行文件，跳过版本与签名检查。>"
    }

    Save-Report $b (Join-Path $Staging "05-dbx-install.txt")
    Write-Ok "DBX 安装信息已写入 05-dbx-install.txt"
}

# ---------------------------------------------------------------------------
# 事件日志
# ---------------------------------------------------------------------------

function Collect-EventLog {
    param([string]$Staging)

    Write-Step "读取最近 3 天与 DBX/WebView2 相关的应用程序事件日志"
    $b = New-Object System.Text.StringBuilder
    Add-Header $b "应用程序日志（最近 3 天，最多 600 条，筛选 DBX/WebView2 相关）"
    try {
        $startTime = (Get-Date).AddDays(-3)
        $events = @(Get-WinEvent -FilterHashtable @{ LogName = "Application"; StartTime = $startTime } -MaxEvents 600 -ErrorAction Stop)
        $matched = New-Object System.Collections.Generic.List[object]
        foreach ($event in $events) {
            try {
                $message = [string]$event.Message
                if ($message -match "(?i)dbx|webview2|msedgewebview2") {
                    [void]$matched.Add($event)
                }
            } catch { }
        }
        if ($matched.Count -eq 0) {
            Add-Line $b "最近 3 天未发现与 DBX/WebView2 相关的应用程序错误事件。"
        } else {
            foreach ($event in ($matched | Select-Object -First 60)) {
                Add-Line $b ("时间：{0}  级别：{1}  事件 ID：{2}  来源：{3}" -f $event.TimeCreated.ToString("yyyy-MM-dd HH:mm:ss"), $event.LevelDisplayName, $event.Id, $event.ProviderName)
                try {
                    $message = [string]$event.Message
                    if ($message.Length -gt 1200) { $message = $message.Substring(0, 1200) + " ...（已截断）" }
                    Add-Line $b ("  " + ($message -replace "`r`n", "`r`n  "))
                } catch { }
                Add-Line $b ""
            }
        }
    } catch {
        Add-Line $b ("读取事件日志失败（可能日志为空或权限不足）：" + $_.Exception.Message)
        Write-WarnMsg ("读取应用程序事件日志失败：" + $_.Exception.Message)
    }

    Save-Report $b (Join-Path $Staging "06-event-log-recent.txt")
    Write-Ok "事件日志摘要已写入 06-event-log-recent.txt"
}

# ---------------------------------------------------------------------------
# README
# ---------------------------------------------------------------------------

function Write-Readme {
    param(
        [string]$Staging,
        [string]$ZipPath
    )

    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("DBX 启动问题日志收集包")
    $lines.Add("========================")
    $lines.Add("")
    $lines.Add(("收集时间：" + (Get-Date -Format "yyyy-MM-dd HH:mm:ss zzz")))
    $lines.Add(("计算机名：" + [Environment]::MachineName))
    $lines.Add(("当前用户：" + [Environment]::UserName))
    $lines.Add(("收集脚本版本：" + $script:ScriptVersion))
    $lines.Add("")
    $lines.Add("## 这个压缩包里有什么")
    $lines.Add("")
    $lines.Add("  README.txt                        本说明文件")
    $lines.Add("  01-system-info.txt                Windows 版本/内部版本号、区域、提权状态、相关环境变量、磁盘空间等")
    $lines.Add("  02-webview2-runtime.txt           WebView2 运行时版本（注册表 + EdgeWebView 安装目录），缺失时会明确标注")
    $lines.Add("  03-security-products.txt          安全软件信息（SecurityCenter2 注册信息、常见安全软件进程/服务/目录探测、Defender 状态）")
    $lines.Add("  04-processes.txt                  msedgewebview2.exe 与 dbx.exe 进程列表（PID、父进程、命令行、文件版本）")
    $lines.Add("  05-dbx-install.txt                DBX 安装版本、可执行文件位置、SHA256、数字签名状态")
    $lines.Add("  06-event-log-recent.txt           最近 3 天应用程序日志中与 DBX/WebView2 相关的错误（尽力而为）")
    $lines.Add("  logs\                             从 DBX 日志目录复制的启动日志及清单：")
    $lines.Add("      log-file-list.txt             已收集日志文件清单（来源、大小、SHA256）")
    $lines.Add("      startup.log                   DBX 启动探测日志（若存在）")
    $lines.Add("      webview2-recovery.log         WebView2 故障恢复日志（若存在）")
    $lines.Add("      webview2-preflight-*.log      WebView2 启动预检日志（若存在）")
    $lines.Add("      compatibility-marker-and-data-dirs.txt  兼容模式标记与 DBX 数据目录清单")
    $lines.Add("      compatibility-*\              兼容模式标记文件副本（webview2-enterprise-compat.enabled）")
    $lines.Add("")
    $lines.Add("## 复现步骤（重要）")
    $lines.Add("")
    $lines.Add("1. 完全退出 DBX（包括任务栏右下角托盘图标；必要时在任务管理器中结束 dbx.exe 与 msedgewebview2.exe）。")
    $lines.Add("2. 建议在启动 DBX 前，在「命令提示符」中执行下面两行，让启动日志保留到专用目录：")
    $lines.Add("")
    $lines.Add("       set DBX_KEEP_STARTUP_LOG=1")
    $lines.Add("       set DBX_STARTUP_LOG_DIR=%USERPROFILE%\Desktop\dbx-startup-capture")
    $lines.Add("")
    $lines.Add("   然后在同一个命令提示符窗口中启动 DBX（可直接输入 DBX.exe 的完整路径）。")
    $lines.Add("   如果使用了 DBX_STARTUP_LOG_DIR，请在同一窗口运行本收集脚本（脚本会自动读取该目录），")
    $lines.Add("   或手动把该目录一起打包发给我们。")
    $lines.Add("3. 启动 DBX，等待 60~120 秒，直到确认无法显示主界面或出现报错，然后关闭 DBX。")
    $lines.Add("4. 找到 collect-dbx-startup-logs.ps1，右键选择「使用 PowerShell 运行」。")
    $lines.Add("   如果系统限制运行脚本，可在命令提示符中执行：")
    $lines.Add("")
    $lines.Add("       powershell -NoProfile -ExecutionPolicy Bypass -File .\collect-dbx-startup-logs.ps1")
    $lines.Add("")
    $lines.Add("5. 脚本结束时会显示 zip 路径（默认在桌面）。请把该 zip 压缩包发回给技术支持。")
    $lines.Add("")
    $lines.Add("## 说明")
    $lines.Add("")
    $lines.Add("- 脚本只读取和复制信息，不修改系统设置、不删除任何文件，不需要管理员权限。")
    $lines.Add("- 个别项目读取失败时会以黄色警告显示，不影响其它信息的收集。")
    $lines.Add("- 若设置了 DBX_STARTUP_LOG_DIR，请确认运行收集脚本时该环境变量仍然有效；否则请手动提供该目录。")
    $lines.Add("- startup.log 默认只在启动失败/恢复流程中保留；设置 DBX_KEEP_STARTUP_LOG=1 可确保日志保留。")
    $lines.Add("- 收集包可能包含用户名、计算机名、本地路径、数据库连接名等隐私信息，发送前请自行确认。")
    $lines.Add("- 所有文本文件均为 UTF-8 编码，可用记事本直接打开。")
    $lines.Add("")
    $lines.Add("## 本次收集结果摘要")
    $lines.Add("")
    if ($script:CollectedLogs.Count -eq 0) {
        $lines.Add("- 未收集到 DBX 日志文件（请确认是否已按上面的步骤复现过启动失败）。")
    } else {
        $lines.Add(("- 共收集 DBX 日志文件 {0} 个：" -f $script:CollectedLogs.Count))
        foreach ($log in $script:CollectedLogs) {
            $lines.Add(("    * {0}（来源：{1}）" -f [System.IO.Path]::GetFileName($log.Destination), $log.Source))
        }
    }
    if ($script:Warnings.Count -gt 0) {
        $lines.Add(("- 收集过程中的警告 {0} 条：" -f $script:Warnings.Count))
        foreach ($warning in $script:Warnings) {
            $lines.Add(("    * " + $warning))
        }
    } else {
        $lines.Add("- 收集过程中没有出现警告。")
    }
    if ($ZipPath) {
        $lines.Add("")
        $lines.Add(("压缩包：" + $ZipPath))
    }

    $content = ($lines -join "`r`n") + "`r`n"
    $path = Join-Path $Staging "README.txt"
    [System.IO.File]::WriteAllText($path, $content, (New-Object System.Text.UTF8Encoding($true)))
}

# ---------------------------------------------------------------------------
# 输出目录与打包
# ---------------------------------------------------------------------------

function Resolve-OutputDir {
    param([string]$Requested)
    if (-not [string]::IsNullOrWhiteSpace($Requested)) {
        try {
            if (-not (Test-Path -LiteralPath $Requested)) {
                New-Item -ItemType Directory -Force -Path $Requested | Out-Null
            }
            return (Resolve-Path -LiteralPath $Requested).Path
        } catch {
            Write-WarnMsg ("指定的输出目录不可用：{0}（{1}）" -f $Requested, $_.Exception.Message)
        }
    }
    try {
        $desktop = [Environment]::GetFolderPath([Environment+SpecialFolder]::Desktop)
        if (-not [string]::IsNullOrWhiteSpace($desktop) -and (Test-Path -LiteralPath $desktop)) {
            return $desktop
        }
    } catch { }
    try {
        $fallbackDesktop = Join-Path $env:USERPROFILE "Desktop"
        if (Test-Path -LiteralPath $fallbackDesktop) { return $fallbackDesktop }
    } catch { }
    Write-WarnMsg ("未找到桌面目录，压缩包将保存到用户目录：" + $env:USERPROFILE)
    return $env:USERPROFILE
}

function New-ZipPackage {
    param(
        [string]$Staging,
        [string]$OutputDirectory
    )

    Write-Step "打包收集结果"
    $hostName = Get-HostNameSafe
    $timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $zipName = "DBX-startup-logs-{0}-{1}.zip" -f $hostName, $timestamp
    $zipPath = Join-Path $OutputDirectory $zipName
    if (Test-Path -LiteralPath $zipPath) {
        $zipName = "DBX-startup-logs-{0}-{1}-1.zip" -f $hostName, $timestamp
        $zipPath = Join-Path $OutputDirectory $zipName
    }

    Compress-Archive -Path (Join-Path $Staging "*") -DestinationPath $zipPath -CompressionLevel Optimal -ErrorAction Stop
    if (-not (Test-Path -LiteralPath $zipPath)) {
        throw ("压缩包未生成：" + $zipPath)
    }
    $zipItem = Get-Item -LiteralPath $zipPath
    $sha256 = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
    return [pscustomobject]@{
        Path = $zipPath
        Bytes = $zipItem.Length
        Sha256 = $sha256
    }
}

# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host ("DBX 启动问题日志收集工具 v" + $script:ScriptVersion) -ForegroundColor White
Write-Host "只读收集，不会修改系统设置或删除文件；无需管理员权限。" -ForegroundColor Gray
Write-Host ""

if ($env:OS -ne "Windows_NT") {
    Write-Host "[错误] 本脚本仅支持 Windows 系统（Windows 10 / 11）。" -ForegroundColor Red
    if (-not $NoPause -and [Environment]::UserInteractive) {
        [void](Read-Host "按 Enter 键退出")
    }
    exit 1
}

$outputDirectory = Resolve-OutputDir -Requested $OutputDir
$stagingName = "dbx-startup-logs-staging-" + (Get-Date -Format "yyyyMMdd-HHmmss") + "-" + [System.IO.Path]::GetRandomFileName().Substring(0, 6)
$stagingDir = Join-Path $env:TEMP $stagingName
try {
    New-Item -ItemType Directory -Force -Path $stagingDir -ErrorAction Stop | Out-Null
} catch {
    Write-Host ("[错误] 无法创建临时目录：" + $stagingDir) -ForegroundColor Red
    Write-Host ("        原因：" + $_.Exception.Message) -ForegroundColor Red
    exit 1
}
Write-Info ("临时整理目录：" + $stagingDir)
Write-Info ("输出目录：" + $outputDirectory)

Invoke-Capture -Name "系统信息" -Action { Collect-SystemInfo -Staging $stagingDir }
Invoke-Capture -Name "WebView2 运行时" -Action { Collect-WebView2Runtime -Staging $stagingDir }
Invoke-Capture -Name "安全软件" -Action { Collect-SecurityProducts -Staging $stagingDir }
Invoke-Capture -Name "进程信息" -Action { Collect-ProcessInfo -Staging $stagingDir }
Invoke-Capture -Name "DBX 安装信息" -Action { Collect-DbxInstallInfo -Staging $stagingDir }
Invoke-Capture -Name "事件日志" -Action { Collect-EventLog -Staging $stagingDir }
Invoke-Capture -Name "DBX 日志文件" -Action { Collect-LogFiles -Staging $stagingDir }
Invoke-Capture -Name "兼容模式标记" -Action { Collect-CompatibilityArtifacts -Staging $stagingDir }

try {
    Write-Readme -Staging $stagingDir -ZipPath ""
} catch {
    Write-WarnMsg ("生成 README.txt 失败：" + $_.Exception.Message)
}

$zipResult = $null
try {
    $zipResult = New-ZipPackage -Staging $stagingDir -OutputDirectory $outputDirectory
} catch {
    Write-WarnMsg ("自动打包失败：" + $_.Exception.Message)
}
$script:ZipPath = $zipResult

Write-Host ""
if ($zipResult) {
    Write-Host "==============================================================" -ForegroundColor Green
    Write-Host " 日志收集完成" -ForegroundColor Green
    Write-Host "==============================================================" -ForegroundColor Green
    Write-Host (" 压缩包路径：" + $zipResult.Path) -ForegroundColor Green
    Write-Host (" 文件大小：" + ("{0:N2} MB" -f ($zipResult.Bytes / 1MB)))
    Write-Host (" SHA256：" + $zipResult.Sha256)
    Write-Host (" 临时整理目录：" + $stagingDir) -ForegroundColor Gray
    if ($script:Warnings.Count -gt 0) {
        Write-Host (" 收集警告：" + $script:Warnings.Count + " 条（详见 README.txt 与各 txt 文件）") -ForegroundColor Yellow
    }
    Write-Host ""
    Write-Host " 请把上面的 zip 压缩包发送给 DBX 技术支持。" -ForegroundColor Cyan
    Write-Host " 压缩包可能包含用户名、路径、数据库连接名等信息，发送前请自行确认。" -ForegroundColor Gray
} else {
    Write-Host "==============================================================" -ForegroundColor Yellow
    Write-Host " 收集已完成，但自动打包失败" -ForegroundColor Yellow
    Write-Host "==============================================================" -ForegroundColor Yellow
    Write-Host (" 请手动把下面的目录压缩后发送给我们：" + $stagingDir) -ForegroundColor Yellow
    if ($script:Warnings.Count -gt 0) {
        Write-Host (" 收集警告：" + $script:Warnings.Count + " 条") -ForegroundColor Yellow
        foreach ($warning in $script:Warnings) {
            Write-Host ("   - " + $warning) -ForegroundColor Yellow
        }
    }
}

if (-not $NoPause -and [Environment]::UserInteractive) {
    Write-Host ""
    [void](Read-Host "按 Enter 键退出")
}
