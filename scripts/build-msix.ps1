# ============================================================
# CorpMail · MSIX 商店包构建（微软官方 WinApp CLI）
#
# 【一次性准备】（每台打包机只做一次）
#   winget install Microsoft.winappcli --source winget
#   首次 pack 需联网从 nuget.org 下载 makeappx 等工具（之后走本地缓存）
#   文档：https://learn.microsoft.com/windows/apps/dev-tools/winapp-cli/
#
# 【流程】本机已用 NSIS 包安装并测试过（部署链路第 1 步），
#   本脚本只做一件事：出未签名商店提交包（商店认证后微软代签，
#   上传时的签名一律被替换，无需本地证书）
#   → Product identity 校验全靠 manifest Identity 三件套（已定稿）
#   → NSIS 渠道的本地验收/功能测试先行，本包直接进商店提交流程
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-msix.ps1
#   产物：store-assets\msix\out\CorpMail_x.y.z.0_x64.msix（未签名）
#
# 【编码】本文件必须为 UTF-8 with BOM（PS5.1 无 BOM 按 GBK 解码会乱码），
#   编辑后请保留 BOM。
# ============================================================
param(
    # exe/dll 来源：NSIS 安装目录（默认自动探测；自定义位置传参覆盖）
    [string]$InstallDir = "",
    # winapp.exe 路径（默认 PATH 探测；standalone zip 安装时指定）
    [string]$WinApp = ""
)

$ErrorActionPreference = "Stop"

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$ManifestDir = Join-Path $Root "store-assets\msix"
$AssetsDir = Join-Path $ManifestDir "assets"
$OutDir = Join-Path $ManifestDir "out"

# ---- 版本号（读 Cargo.toml，注入 manifest）----
# 【中文兼容】Cargo.toml/AppxManifest 含中文且无 BOM，-Raw 必须显式
#   -Encoding UTF8，否则 PS5.1 按 ANSI（GBK）解码会乱码
$CargoToml = Get-Content (Join-Path $Root "src-tauri\Cargo.toml") -Raw -Encoding UTF8
if ($CargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    Write-Error "[msix] 无法从 src-tauri/Cargo.toml 解析版本号"
}
$Version = $Matches[1]

# ---- exe/dll 来源：安装目录（未传参则探测 NSIS perMachine 默认位置）----
if (-not $InstallDir) {
    $candidates = @(
        "$env:ProgramFiles\CorpMail",
        "${env:ProgramFiles(x86)}\CorpMail",
        "D:\CorpMail", "C:\CorpMail"
    )
    $InstallDir = $candidates | Where-Object { Test-Path (Join-Path $_ "CorpMail.exe") } | Select-Object -First 1
    if (-not $InstallDir) {
        Write-Error "[msix] 未找到 CorpMail.exe。先装 CorpMail_${Version}_x64-setup.exe，或传 -InstallDir。探测过：$($candidates -join ' | ')"
    }
}
Write-Host "[msix] 来源（安装目录）：$InstallDir"

# ---- winapp 定位 ----
if ($WinApp) {
    $winapp = (Resolve-Path $WinApp).Path
} else {
    $cmd = Get-Command winapp -ErrorAction SilentlyContinue
    if (-not $cmd) { Write-Error "[msix] 未找到 WinApp CLI（winget install Microsoft.winappcli --source winget；standalone 安装则以 -WinApp 指定 winapp.exe 路径）" }
    $winapp = $cmd.Source
}

# ---- 复制：manifest + 图标 + exe/dll 到同一目录 ----
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ("corpmail-msix-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $Stage | Out-Null
try {
    Copy-Item (Join-Path $ManifestDir "AppxManifest.xml") $Stage
    New-Item -ItemType Directory -Path (Join-Path $Stage "assets") | Out-Null
    Copy-Item (Join-Path $AssetsDir "*.png") (Join-Path $Stage "assets")
    Copy-Item "$InstallDir\*" $Stage -Recurse -Force
    # 剔除卸载器与安装日志（不属于应用负载）
    @("Uninstall.exe", "uninstall.exe", "install.log") | ForEach-Object {
        $p = Join-Path $Stage $_
        if (Test-Path $p) { Remove-Item $p -Force }
    }

    # ---- 版本注入 <Identity>（四段制末段 0）----
    # 分组正则只锚定 <Identity> 的 Version；裸 'Version="[0-9.]+"' 会把
    # MinVersion="10.0.17763.0" 一起误替换成应用版本。校验恰好 1 处。
    $manifestPath = Join-Path $Stage "AppxManifest.xml"
    $identPattern = '(<Identity\b[^>]*?\bVersion=")[0-9.]+(")'
    $manifestText = Get-Content $manifestPath -Raw -Encoding UTF8
    if ([regex]::Matches($manifestText, $identPattern).Count -ne 1) {
        Write-Error "[msix] manifest 中 <Identity> Version 匹配数异常（应为 1）"
    }
    $manifestText -replace $identPattern, ('${1}' + $Version + '.0' + '${2}') |
        Set-Content $manifestPath -Encoding UTF8 -NoNewline

    # ---- winapp pack（未签名：商店认证后微软代签，上传签名一律被替换）----
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    $msix = Join-Path $OutDir "CorpMail_$Version.0_x64.msix"
    & $winapp pack $Stage --output $msix --no-sign
    if ($LASTEXITCODE -ne 0) { Write-Error "[msix] winapp pack 失败（exit $LASTEXITCODE）" }

    # ---- 输出 ----
    Write-Host ""
    Write-Host "[msix] 产物：$msix" -ForegroundColor Green
    Write-Host ("[msix] 大小：{0} MB" -f [math]::Round((Get-Item $msix).Length / 1MB, 2))
    Write-Host "[msix] 下一步：上传 Partner Center（商店认证后微软代签）"
}
finally {
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
}
