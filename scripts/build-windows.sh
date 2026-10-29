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

# 0) 本机 patched NSIS 工具链（PATH 前置）。
#    brew nsis 3.13 在 macOS 上必崩：libiconv 的 "wchar_t" 通道拒绝 NSIS 内部标记串中的
#    U+0002（EILSEQ）→ makensis 处理任意脚本即 bad_alloc；中文 .nlf 再触发 "Bad text
#    encoding"（详见设计文档 §11.4）。
#    自建 patched 工具链位于 .temp/nsis-toolchain：bin/makensis 是 wrapper 脚本（注入
#    NSISDIR），bin/makensis.bin 才是真实二进制。
#    注意：tauri-bundler 调 makensis 时会 .env_remove("NSISDIR")，导出该变量无效，
#    数据目录定位必须由 wrapper 在进程内完成。
NSIS_BIN="${ROOT}/.temp/nsis-toolchain/bin"
if [ -x "${NSIS_BIN}/makensis" ]; then
    export PATH="${NSIS_BIN}:${PATH}"
    echo "[build-windows] 使用自建 patched NSIS：${NSIS_BIN}/makensis（wrapper → makensis.bin）"
else
    echo "[build-windows] 警告：未找到自建 patched NSIS，将回退系统 makensis（brew 3.13 已知必崩）" >&2
fi

# 1) 预置 WebView2 Bootstrapper 打包缓存。
#    embedBootstrapper 模式下 bundler 需从微软 fwlink 下载（内网拦截 go.microsoft.com），
#    但其下载逻辑为"缓存文件存在即跳过联网"——预置官方已验签副本即可离线打包。
CACHE_DIR="${HOME}/Library/Caches/tauri"
mkdir -p "${CACHE_DIR}"
if [ ! -f "${CACHE_DIR}/MicrosoftEdgeWebview2Setup.exe" ]; then
    cp webview2/MicrosoftEdgeWebview2Setup.exe "${CACHE_DIR}/"
    echo "[build-windows] 已预置 Bootstrapper 打包缓存：${CACHE_DIR}/MicrosoftEdgeWebview2Setup.exe"
fi

# 2) 交叉编译 + NSIS 打包（单命令两步内聚：cargo build → bundler）
#    bundle.targets 经 --config 内联指定：CLI 的 --bundles 参数按宿主平台条件编译枚举
#    （macOS 无 nsis 变体）必被拒，而 config targets 走平台无关 BundleType、bundler 按
#    target triple 调度——这是官方交叉打包的通道。
#    gnu 目标下 tauri-build 自动拷 WebView2Loader.dll 到产物目录，bundler 自动装入安装包。
#
#    SignPath 云签名（设计 §11.2）：检测到凭据才注入 bundle.windows.signCommand，
#    使签名仅发生在正式发版构建（本地无凭据自动跳过，无需改配置）。
#    签名点共 4 类：主程序 exe / NSIS 插件 dll / 内嵌卸载器（NSIS 编译期钩子）/ 最终安装器。
if [ -n "${SIGNPATH_API_TOKEN:-}" ] && [ -n "${SIGNPATH_ORGANIZATION_ID:-}" ]; then
    SIGN_SCRIPT="${ROOT}/scripts/sign-signpath.sh"
    [ -x "${SIGN_SCRIPT}" ] || chmod +x "${SIGN_SCRIPT}"
    echo "[build-windows] SignPath 签名已启用：${SIGN_SCRIPT}（project=${SIGNPATH_PROJECT_SLUG:-?} policy=${SIGNPATH_POLICY_SLUG:-?}）"
    CONFIG_JSON=$(printf '{"bundle":{"targets":["nsis"],"windows":{"signCommand":{"cmd":"%s","args":["%%1"]}}}}' "${SIGN_SCRIPT}")
else
    echo "[build-windows] 未检测到 SIGNPATH_API_TOKEN/SIGNPATH_ORGANIZATION_ID，本构建不签名（正式发版凭据见 scripts/sign-signpath.sh 头注释）"
    CONFIG_JSON='{"bundle":{"targets":["nsis"]}}'
fi

cargo tauri build --target x86_64-pc-windows-gnu \
    --config "$CONFIG_JSON" "$@"

# 3) 产物清单
echo
echo "[build-windows] 安装器产物："
ls -la target/x86_64-pc-windows-gnu/release/bundle/nsis/
