#!/bin/sh
# macOS 原生外壳验证（WebDriver 覆盖不到的那一层）：
#   窗口存在性 / 关窗隐藏到托盘 / 进程常驻 / 窗口级截图
#
# 为什么单列一层：Tauri 的 WebDriver 只能驱动 WebView 内部 DOM，
# 托盘、Cmd+W 隐藏、通知、系统对话框都在 WKWebView 之外。
# 这层用 AppleScript（System Events 的 Accessibility API）+ screencapture 补。
#
# 前置权限（首次会弹窗，点允许即可）：
#   系统设置 → 隐私与安全性 → 辅助功能    （控制 System Events）
#   系统设置 → 隐私与安全性 → 屏幕录制    （screencapture 抓窗口）
#
# 用法：scripts/e2e-shell-macos.sh [输出目录]
set -eu

OUT="${1:-$(cd "$(dirname "$0")/.." && pwd)/e2e/artifacts/shell-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"
APP_BIN="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/target/debug/corpmail"
PROC="corpmail"

say() { printf '%s\n' "$*"; }
fail() { printf '✗ %s\n' "$*" >&2; exit 1; }

ax() {
    osascript -e "tell application \"System Events\" to tell process \"$PROC\" to $1" 2>/dev/null
}

# ---- 1. 确保应用在运行 ------------------------------------------------
if ! pgrep -f "target/debug/corpmail" >/dev/null 2>&1; then
    say "启动 $APP_BIN"
    "$APP_BIN" >"$OUT/app.log" 2>&1 &
    sleep 3
fi

# ---- 2. 等待主窗口 ----------------------------------------------------
i=0
while [ $i -lt 20 ]; do
    if [ "$(ax 'count of windows')" = "1" ] 2>/dev/null; then break; fi
    i=$((i + 1)); sleep 1
done
[ "$(ax 'count of windows')" = "1" ] || fail "20s 内未出现主窗口（辅助功能权限未授予？）"

TITLE=$(ax 'get title of window 1')
[ "$TITLE" = "CorpMail" ] || fail "窗口标题异常：$TITLE"
say "✓ 主窗口存在，标题 $TITLE"

# ---- 3. 窗口级截图 ----------------------------------------------------
GEOM=$(ax 'get {position of window 1, size of window 1}' | tr -d ' ')
# AppleScript 返回 "10, 25, 1440, 900" → 去掉空格后是 "10,25,1440,900"
RECT=$(printf '%s' "$GEOM" | sed -n 's/.*{\([0-9-]*\), *\([0-9-]*\)}, *{\([0-9-]*\), *\([0-9-]*\)}.*/\1,\2,\3,\4/p')
[ -n "$RECT" ] || RECT=$(printf '%s' "$GEOM" | tr -d ' ')
screencapture -x -R"$RECT" "$OUT/01-main-window.png" || fail "screencapture 失败（屏幕录制权限？）"
say "✓ 窗口截图：$OUT/01-main-window.png"

# ---- 4. 关窗应隐藏而非退出（托盘常驻） --------------------------------
osascript -e "tell application \"System Events\" to keystroke \"w\" using command down" 2>/dev/null || true
sleep 2
WINDOWS_AFTER=$(ax 'count of windows' || echo 0)
[ "$WINDOWS_AFTER" = "0" ] || fail "Cmd+W 后窗口仍存在（count=$WINDOWS_AFTER）"
pgrep -f "target/debug/corpmail" >/dev/null || fail "Cmd+W 后进程退出（应隐藏到托盘）"
say "✓ 关窗后进程常驻、窗口已隐藏"
screencapture -x "$OUT/02-after-close-desktop.png" 2>/dev/null || true

say ""
say "外壳层断言通过。截图目录：$OUT"
