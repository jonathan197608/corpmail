#!/bin/sh
# 创建本地代码签名证书（每台机器一次）
#
# 为什么需要：macOS 钥匙串条目的 ACL 按「签名身份」校验。未签名的二进制走
# ad-hoc 签名，其 designated requirement 就是 cdhash —— 每次重新链接都变，
# ACL 必然失配，于是每次构建后读密码都会弹「想要使用您存储在钥匙串中的机密
# 信息」，点「始终允许」也只对当次 cdhash 生效。换成固定证书后 DR 变成
# identifier + certificate leaf = H"..."，跨构建恒定，授权一次长期有效。
#
# 证书故意**不加入信任链**（不调 add-trusted-cert）：
#   - 本地签名与运行不需要受信任，DR 匹配也与信任状态无关（实测有效）
#   - 标成信任根会让这把私钥签的任何东西都被本机信任，收益为零风险非零
#   - 代价仅是 Gatekeeper 不认，分发场景请改用 Developer ID 证书并公证
#
# 用法：scripts/setup-macos-signing.sh
# 幂等：已存在同名身份则直接退出
set -eu

IDENTITY="${CORPMAIL_SIGN_IDENTITY:-CorpMail Local}"
KEYCHAIN="${CORPMAIL_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

if security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "\"$IDENTITY\""; then
    echo "✓ 签名身份已存在：$IDENTITY"
    exit 0
fi

command -v openssl >/dev/null 2>&1 || {
    echo "缺少 openssl，无法生成证书" >&2
    exit 1
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
P12PASS="corpmail"

openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
    -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
    -subj "/CN=$IDENTITY" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null

# macOS 的 security import 只接受 SHA1 MAC + 3DES 的 PKCS#12。
# OpenSSL 3+ 默认用 AES-256-CBC / SHA-256 导出，导入会报
# "MAC verification failed during PKCS12 import"——必须 -legacy 并显式指定算法。
openssl pkcs12 -export -legacy \
    -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -out "$WORK/cert.p12" \
    -passout "pass:$P12PASS" -name "$IDENTITY" \
    -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1 2>/dev/null

# -T 把 codesign 写进私钥 ACL，否则每次签名都会弹「security 想要使用钥匙串中的密钥」
security import "$WORK/cert.p12" -k "$KEYCHAIN" -P "$P12PASS" \
    -T /usr/bin/codesign -T /usr/bin/security

echo
echo "✓ 已创建签名身份：$IDENTITY"
security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null | grep "\"$IDENTITY\"" || true
echo
echo "开发启动用 scripts/dev-macos.sh（构建后自动重签）。"
echo "打包时 tauri.conf.json 的 bundle.macOS.signingIdentity 已指向该名称，"
echo "需要临时换证书时用 APPLE_SIGNING_IDENTITY 覆盖。"
