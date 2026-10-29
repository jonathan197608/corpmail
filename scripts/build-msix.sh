#!/bin/bash
# ============================================================
# NanoMail · MSIX 商店包构建（Microsoft Store 渠道）
# 宿主：Windows（需 Windows SDK 的 makeappx；WACK 校验同机）
# 前置：scripts/build-windows.sh 已产出 NSIS 安装器，且已在
#       本机安装一次（取回完整安装目录）
# 产物：store-assets/msix/out/NanoMail_x.y.z.0_x64.msix（未签名，
#       商店认证后由微软代签）
# 用法：scripts/build-msix.sh [安装目录] （默认 D:\Mailbox）
# ============================================================
set -euo pipefail

INSTALL_DIR="${1:-D:\\Mailbox}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST_DIR="${ROOT}/store-assets/msix"
OUT_DIR="${MANIFEST_DIR}/out"
VERSION="$(grep -m1 '^version' "${ROOT}/src-tauri/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"

echo "[msix] 版本 ${VERSION}.0，源目录 ${INSTALL_DIR}"

# 1) 布包：清单 + 图标 + 安装目录内容
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp "${MANIFEST_DIR}/AppxManifest.xml" "${STAGE}/"
mkdir -p "${STAGE}/assets"
cp "${MANIFEST_DIR}"/assets/*.png "${STAGE}/assets/"
# 安装目录内容（Mailbox.exe / WebView2Loader.dll 等）；排除卸载器
cp -r "${INSTALL_DIR}/." "${STAGE}/"
rm -f "${STAGE}/Uninstall.exe" "${STAGE}/uninstall.exe" 2>/dev/null || true

# 2) 版本号注入清单（四段制末段 0）
sed -i "s/Version=\"[0-9.]*\"/Version=\"${VERSION}.0\"/" "${STAGE}/AppxManifest.xml"

# 3) MakeAppx（Windows SDK）
MAKEAPPX="${MAKEAPPX:-$(ls 'C:\Program Files (x86)\Windows Kits\10\bin'/*/x64/makeappx.exe 2>/dev/null | sort -V | tail -1 || true)}"
if [ -z "${MAKEAPPX}" ]; then
    echo "[msix] 未找到 makeappx.exe，请装 Windows SDK 或以 MAKEAPPX=... 指定" >&2
    exit 1
fi
mkdir -p "${OUT_DIR}"
"${MAKEAPPX}" pack /d "${STAGE}" /p "${OUT_DIR}\\NanoMail_${VERSION}.0_x64.msix" /o

# 4) WACK（Windows 应用认证工具包）提示
echo
echo "[msix] 产物：${OUT_DIR}/NanoMail_${VERSION}.0_x64.msix"
echo "提交前请跑 WACK 校验（Partner Center 拒收清单错误）："
echo "  \"C:\Program Files (x86)\Windows Kits\10\App Certification Kit\appcert.exe\" reset"
echo "  appcert.exe test -apptype desktop -appxpath \"$(cygpath -w "${OUT_DIR}")\\\\NanoMail_${VERSION}.0_x64.msix\""
echo "确认项：virtualization 禁用生效、Publisher 与 Partner Center 一致、WACK 零 fail"
