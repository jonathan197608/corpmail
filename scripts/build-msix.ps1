# ============================================================
# CorpMail · MSIX 商店包构建（Microsoft Store 渠道）· PowerShell 原生版
# 宿主：Windows（需 Windows SDK：makeappx 打包 + WACK 校验）
#
# 【编码】本文件必须为 UTF-8 with BOM（当前已是）。Windows PowerShell 5.1
#   对无 BOM 脚本按系统 ANSI（中文系统 = GBK）解码，中文注释与
#   Write-Host 输出会乱码，甚至引发语法错误。
#   编辑后请保留 BOM，勿转存为无 BOM UTF-8。
#
# 前置链路（部署方案确认）：
#   1) macOS 交叉编译出 NSIS：scripts/build-windows.sh
#   2) 本机（Windows）安装 CorpMail_x.y.z_x64-setup.exe 一次
#   3) 本脚本取安装目录 → 重打包 MSIX
#
# 产物：store-assets/msix/out/CorpMail_x.y.z.0_x64.msix（未签名，
#       商店认证后由微软代签）
# 用法：powershell -ExecutionPolicy Bypass -File scripts\build-msix.ps1 [-InstallDir "D:\CorpMail"]
# ============================================================
param(
    # CorpMail 安装目录（NSIS 默认装到 Program Files；自装位置可传参覆盖）
    [string]$InstallDir = ""
)

$ErrorActionPreference = "Stop"

# ---- 定位仓库根（脚本在 <repo>/scripts/ 下；也支持从任意 cwd 调用）----
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$ManifestDir = Join-Path $Root "store-assets\msix"
$AssetsDir = Join-Path $ManifestDir "assets"
$OutDir = Join-Path $ManifestDir "out"

# ---- 版本：读 src-tauri/Cargo.toml（与商店 manifest 注入同源）----
# 【中文兼容】Cargo.toml 含中文注释，无 BOM；PS5.1 的 -Raw 默认按 ANSI
#   （GBK）解码会乱码，必须显式 -Encoding UTF8。下同（AppxManifest）。
$CargoToml = Get-Content (Join-Path $Root "src-tauri\Cargo.toml") -Raw -Encoding UTF8
if ($CargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    Write-Error "[msix] 无法从 src-tauri/Cargo.toml 解析版本号"
}
$Version = $Matches[1]
Write-Host "[msix] 版本 $Version.0"

# ---- 安装目录：未传参则自动探测（NSIS perMachine 默认位置 + 常见自装位置）----
if (-not $InstallDir) {
    $candidates = @(
        "$env:ProgramFiles\CorpMail",
        "${env:ProgramFiles(x86)}\CorpMail",
        "D:\CorpMail", "C:\CorpMail"
    )
    $InstallDir = $candidates | Where-Object { Test-Path (Join-Path $_ "CorpMail.exe") } | Select-Object -First 1
    if (-not $InstallDir) {
        Write-Error "[msix] 未探测到 CorpMail 安装目录（找 CorpMail.exe）。请先安装 CorpMail_${Version}_x64-setup.exe，或以 -InstallDir 指定。探测过：$($candidates -join ' | ')"
    }
}
if (-not (Test-Path (Join-Path $InstallDir "CorpMail.exe"))) {
    Write-Error "[msix] $InstallDir 下没有 CorpMail.exe（入口文件缺失）"
}
Write-Host "[msix] 源目录（安装目录）：$InstallDir"

# ---- makeappx：取 Windows SDK 最新版 ----
$sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
if (-not (Test-Path $sdkRoot)) {
    Write-Error "[msix] 未找到 Windows SDK（$sdkRoot）。请安装 Windows SDK（任意近版，勾选 SDK 桌版工具）。"
}
$makeappx = Get-ChildItem $sdkRoot -Directory |
    # SDK 目录名为版本号（10.0.26100.0 等）：[version] 数值排序取最新；
    # 字符串排序会把 10.0.9xxx 误排在 10.0.26xxx 前
    Sort-Object { if ($_.Name -match '^\d+(\.\d+)+$') { [version]$_.Name } else { [version]'0.0' } } -Descending |
    ForEach-Object { Join-Path $_.FullName "x64\makeappx.exe" } |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $makeappx) {
    Write-Error "[msix] SDK 已装但未找到 x64\makeappx.exe（安装 SDK 时需包含桌面工具）"
}
Write-Host "[msix] makeappx：$makeappx"

# ---- 1) 布包：manifest + 图标 + 安装目录内容 ----
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ("corpmail-msix-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $Stage | Out-Null
try {
    Copy-Item (Join-Path $ManifestDir "AppxManifest.xml") $Stage
    New-Item -ItemType Directory -Path (Join-Path $Stage "assets") | Out-Null
    Copy-Item (Join-Path $AssetsDir "*.png") (Join-Path $Stage "assets")

    # 安装目录全量拷入（CorpMail.exe / WebView2Loader.dll 等）；剔除卸载器与安装日志
    Copy-Item "$InstallDir\*" $Stage -Recurse -Force
    @("Uninstall.exe", "uninstall.exe", "install.log") | ForEach-Object {
        $p = Join-Path $Stage $_
        if (Test-Path $p) { Remove-Item $p -Force }
    }

    # ---- 2) 版本号注入 manifest（四段制，末段 0）----
    # 读显式 -Encoding UTF8（manifest 无 BOM 且含中文注释/名称，防 GBK 乱码）；
    # 写回 -Encoding UTF8（PS5.1 = UTF-8 with BOM，XML 规范允许，makeappx 接受）
    $manifestPath = Join-Path $Stage "AppxManifest.xml"
    (Get-Content $manifestPath -Raw -Encoding UTF8) -replace 'Version="[0-9.]+"', "Version="$Version.0"" |
        Set-Content $manifestPath -Encoding UTF8 -NoNewline

    # ---- 3) MakeAppx 打包 ----
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    $msix = Join-Path $OutDir "CorpMail_$Version.0_x64.msix"
    & $makeappx pack /d $Stage /p $msix /o
    if ($LASTEXITCODE -ne 0) { Write-Error "[msix] makeappx 失败（exit $LASTEXITCODE）" }

    # ---- 4) 输出与 WACK 提示 ----
    Write-Host ""
    Write-Host "[msix] 产物：$msix" -ForegroundColor Green
    $sizeMb = [math]::Round((Get-Item $msix).Length / 1MB, 2)
    Write-Host "[msix] 大小：$sizeMb MB"

    Write-Host ""
    Write-Host "本地验证（可选，自签名临时安装）："
    Write-Host "  Add-AppxPackage -Path `"$msix`" -ForceApplicationShutdown"
    Write-Host "提交前请跑 WACK 认证预检："
    $wack = "${env:ProgramFiles(x86)}\Windows Kits\10\App Certification Kit\appcert.exe"
    Write-Host "  `"$wack`" reset"
    Write-Host "  `"$wack`" test -apptype desktop -appxpath `"$msix`""
    Write-Host "确认项：virtualization 禁用生效、Identity 三件套与 Partner Center 一致、WACK 零 FAIL"
}
finally {
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
}
