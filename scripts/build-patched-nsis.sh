#!/bin/bash
# ============================================================
# CorpMail · 自建 patched NSIS 工具链（macOS 宿主）
# ============================================================
# brew nsis 3.13 在 macOS 上必崩：libiconv 的 "wchar_t" 转换通道对 C0 控制符/CJK/BOM
# 返回 EILSEQ → makensis 处理任意脚本即 bad_alloc → SIGABRT（详见 docs-private/设计文档 §11.4）。
# 本脚本从 NSIS 3.13 源码构建 patched 版本，安装到用户级持久目录（不入 .temp/），
# 供 scripts/build-windows.sh 使用。
#
# 产物：~/.local/share/corpmail/nsis-toolchain/
#   bin/makensis          wrapper 脚本（进程内注入 NSISDIR → makensis.bin）
#   bin/makensis.bin      patched 真实二进制
#   share/nsis/           数据目录（Stubs/Include/Plugins，复用 brew 发行数据）
#
# patch：util.h 新增 nsis_iconv_wename()（宿主字节序 UTF-32LE/BE），
#    util.cpp/utf.cpp 共 10 处 iconv 编码名 "wchar_t" → nsis_iconv_wename()。
# 幂等：工具链已存在且可运行则跳过（--force 强制重建）。
# 用法：scripts/build-patched-nsis.sh [--force]
# ============================================================
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${HOME}/.local/share/corpmail/nsis-toolchain"
BIN="${TARGET}/bin"

NSIS_VERSION="3.13"
# SourceForge 302 重定向下载
SRC_URL="https://downloads.sourceforge.net/project/nsis/NSIS%203/${NSIS_VERSION}/nsis-${NSIS_VERSION}-src.tar.bz2"
# 源码包 sha256（2026-11-11 取自上述 URL 的实测值）。编译产物进签名安装包，
# 下载必须校验：不校验等于把供应链交给 TLS 之外的一切中间环节。
# 升级 NSIS_VERSION 时必须同步更新此值。
NSIS_SHA256="a8ffe024602d46b6d766f9e1ce30c324ad2a24daeacd3efc2642d436a0c157ac"

if [ "${1:-}" = "--force" ]; then
    # 只删构建产物，不整目录 rm -rf：TARGET 可能与用户数据/软链共享父目录，
    # 递归删除越界代价过高。数据目录（share/nsis）由第 5 步覆盖写入。
    echo "[patched-nsis] --force：删除现有 makensis 产物后重建"
    rm -f "${BIN}/makensis" "${BIN}/makensis.bin"
fi
if [ -x "${BIN}/makensis" ] && [ -x "${BIN}/makensis.bin" ]; then
    echo "[patched-nsis] 工具链已存在且可用：${BIN}/makensis（跳过；--force 重建）"
    exit 0
fi

echo "[patched-nsis] 目标目录：${TARGET}"
mkdir -p "${BIN}"

# 1) 下载并解压 NSIS 源码到临时工作区
WORK="$(mktemp -d /tmp/nsis-build.XXXXXX)"
trap 'rm -rf "${WORK}"' EXIT
echo "[patched-nsis] 下载 NSIS ${NSIS_VERSION} 源码：${SRC_URL}"
curl -fsSL --max-time 120 -o "${WORK}/nsis-src.tar.bz2" "${SRC_URL}"
ACTUAL_SHA256="$(shasum -a 256 "${WORK}/nsis-src.tar.bz2" | awk '{print $1}')"
if [ "${ACTUAL_SHA256}" != "${NSIS_SHA256}" ]; then
    echo "[patched-nsis] 源码包 sha256 校验失败，已中止" >&2
    echo "  期望：${NSIS_SHA256}" >&2
    echo "  实际：${ACTUAL_SHA256}" >&2
    exit 1
fi
echo "[patched-nsis] sha256 校验通过"
tar xjf "${WORK}/nsis-src.tar.bz2" -C "${WORK}"
SRC="${WORK}/nsis-${NSIS_VERSION}-src"

# 2) 应用 patch（helper + 10 处 iconv 编码名替换）
echo "[patched-nsis] 应用 macOS libiconv 补丁…"
python3 - "${SRC}" << 'PYEOF'
import sys
src = sys.argv[1]
h = open(f'{src}/source/util.h', encoding='utf-8').read()
anchor = 'inline bool Platform_IsBigEndian() { return FIX_ENDIAN_INT16(0x00ff) != 0x00ff; }'
if 'nsis_iconv_wename' not in h:
    assert anchor in h, 'util.h 锚点未找到，NSIS 版本不匹配？'
    helper = anchor + '\ninline const char* nsis_iconv_wename() { return Platform_IsBigEndian() ? "UTF-32BE" : "UTF-32LE"; }'
    open(f'{src}/source/util.h', 'w', encoding='utf-8').write(h.replace(anchor, helper))
for f in ('util.cpp', 'utf.cpp'):
    p = f'{src}/source/{f}'
    s = open(p, encoding='utf-8').read()
    s = s.replace('"wchar_t"', 'nsis_iconv_wename()')
    open(p, 'w', encoding='utf-8').write(s)
PYEOF

# 3) 编译（SKIPUTILS=all 必带：utils 区段 AddZLib 在 SConscript 读取期对任何目标都缺
#    ZLIB_W32 直接 Exit(1)，见设计 §11.4）
echo "[patched-nsis] scons 编译 makensis（SKIPUTILS=all）…"
(cd "${SRC}" && scons SKIPUTILS=all makensis)
MK_BIN="${SRC}/build/urelease/makensis/makensis"
[ -x "${MK_BIN}" ] || { echo "[patched-nsis] 编译产物缺失：${MK_BIN}" >&2; exit 1; }

# 4) 落位工具链（真实二进制用 cat > 并 cmp 校验，避免半写文件被 arm64 签名机制 SIGKILL）
echo "[patched-nsis] 安装 makensis.bin…"
cat "${MK_BIN}" > "${BIN}/makensis.bin"
chmod +x "${BIN}/makensis.bin"
cmp -s "${MK_BIN}" "${BIN}/makensis.bin" || { echo "[patched-nsis] 二进制拷贝校验失败" >&2; exit 1; }

# 5) 数据目录：复用 brew 发行数据（bundler 调 makensis 时 .env_remove(NSISDIR)，
#    由 wrapper 进程内注入；数据目录必须随工具链搬迁）。
#    brew 的 share/nsis 是符号链接（→ Cellar/.../share/nsis），cp -R 会把 symlink
#    原样复制成断链，stub 读不到——必须 -L 解引用复制真实文件。
echo "[patched-nsis] 复制 NSIS 数据目录（复用 brew 发行数据，-L 解引用）…"
NSIS_PREFIX="$(brew --prefix nsis)"
DATA_SRC="${NSIS_PREFIX}/share/nsis"
[ -d "${DATA_SRC}" ] || { echo "[patched-nsis] 未找到 brew nsis 数据目录：${DATA_SRC}" >&2; exit 1; }
mkdir -p "${TARGET}/share"
cp -RL "${DATA_SRC}" "${TARGET}/share/nsis"

# 6) wrapper 脚本：自算 NSISDIR 注入后 exec makensis.bin
#    tauri-bundler 显式 .env_remove("NSISDIR")，外部 export 无效，必须进程内注入
echo "[patched-nsis] 写 wrapper（bin/makensis）…"
cat > "${BIN}/makensis" << 'WRAPPER'
#!/bin/sh
# patched NSIS wrapper：进程内注入数据目录（bundler 会剥掉外部 NSISDIR 环境变量）
self_dir="$(cd "$(dirname "$0")" && pwd)"
export NSISDIR="${self_dir}/../share/nsis"
exec "${self_dir}/makensis.bin" "$@"
WRAPPER
chmod +x "${BIN}/makensis"

# 7) 自检：用 patched makensis 编译含中文 .nlf 的脚本（系统版必崩的场景）
echo "[patched-nsis] 自检：编译含 SimpChinese 的安装脚本…"
PROBE="${WORK}/probe.nsi"
PROBE_OUT="${WORK}/probe.exe"
# 双引号 heredoc：让 ${PROBE_OUT} 展开为绝对路径（单引号 heredoc 会把 ${PROBE_OUT}
# 当 NSIS 变量字面量，未定义 → 报错误判为崩）
cat > "${PROBE}" << EOF
Unicode true
!include "MUI2.nsh"
Name "自建测试"
OutFile "${PROBE_OUT}"
!insertmacro MUI_LANGUAGE "SimpChinese"
Section ""
SectionEnd
EOF
if ! "${BIN}/makensis" "${PROBE}" >/dev/null 2>&1; then
    echo "[patched-nsis] 自检失败：patched makensis 仍崩？" >&2
    # 只清掉不可用的产物（下次运行会重新编译），不整目录删
    rm -f "${BIN}/makensis" "${BIN}/makensis.bin"
    exit 1
fi
echo "[patched-nsis] 自检通过：${BIN}/makensis（可编译中文安装脚本）"
echo "[patched-nsis] 完成：${BIN}/makensis （build-windows.sh 将自动使用）"