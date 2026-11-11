#!/bin/bash
# ============================================================
# CorpMail · Windows 交叉打包（官方 NSIS，设计 §11.2/§11.3 v3.4）
# 宿主：macOS（mingw-w64 交叉编译 + tauri-bundler NSIS）
# 产物：target/x86_64-pc-windows-gnu/release/bundle/nsis/*-setup.exe
# 用法：scripts/build-windows.sh [--debug]
# ============================================================
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "${ROOT}/src-tauri"

# 0) 本机 patched NSIS 工具链（PATH 前置；持久目录，见 build-patched-nsis.sh）。
#    brew nsis 3.13 在 macOS 上必崩：libiconv 的 "wchar_t" 通道拒绝 NSIS 内部标记串中的
#    U+0002（EILSEQ）→ makensis 处理任意脚本即 bad_alloc；中文 .nlf 再触发 "Bad text
#    encoding"。Linux/Windows 宿主没有这个 libiconv 回归，用系统 nsis 即可。
#    patched 工具链位于 ~/.local/share/corpmail/nsis-toolchain：bin/makensis 是 wrapper
#    脚本（进程内注入 NSISDIR），bin/makensis.bin 才是真实二进制。
#    注意：tauri-bundler 调 makensis 时会 .env_remove("NSISDIR")，导出该变量无效，
#    数据目录定位必须由 wrapper 在进程内完成。
NSIS_BIN="${HOME}/.local/share/corpmail/nsis-toolchain/bin"
if [ -x "${NSIS_BIN}/makensis" ] && [ -x "${NSIS_BIN}/makensis.bin" ]; then
    export PATH="${NSIS_BIN}:${PATH}"
    echo "[build-windows] 使用自建 patched NSIS：${NSIS_BIN}/makensis（wrapper → makensis.bin）"
else
    echo "[build-windows] 错误：未找到自建 patched NSIS（${NSIS_BIN}/makensis）" >&2
    echo "  macOS 宿主下系统 brew makensis 3.13 因 libiconv 'wchar_t' 通道 EILSEQ 必崩，" >&2
    echo "  不能回退。请先构建工具链（源码构建约 1 分钟）：" >&2
    echo "    scripts/build-patched-nsis.sh" >&2
    echo "  （Linux CI 宿主无此 bug，直接用 apt install nsis，不依赖本工具链）" >&2
    exit 1
fi

# 1) 预置 WebView2 Bootstrapper 打包缓存。
#    embedBootstrapper 模式下 bundler 需从微软 fwlink 下载（内网拦截 go.microsoft.com），
#    但其下载逻辑为"缓存文件存在即跳过联网"——预置官方已验签副本即可离线打包。
#
#    SHA-256 固定校验（审计 R69）：仓库内这份 exe 会原样进入安装包并在用户机器上
#    以安装器身份执行，被替换等于把自己签不了名的可执行文件塞进安装流程。微软
#    对 evergreen bootstrapper 不发布稳定哈希（每次更新都变），所以这里锁的是
#    "仓库里这一份"的指纹：换新版本必须人工核对来源后更新常量，脚本不做自动升级。
#    来源：https://go.microsoft.com/fwlink/p/?LinkId=2124701（2026-10 下载，1,888,472 字节）
WEBVIEW2_SHA256="ca4c9016c70e2184c5e231662e0e98abf4826647180ef3faab32fcb1ca2ec15e"
WEBVIEW2_SRC="webview2/MicrosoftEdgeWebview2Setup.exe"

sha256_of() {
    shasum -a 256 "$1" | awk '{print $1}'
}

if [ ! -f "${WEBVIEW2_SRC}" ]; then
    echo "[build-windows] 错误：缺少 ${WEBVIEW2_SRC}（embedBootstrapper 打包必需）" >&2
    exit 1
fi
ACTUAL_SHA=$(sha256_of "${WEBVIEW2_SRC}")
if [ "${ACTUAL_SHA}" != "${WEBVIEW2_SHA256}" ]; then
    echo "[build-windows] 错误：WebView2 Bootstrapper 指纹不符" >&2
    echo "  期望 ${WEBVIEW2_SHA256}" >&2
    echo "  实际 ${ACTUAL_SHA}" >&2
    echo "  若确属官方新版本，请人工核对来源后更新脚本内常量（勿跳过校验）" >&2
    exit 1
fi

CACHE_DIR="${HOME}/Library/Caches/tauri"
mkdir -p "${CACHE_DIR}"
if [ ! -f "${CACHE_DIR}/MicrosoftEdgeWebview2Setup.exe" ]; then
    cp "${WEBVIEW2_SRC}" "${CACHE_DIR}/"
    echo "[build-windows] 已预置 Bootstrapper 打包缓存：${CACHE_DIR}/MicrosoftEdgeWebview2Setup.exe"
elif [ "$(sha256_of "${CACHE_DIR}/MicrosoftEdgeWebview2Setup.exe")" != "${WEBVIEW2_SHA256}" ]; then
    # 缓存里是别的版本（bundler 曾自己下过新版）：覆盖成已核验的副本，保证产物可控
    cp "${WEBVIEW2_SRC}" "${CACHE_DIR}/"
    echo "[build-windows] 已用核验副本覆盖缓存中的旧/异 Bootstrapper"
fi

# 2) 质量门禁：clippy 零告警才允许打包（与开发约定一致；无跳过开关，审计 R64）
#    对 Windows 交叉目标整库 + 全部 targets 检查（编译期与 bundler 同目标，保障
#    不引入平台条件编译告警）。clippy 失败即中止（set -e），阻断出包。
echo "[build-windows] 质量门禁：cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings"
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings

# 3) 交叉编译 + NSIS 打包（单命令两步内聚：cargo build → bundler）
#    bundle.targets 经 --config 内联指定：CLI 的 --bundles 参数按宿主平台条件编译枚举
#    （macOS 无 nsis 变体）必被拒，而 config targets 走平台无关 BundleType、bundler 按
#    target triple 调度——这是官方交叉打包的通道。
#    gnu 目标下 tauri-build 自动拷 WebView2Loader.dll 到产物目录，bundler 自动装入安装包。
#
#    产物**不签名**：云签名（SignPath）路线已废弃并移除。商店渠道由 Microsoft 签名，
#    直发 NSIS 安装器为未签名包，首次运行会触发 SmartScreen 提示。
CONFIG_JSON='{"bundle":{"targets":["nsis"]}}'

cargo tauri build --target x86_64-pc-windows-gnu \
    --config "$CONFIG_JSON" "$@"

# 4) 产物清单
echo
echo "[build-windows] 安装器产物："
ls -la target/x86_64-pc-windows-gnu/release/bundle/nsis/
