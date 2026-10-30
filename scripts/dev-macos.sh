#!/bin/sh
# macOS 开发启动：构建 → 用固定身份重签 → 运行
#
# 为什么必须重签：tauri dev 不做签名，arm64 链接器会自动给产物打 ad-hoc 签名，
# 其 designated requirement 是 cdhash，每次重新链接都变。钥匙串条目 ACL 按 DR
# 校验，于是每次构建后首次读密码都会弹授权窗口。重签后 DR 变成
# identifier + certificate leaf = H"..."，保持不变，授权一次长期有效。
#
# 注：cargo 只按依赖 mtime 判断是否重建，重签（改产物 mtime）不会引发多余编译。
#
# 用法：scripts/dev-macos.sh [cargo build 附加参数]
set -eu

cd "$(dirname "$0")/../src-tauri"

IDENTITY="${CORPMAIL_SIGN_IDENTITY:-CorpMail Local}"
BIN="target/debug/corpmail"

if ! security find-identity -p codesigning 2>/dev/null | grep -q "\"$IDENTITY\""; then
    echo "找不到签名身份「$IDENTITY」" >&2
    echo "先执行 scripts/setup-macos-signing.sh 创建" >&2
    exit 1
fi

cargo build "$@"

codesign --force --sign "$IDENTITY" \
    --identifier com.corpmail.desktop \
    "$BIN"

# DR 里出现 cdhash 说明重签没生效（比如被后续链接覆盖），此时弹窗问题依旧存在
if codesign -d -r- "$BIN" 2>&1 | grep -q "certificate leaf"; then
    echo "✓ 已签名：$IDENTITY"
else
    echo "✗ 签名未生效，DR 仍为 cdhash" >&2
    codesign -d -r- "$BIN" 2>&1 | tail -1 >&2
    exit 1
fi

exec "$BIN"
