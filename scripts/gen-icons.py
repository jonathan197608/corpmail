#!/usr/bin/env python3
"""生成 CorpMail 全平台图标（Windows 多尺寸 ICO / PNG / MSIX 资产）。

背景（2026-11-09）：Windows 上图标模糊、各场合视觉宽度不一致。根因：
  - icon.ico 只有单层 32×32，任务栏/Alt-Tab/开始菜单在 125%/150% DPI 下
    被系统放大（48/64/256 全靠缩放）→ 模糊；
  - icon.png 仅 128×128，MSIX 资产是另一次生成的低分辨率图（44px 文件仅
    268 字节、Wide310x150Logo 实为 620×300）→ 商店/开始菜单再缩放。
本脚本以同一矢量几何在 8× 超采样下重绘并输出所有尺寸，保证各尺寸视觉
宽度一致（蓝色圆角方块 81% 宽、白色信封 2px@64 描边）。

依赖：Pillow（pip install pillow）。用法：python3 scripts/gen-icons.py
"""
import io
import struct
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SS = 8  # 超采样倍数：先大图绘制再 LANCZOS 缩放，小尺寸边缘更干净

BLUE = (37, 99, 235, 255)      # #2563EB
WHITE = (239, 246, 255, 255)   # #EFF6FF

# 以下坐标均为 64×64 设计稿坐标系
SQUARE = (6.0, 10.0, 57.5, 53.5, 7.0)          # 蓝色圆角方块（x0,y0,x1,y1,r）
BODY = (16.0, 24.0, 49.5, 43.5, 2.5)           # 白色信封外框
FLAP = [(16.0, 24.0), (32.8, 36.0), (49.5, 24.0)]  # 信封盖 V
STROKE = 2.0                                    # 64 坐标系线宽


def draw_icon(size: int) -> Image.Image:
    """按 64 坐标系绘制正方形图标（白底透明）。"""
    k = SS
    n = int(round(size * k * 64 / size))  # 统一以 64 坐标系放大
    n = 64 * k
    im = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.rounded_rectangle(
        [SQUARE[0] * k, SQUARE[1] * k, SQUARE[2] * k, SQUARE[3] * k],
        radius=SQUARE[4] * k,
        fill=BLUE,
    )
    d.rounded_rectangle(
        [BODY[0] * k, BODY[1] * k, BODY[2] * k, BODY[3] * k],
        radius=BODY[4] * k,
        outline=WHITE,
        width=int(STROKE * k),
    )
    d.line([(x * k, y * k) for x, y in FLAP], fill=WHITE,
           width=int(STROKE * k), joint="curve")
    # 端点补圆帽（PIL 的 line 无 round cap）
    r = STROKE * k / 2
    for x, y in (FLAP[0], FLAP[-1]):
        d.ellipse([x * k - r, y * k - r, x * k + r, y * k + r], fill=WHITE)
    return im.resize((size, size), Image.LANCZOS)


def wide_logo(w: int, h: int) -> Image.Image:
    """宽幅磁贴（Wide310x150）：蓝底满幅 + 等比居中信封。"""
    k = SS
    im = Image.new("RGBA", (w * k, h * k), BLUE)
    art = draw_icon(64)  # 取 64 见方的完整图案（含蓝底）再缩放居中
    target = int(h * 0.62)
    art = art.resize((target, target), Image.LANCZOS)
    im.paste(art, ((w * k - target) // 2, (h * k - target) // 2), art)
    return im.resize((w, h), Image.LANCZOS)


def write_ico(path: Path, frames: list[tuple[int, Image.Image]]) -> None:
    """多尺寸 ICO（Vista+ 允许 PNG 压缩条目）：每层用独立超采样帧，避免
    让系统/库自行缩放导致小尺寸糊边。"""
    blobs = []
    for size, im in frames:
        buf = io.BytesIO()
        im.save(buf, "PNG")
        blobs.append((size, buf.getvalue()))
    n = len(blobs)
    offset = 6 + 16 * n
    entries = b""
    data = b""
    for size, blob in blobs:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        data += blob
        offset += len(blob)
    path.write_bytes(struct.pack("<HHH", 0, 1, n) + entries + data)


def save_png(img: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    img.save(path, "PNG", optimize=True)
    print(f"  {path.relative_to(ROOT)}  {img.width}x{img.height}")


def main() -> None:
    icons = ROOT / "src-tauri/icons"
    master = draw_icon(1024)
    save_png(master, icons / "icon.png")
    for name, size in {"32x32.png": 32, "128x128.png": 128, "128x128@2x.png": 256}.items():
        save_png(draw_icon(size), icons / name)

    # Windows ICO：覆盖系统会用到的全部尺寸（16/20/24/32/40/48/64/128/256）
    ico_sizes = [(16, 16), (20, 20), (24, 24), (32, 32), (40, 40),
                 (48, 48), (64, 64), (128, 128), (256, 256)]
    write_ico(icons / "icon.ico", [(s, draw_icon(s)) for s, _ in ico_sizes])
    print(f"  src-tauri/icons/icon.ico  {', '.join(str(s) for s, _ in ico_sizes)}")

    # MSIX / 微软商店资产：按清单引用尺寸精确输出，避免系统二次缩放
    msix = ROOT / "store-assets/msix/assets"
    for name, size in {
        "Square44x44Logo.png": 44,
        "Square50x50Logo.png": 50,
        "Square150x150Logo.png": 150,
        "Square150x150Logo.scale-200.png": 300,
        "Square310x310Logo.png": 310,
        "Square620x620Logo.png": 620,
        "StoreLogo.png": 50,
    }.items():
        save_png(draw_icon(size), msix / name)
    save_png(wide_logo(310, 150), msix / "Wide310x150Logo.png")
    save_png(wide_logo(620, 300), msix / "Wide310x150Logo.scale-200.png")


if __name__ == "__main__":
    main()