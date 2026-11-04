#!/bin/sh
# CorpMail macOS 功能自动化测试入口（GUI E2E）
#
# 为什么需要单独的构建：内嵌 WebDriver 服务端由 Cargo feature `e2e` 编入
# （src-tauri/Cargo.toml），生产二进制默认不含该插件与监听端口。
#
# 为什么必须重签：钥匙串条目 ACL 绑定签名身份。ad-hoc 签名的 designated
# requirement 是 cdhash，每次链接都变 —— 自动化会在第一次读密码时弹授权框卡住。
# 用固定身份（scripts/setup-macos-signing.sh 生成）重签后 DR 稳定。
#
# 用法：e2e/run.sh [wdio 参数，如 --spec specs/01-wizard.spec.mjs]
set -eu

cd "$(dirname "$0")"
ROOT=$(cd .. && pwd)
BIN="$ROOT/src-tauri/target/debug/corpmail"
IDENTITY="${CORPMAIL_SIGN_IDENTITY:-CorpMail Local}"

echo "== 1/4 构建 E2E 二进制（cargo build --features e2e） =="
(cd "$ROOT/src-tauri" && cargo build --features e2e)

echo "== 2/4 固定身份重签 =="
if security find-identity -p codesigning 2>/dev/null | grep -q "\"$IDENTITY\""; then
    codesign --force --sign "$IDENTITY" --identifier com.corpmail.desktop "$BIN"
    echo "   ✓ 已签名：$IDENTITY"
else
    echo "   ! 未找到签名身份「$IDENTITY」，跳过重签"
    echo "     首次读钥匙串可能弹授权框，自动化会卡住；先跑 scripts/setup-macos-signing.sh"
fi

echo "== 3/4 清理残留实例（单实例插件会把新实例合并到旧进程） =="
pkill -f "target/debug/corpmail" 2>/dev/null || true

echo "== 4/4 运行 WebdriverIO =="
# 以关键包是否落地判断依赖完整性（只判目录会在上次中断安装后误判为「已装好」）
if [ ! -f node_modules/@wdio/tauri-service/package.json ]; then
    npm install --no-audit --no-fund
fi
./node_modules/.bin/wdio run ./wdio.conf.mjs "$@"
