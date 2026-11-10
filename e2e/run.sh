#!/bin/sh
# CorpMail macOS 功能自动化测试入口（GUI E2E，两阶段）
#
# 为什么需要单独的构建：内嵌 WebDriver 服务端由 Cargo feature `e2e` 编入
# （src-tauri/Cargo.toml），生产二进制默认不含该插件与监听端口。
#
# 为什么必须重签：钥匙串条目 ACL 绑定签名身份。ad-hoc 签名的 designated
# requirement 是 cdhash，每次链接都变 —— 自动化会在第一次读密码时弹授权框卡住。
# 用固定身份（scripts/setup-macos-signing.sh 生成）重签后 DR 稳定。
#
# 两阶段（2026-11-10）：
#   阶段一 主链路：specs/*.spec.mjs（00 冒烟 → 07 导出 → 90 失败链路），
#          单 app 进程贯穿全程（服务在 onPrepare 启动一次）。
#   阶段二 复用链路：主链路退出后由 support/prepare-reuse.mjs 生成「重装」
#          状态（toml 丢失 + 锚点库清除 + 最终数据复制到 reuse-data/），
#          再起第二个 wdio 实例只跑 specs/phase2/08-reuse-adopt.spec.mjs——
#          向导指向既有数据目录的账号一致性/直接沿用（重装找回历史邮件）。
#          阶段二需要 app 真正重启（读盘上的 corpmail.toml），无法在
#          阶段一的同一进程内实现，这是独立调用的根本原因。
#
# 用法：e2e/run.sh [wdio 参数，如 --spec specs/01-wizard.spec.mjs]
#       （带 --spec 单点调试时，主链路通常不会生成 reuse marker，阶段二自动跳过）
set -eu

cd "$(dirname "$0")"
ROOT=$(cd .. && pwd)
BIN="$ROOT/src-tauri/target/debug/corpmail"
IDENTITY="${CORPMAIL_SIGN_IDENTITY:-CorpMail Local}"

echo "== 1/5 构建 E2E 二进制（cargo build --features e2e） =="
(cd "$ROOT/src-tauri" && cargo build --features e2e)

echo "== 2/5 固定身份重签 =="
if security find-identity -p codesigning 2>/dev/null | grep -q "\"$IDENTITY\""; then
    codesign --force --sign "$IDENTITY" --identifier com.corpmail.desktop "$BIN"
    echo "   ✓ 已签名：$IDENTITY"
else
    echo "   ! 未找到签名身份「$IDENTITY」，跳过重签"
    echo "     首次读钥匙串可能弹授权框，自动化会卡住；先跑 scripts/setup-macos-signing.sh"
fi

echo "== 3/5 清理残留实例（单实例插件会把新实例合并到旧进程） =="
pkill -f "target/debug/corpmail" 2>/dev/null || true

echo "== 4/5 依赖检查 =="
# 以关键包是否落地判断依赖完整性（只判目录会在上次中断安装后误判为「已装好」）
if [ ! -f node_modules/@wdio/tauri-service/package.json ]; then
    npm install --no-audit --no-fund
fi

# 两个阶段共用同一 RUN_DIR（同一隔离 HOME 与产物目录），shot 清单合并
RUN_ID="${E2E_RUN_ID:-$(date +%Y-%m-%dT%H-%M-%S)}"
export E2E_RUN_ID="$RUN_ID"
RUN_DIR="artifacts/$RUN_ID"

echo "== 5/5 阶段一：主链路（00-07 + 90）run=$RUN_ID =="
MAIN_STATUS=0
./node_modules/.bin/wdio run ./wdio.conf.mjs "$@" || MAIN_STATUS=$?

# 阶段二前置：app 必须退出后再复制库（WAL 运行中复制会拿到撕裂副本）
pkill -f "target/debug/corpmail" 2>/dev/null || true
sleep 1

REUSE_STATUS=0
if node support/prepare-reuse.mjs "$RUN_DIR" && [ -f "$RUN_DIR/reuse-ready.json" ]; then
    echo "== 阶段二：重装复用链路（08-reuse-adopt） =="
    ./node_modules/.bin/wdio run ./wdio.conf.mjs \
        --spec ./specs/phase2/08-reuse-adopt.spec.mjs || REUSE_STATUS=$?
else
    echo "   阶段二跳过（主链路未生成 reuse marker：可能未跑到向导/迁移）"
fi

# 任一阶段失败都让 run.sh 以非零退出（CI 语义）
if [ "$MAIN_STATUS" -ne 0 ]; then exit "$MAIN_STATUS"; fi
if [ "$REUSE_STATUS" -ne 0 ]; then exit "$REUSE_STATUS"; fi
exit 0
