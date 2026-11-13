#!/bin/sh
# CorpMail macOS 功能自动化测试入口（GUI E2E，三阶段）
#
# 为什么需要单独的构建：内嵌 WebDriver 服务端由 Cargo feature `e2e` 编入
# （src-tauri/Cargo.toml），生产二进制默认不含该插件与监听端口。
#
# 为什么必须重签：钥匙串条目 ACL 绑定签名身份。ad-hoc 签名的 designated
# requirement 是 cdhash，每次链接都变 —— 自动化会在第一次读密码时弹授权框卡住。
# 用固定身份（scripts/setup-macos-signing.sh 生成）重签后 DR 稳定。
#
# 三阶段（前两阶段为 2026-11-10 的两阶段设计，A3 于 2026-11-13 加第三阶段）：
#   阶段一 主链路：specs/*.spec.mjs（00 冒烟 → 07 导出 → 90 失败 → 95 迁移），
#          单 app 进程贯穿全程（服务在 onPrepare 启动一次）。
#   阶段二 复用链路：主链路退出后由 support/prepare-reuse.mjs 生成「重装」
#          状态（toml 丢失 + 锚点库清除 + 最终数据复制到 reuse-data/），
#          再起第二个 wdio 实例只跑 specs/phase2/08-reuse-adopt.spec.mjs——
#          向导指向既有数据目录的账号一致性/直接沿用（重装找回历史邮件）。
#          阶段二需要 app 真正重启（读盘上的 corpmail.toml），无法在
#          阶段一的同一进程内实现，这是独立调用的根本原因。
#   阶段三 生产后台迁移链（A3）：specs/phase3/09-migration-background.spec.mjs。
#          复用阶段二跑完向导的 HOME（CORPMAIL_E2E_HOME），产物另放 <RUN_ID>-bg；
#          命令前缀给 CORPMAIL_E2E_BACKGROUND=1 在 e2e 二进制里注入生产同构的
#          后台 executor，CORPMAIL_E2E_NO_EXIT=1 保留进程供 wdio 断言事件与
#          落盘。前两阶段不设这两个变量，仍走同步迁移分支（95/90 一字不改）。
#
# 用法：e2e/run.sh [wdio 参数，如 --spec specs/01-wizard.spec.mjs]
#       （带 --spec 单点调试时主链路不会生成 reuse marker；此时必须显式
#         E2E_SKIP_PHASE2=1 与 E2E_SKIP_PHASE3=1，否则按失败退出——静默跳过
#         已改为 fail-closed）
#
# 环境变量：
#   E2E_SKIP_PHASE2=1    单点调试时显式允许跳过阶段二
#   E2E_SKIP_PHASE3=1    单点调试时显式允许跳过阶段三（需阶段二终态）
#   E2E_ALLOW_UNSIGNED=1 没有固定签名身份时仍继续（本地默认 fail-closed）
#   CORPMAIL_SIGN_IDENTITY 覆盖签名身份名（默认 CorpMail Local）
#   CORPMAIL_E2E_*        由本脚本内部为第三阶段设置，不要手动注入主链路
set -eu

cd "$(dirname "$0")"
ROOT=$(cd .. && pwd)
BIN="$ROOT/src-tauri/target/debug/corpmail"
IDENTITY="${CORPMAIL_SIGN_IDENTITY:-CorpMail Local}"

# 停掉本仓库构建的 app 实例（审计 R85）。
# 旧实现 `pkill -f "target/debug/corpmail"` 是子串匹配：一旦有别的进程命令行里
# 带这段路径（编辑器打开文件、`less` 某份日志、tail 某次 stderr 重定向），会被
# 一起杀掉。这里先按完整路径取候选 PID，再用 `ps -o comm=` 复核可执行文件确实
# 是 $BIN 才发信号；SIGTERM 后最多等 10s，仍未退出则 SIGKILL。
still_running() {
    for pid in $(pgrep -f "$BIN" 2>/dev/null || true); do
        [ "$(ps -p "$pid" -o comm= 2>/dev/null)" = "$BIN" ] && return 0
    done
    return 1
}

stop_app() {
    still_running || return 0
    for pid in $(pgrep -f "$BIN" 2>/dev/null || true); do
        if [ "$(ps -p "$pid" -o comm= 2>/dev/null)" = "$BIN" ]; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    i=0
    while [ "$i" -lt 50 ]; do
        still_running || return 0
        sleep 0.2
        i=$((i + 1))
    done
    echo "   ! app 未在 10s 内退出，改发 SIGKILL（可能丢最后一次 checkpoint）" >&2
    for pid in $(pgrep -f "$BIN" 2>/dev/null || true); do
        if [ "$(ps -p "$pid" -o comm= 2>/dev/null)" = "$BIN" ]; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    sleep 0.5
}

# 等 WAL 落盘：SQLite 在最后一个连接关闭时 checkpoint 并删除 -wal。
# 固定 `sleep 1` 在慢机器上不够（VACUUM 后库大、checkpoint 更久），改为轮询到
# wal 消失或为空；超时只告警不失败——prepare-reuse 会把 wal 一并复制，副本仍可用
wait_wal_quiet() {
    # 必须写成 ${1}：`$1-wal` 在 POSIX sh 里会被解析成 ${1-wal}（带默认值的展开）
    wal="${1}-wal"
    i=0
    while [ "$i" -lt 60 ]; do
        if [ ! -f "$wal" ]; then return 0; fi
        if [ "$(wc -c <"$wal" | tr -d ' ')" = "0" ]; then return 0; fi
        sleep 0.5
        i=$((i + 1))
    done
    # 变量名后紧跟全角标点时必须写 ${wal}：不加大括号会连同「，」一起当成变量名
    echo "   ! WAL 30s 内未清空（${wal}），继续执行" >&2
    return 0
}

# C19/C14/C15/A10：前端状态机与请求编排的**行为**测试（假时钟 / 假 DOM /
# 假 htmx）。Rust 侧只有 APP_JS.contains 静态护栏，改坏看门狗三元阈值、把
# 确认标志的 delete 挪回放行分支、删掉刷新 URL 的搜索词拼接、把草稿 flush
# 挪到迁移 POST 之后都能全绿；这里用 Node vm 直接执行 app.js 里带锚点的
# core 段，不启动 app、不依赖 wdio 与签名。
echo "== 0/7 前端行为测试（node --test unit/） =="
node --test unit/migration-watchdog.test.mjs unit/compose-confirm.test.mjs \
    unit/list-refresh.test.mjs unit/migration-draft-flush.test.mjs

echo "== 1/7 构建 E2E 二进制（cargo build --features e2e） =="
(cd "$ROOT/src-tauri" && cargo build --features e2e)

echo "== 2/7 固定身份重签 =="
if security find-identity -p codesigning 2>/dev/null | grep -q "\"$IDENTITY\""; then
    codesign --force --sign "$IDENTITY" --identifier com.corpmail.desktop "$BIN"
    echo "   ✓ 已签名：$IDENTITY"
elif [ "${E2E_ALLOW_UNSIGNED:-0}" = "1" ]; then
    # 无固定身份时。未签名跑得通但不稳：ad-hoc 签名的 ACL 绑定 cdhash，每次
    # 链接都变，首次读钥匙串可能弹授权框把自动化卡死（审计 R85 要求默认 fail-closed）
    echo "   ! 未找到签名身份「${IDENTITY}」，按 E2E_ALLOW_UNSIGNED=1 继续（可能弹钥匙串授权）"
else
    echo "   ✗ 未找到签名身份「${IDENTITY}」" >&2
    echo "     先跑 scripts/setup-macos-signing.sh 建本地证书；" >&2
    echo "     确需未签名运行请显式设置 E2E_ALLOW_UNSIGNED=1" >&2
    exit 1
fi

echo "== 3/7 清理残留实例（单实例插件会把新实例合并到旧进程） =="
stop_app

echo "== 4/7 依赖检查 =="
# 以关键包是否落地判断依赖完整性（只判目录会在上次中断安装后误判为「已装好」）
if [ ! -f node_modules/@wdio/tauri-service/package.json ]; then
    npm install --no-audit --no-fund
fi

# 前两阶段共用同一 RUN_DIR（同一隔离 HOME 与产物目录），shot 清单合并；
# A3 第三阶段产物目录换成 <RUN_ID>-bg，HOME 仍复用前两阶段的已配置终态。
RUN_ID="${E2E_RUN_ID:-$(date +%Y-%m-%dT%H-%M-%S)}"
export E2E_RUN_ID="$RUN_ID"
RUN_DIR="artifacts/$RUN_ID"
ANCHOR_DB="$RUN_DIR/home/Library/Application Support/com.corpmail.desktop/corpmail.db"

echo "== 5/7 阶段一：主链路（00-07 + 90 + 95）run=$RUN_ID =="
MAIN_STATUS=0
./node_modules/.bin/wdio run ./wdio.conf.mjs "$@" || MAIN_STATUS=$?

# 阶段二前置：app 必须退出且事务落盘后再复制库（WAL 运行中复制会拿到撕裂副本）
stop_app
wait_wal_quiet "$ANCHOR_DB"

# 阶段二 fail-closed（审计 R66）：marker 缺失说明主链路没跑出「已配置 + 有库」的
# 终态，旧实现只 echo 跳过并保持退出码 0——整套 E2E 表面全绿，实际少测了重装
# 复用链路。现在只有显式 E2E_SKIP_PHASE2=1（单点调试某个 spec 时）才允许跳过
REUSE_STATUS=0
if node support/prepare-reuse.mjs "$RUN_DIR"; then
    if [ -f "$RUN_DIR/reuse-ready.json" ]; then
        echo "== 阶段二：重装复用链路（08-reuse-adopt） =="
        ./node_modules/.bin/wdio run ./wdio.conf.mjs \
            --spec ./specs/phase2/08-reuse-adopt.spec.mjs || REUSE_STATUS=$?
    elif [ "${E2E_SKIP_PHASE2:-0}" = "1" ]; then
        echo "   阶段二按 E2E_SKIP_PHASE2=1 显式跳过（主链路未生成 reuse marker）"
    else
        echo "   ✗ 阶段二缺少 reuse marker：主链路未产生「已配置 + 有库」终态" >&2
        echo "     若确实只跑单点 spec，请显式设置 E2E_SKIP_PHASE2=1" >&2
        REUSE_STATUS=1
    fi
else
    REUSE_STATUS=$?
fi

# ---- 阶段三（A3）：生产后台迁移链 ----------------------------------------
# 两个 CORPMAIL_E2E_* 开关只在 `--features e2e` 构建里读（lib.rs 的 e2e_flag
# 带 cfg!(feature="e2e") 守卫，生产二进制恒 false）。只在下面这条命令的
# 前缀上传，主链路与阶段二完全不接触它们，仍走同步迁移分支。
BG_STATUS=0
BG_SOURCE_DATA_DIR=""
if [ "${E2E_SKIP_PHASE3:-0}" = "1" ]; then
    echo "== 6/7 阶段三按 E2E_SKIP_PHASE3=1 显式跳过 =="
elif [ ! -f "$RUN_DIR/reuse-ready.json" ]; then
    echo "   ✗ 阶段三缺少 reuse-ready.json：没有「已配置 + 有库」前置状态" >&2
    BG_STATUS=1
else
    BG_SOURCE_DATA_DIR=$(node -e '
        const fs = require("fs");
        const m = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
        if (!m.dataDir || !fs.existsSync(m.dataDir)) process.exit(1);
        process.stdout.write(m.dataDir);
    ' "$RUN_DIR/reuse-ready.json") || BG_SOURCE_DATA_DIR=""
    if [ -z "$BG_SOURCE_DATA_DIR" ]; then
        echo "   ✗ 阶段三前置数据目录不存在（reuse-ready.json 的 dataDir）" >&2
        BG_STATUS=1
    fi
fi

if [ "$BG_STATUS" -eq 0 ] && [ "${E2E_SKIP_PHASE3:-0}" != "1" ]; then
    stop_app
    wait_wal_quiet "$BG_SOURCE_DATA_DIR/corpmail.db"
    echo "== 6/7 阶段三：生产后台迁移链（09） run=${RUN_ID}-bg =="
    CORPMAIL_E2E_BACKGROUND=1 \
    CORPMAIL_E2E_NO_EXIT=1 \
    CORPMAIL_E2E_HOME="$ROOT/e2e/artifacts/$RUN_ID/home" \
    CORPMAIL_E2E_SOURCE_DATA_DIR="$BG_SOURCE_DATA_DIR" \
    E2E_RUN_ID="${RUN_ID}-bg" \
        ./node_modules/.bin/wdio run ./wdio.conf.mjs \
            --spec ./specs/phase3/09-migration-background.spec.mjs || BG_STATUS=$?
    # NO_EXIT 让 app 留在后台（冻结位已置），清掉再收尾，避免干扰下次运行
    stop_app
fi

echo "== 7/7 汇总：主链路=$MAIN_STATUS 阶段二=$REUSE_STATUS 阶段三=$BG_STATUS =="
# 任一阶段失败都让 run.sh 以非零退出（CI 语义）
if [ "$MAIN_STATUS" -ne 0 ]; then exit "$MAIN_STATUS"; fi
if [ "$REUSE_STATUS" -ne 0 ]; then exit "$REUSE_STATUS"; fi
if [ "$BG_STATUS" -ne 0 ]; then exit "$BG_STATUS"; fi
exit 0
