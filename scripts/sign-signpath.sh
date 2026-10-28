#!/bin/bash
# ============================================================
# Mailbox · Windows 产物签名（SignPath 云签名）
# 被两个调用方使用（均传待签文件路径为 $1，要求原地替换为已签名版本）：
#   1) tauri-bundler sign_command：主程序 exe / NSIS 插件 dll / 最终安装器
#   2) NSIS !uninstfinalize 钩子（makensis 编译期，cwd 可能不同）：
#      内嵌卸载器临时 exe
#
# 接入方式（配置在 scripts/build-windows.sh，检测到凭据环境变量时注入）：
#   bundle.windows.signCommand = { cmd: <本脚本>, args: ["%1"] }
#
# 必需环境变量：
#   SIGNPATH_API_TOKEN        SignPath 用户/CI 用户的 API Token
#   SIGNPATH_ORGANIZATION_ID  组织 ID（GUID，签名策略详情页可见）
#   SIGNPATH_PROJECT_SLUG     项目 slug
#   SIGNPATH_POLICY_SLUG      签名策略 slug
# 可选环境变量：
#   SIGNPATH_API_URL          默认 https://app.signpath.io（自建/欧盟节点时覆盖）
#   SIGNPATH_ARTIFACT_SLUG    artifact configuration slug（默认策略缺省）
#   SIGNPATH_TIMEOUT_SECS     轮询超时（默认 900；云签名含恶意软件扫描，大文件勿设太小）
#   SIGNPATH_DRY_RUN=1        只记录调用不真签（链路冒烟用）
#
# REST API：docs.signpath.io/build-system-integration
#   POST /Api/v1/{org}/SigningRequests/SubmitWithArtifact → 201 + Location
#   GET  /Api/v1/{org}/SigningRequests/{id}/Status → {status,isFinalStatus}
#   GET  /Api/v1/{org}/SigningRequests/{id}/SignedArtifact → 签名后二进制
# ============================================================
set -euo pipefail

LOG_PREFIX="[sign-signpath]"
FILE="${1:-}"

die() { echo "${LOG_PREFIX} 错误：$*" >&2; exit 1; }

# ---------- 前置校验 ----------
[ -n "$FILE" ] || die "未收到待签文件参数（sign_command/UNINSTALLERSIGNCOMMAND 的 %1）"
[ -f "$FILE" ] || die "待签文件不存在：${FILE}"

if [ "${SIGNPATH_DRY_RUN:-0}" = "1" ]; then
    echo "${LOG_PREFIX} [dry-run] 跳过真实签名：${FILE}（$(wc -c <"$FILE" | tr -d ' ') 字节）"
    exit 0
fi

MISSING=""
[ -z "${SIGNPATH_API_TOKEN:-}" ] && MISSING="${MISSING} SIGNPATH_API_TOKEN"
[ -z "${SIGNPATH_ORGANIZATION_ID:-}" ] && MISSING="${MISSING} SIGNPATH_ORGANIZATION_ID"
[ -z "${SIGNPATH_PROJECT_SLUG:-}" ] && MISSING="${MISSING} SIGNPATH_PROJECT_SLUG"
[ -z "${SIGNPATH_POLICY_SLUG:-}" ] && MISSING="${MISSING} SIGNPATH_POLICY_SLUG"
[ -n "$MISSING" ] && die "缺少环境变量：${MISSING}（本地无凭据构建请勿设置 SIGNPATH_API_TOKEN，构建脚本会自动跳过签名）"

API_URL="${SIGNPATH_API_URL:-https://app.signpath.io}"
ORG="${SIGNPATH_ORGANIZATION_ID}"
TIMEOUT_SECS="${SIGNPATH_TIMEOUT_SECS:-900}"
BASE="${API_URL}/Api/v1/${ORG}"

echo "${LOG_PREFIX} 签名 ${FILE}（$(wc -c <"$FILE" | tr -d ' ') 字节）→ ${SIGNPATH_PROJECT_SLUG}/${SIGNPATH_POLICY_SLUG} @ ${API_URL}"

# ---------- 1) 提交签名请求 ----------
HDR="$(mktemp)"
BODY="$(mktemp)"
trap 'rm -f "$HDR" "$BODY" "${FILE}.sp-signed.tmp"' EXIT

HTTP_CODE="$(curl -sS -X POST "${BASE}/SigningRequests/SubmitWithArtifact" \
    -H "Authorization: Bearer ${SIGNPATH_API_TOKEN}" \
    -F "projectSlug=${SIGNPATH_PROJECT_SLUG}" \
    -F "signingPolicySlug=${SIGNPATH_POLICY_SLUG}" \
    ${SIGNPATH_ARTIFACT_SLUG:+-F "artifactConfigurationSlug=${SIGNPATH_ARTIFACT_SLUG}"} \
    -F "artifact=@${FILE}" \
    -F "description=mailbox $(basename "${FILE}")" \
    -D "$HDR" -o "$BODY" -w '%{http_code}')" || die "提交失败：无法连接 ${API_URL}（检查网络/代理与内网拦截）"

[ "$HTTP_CODE" = "201" ] || die "提交失败 HTTP ${HTTP_CODE}：$(tr -d '\r' <"$BODY" | head -c 400)"

# Location 头形如 .../SigningRequests/{guid}，取末段为请求 ID
REQUEST_ID="$(tr -d '\r' <"$HDR" | awk 'tolower($1)=="location:"{print $2}' | sed 's:.*/::')"
[ -n "$REQUEST_ID" ] || die "响应缺少 Location 头（无法定位 signingRequestId）"
echo "${LOG_PREFIX} 已提交：signingRequestId=${REQUEST_ID}"

# ---------- 2) 轮询状态 ----------
DEADLINE=$(( $(date +%s) + TIMEOUT_SECS ))
while :; do
    sleep 3
    HTTP_CODE="$(curl -sS -H "Authorization: Bearer ${SIGNPATH_API_TOKEN}" \
        -o "$BODY" -w '%{http_code}' "${BASE}/SigningRequests/${REQUEST_ID}/Status")" \
        || die "状态查询失败（网络中断）"
    [ "$HTTP_CODE" = "200" ] || die "状态查询失败 HTTP ${HTTP_CODE}"

    STATUS_JSON="$(cat "$BODY")"
    STATUS="$(printf '%s' "$STATUS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"

    if [ "$STATUS" = "Completed" ]; then
        break
    elif [ "$STATUS" = "InProgress" ] || [ "$STATUS" = "WaitingForApproval" ]; then
        [ "$(date +%s)" -lt "$DEADLINE" ] || die "签名超时（>${TIMEOUT_SECS}s，最后状态 ${STATUS}）"
    else
        die "签名终态异常：${STATUS}（详情：${STATUS_JSON}）"
    fi
done

# ---------- 3) 下载签名产物并原地替换 ----------
HTTP_CODE="$(curl -sS -H "Authorization: Bearer ${SIGNPATH_API_TOKEN}" \
    -o "${FILE}.sp-signed.tmp" -w '%{http_code}' \
    "${BASE}/SigningRequests/${REQUEST_ID}/SignedArtifact")" \
    || die "下载签名产物失败（网络中断）"
[ "$HTTP_CODE" = "200" ] || die "下载签名产物失败 HTTP ${HTTP_CODE}"
[ -s "${FILE}.sp-signed.tmp" ] || die "签名产物为空文件"

# 外置卷纪律：cat 覆写 + cmp 校验（cp 半写文件会被 arm64 代码签名机制 SIGKILL）
cat "${FILE}.sp-signed.tmp" > "$FILE"
cmp -s "${FILE}.sp-signed.tmp" "$FILE" || die "原地替换校验失败（cmp 不一致）：${FILE}"

echo "${LOG_PREFIX} 完成：${FILE} 已签名（$(wc -c <"$FILE" | tr -d ' ') 字节，requestId=${REQUEST_ID}）"
