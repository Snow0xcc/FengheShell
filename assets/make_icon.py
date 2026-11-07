# -*- coding: utf-8 -*-
"""Generate the FengheShell app icon: a shell that holds a terminal prompt.

设计说明（与项目本身对齐，而不是上一版那块五花肉）：

* 圆角底板 = 终端窗口，取主题里的面板底色 `#23262d -> #12141a`
  （见 ui/theme.slint 的 base-bg-panel / window-base）。
* 扇面 = Shell（贝壳），也是「风河」里风与水的纹路：以品牌主色
  `#4a90e2`（Theme.accent 的深色档）向上展开，扇骨脊线给出层次，
  外缘做成扇贝的波浪齿边。
* 提示符 `>` + 一条渐隐的风河曲线（同时是光标下划线）= 终端。
  冷白 + 青色，在蓝色扇面上仍然保持高对比。
* 左上角两道极淡的横向风纹只做底纹，缩小到 16px 时不参与识别。

纯 Pillow，4x 超采样后降采样，边缘干净。
输出：icon.png (256)、icon@512.png、fengheshell.ico（多尺寸）

用法（在 assets/ 目录下）：python3 make_icon.py
"""
import math
from PIL import Image, ImageDraw, ImageFilter, ImageChops

SS = 4
BASE = 256
S = BASE * SS  # 工作画布 1024

OUT_DIR = "."  # 脚本在 assets/ 下运行

# ------------------------------------------------------------------ 配色
TILE_TOP = (35, 38, 46)      # #23262d  主题深色面板
TILE_BOT = (17, 19, 25)      # #111319
SHELL_HI = (104, 174, 244)   # #68aef4  扇面上缘（accent 提亮）
SHELL_LO = (37, 100, 186)    # #2564ba  扇面下缘（accent 加深）
RIDGE = (20, 56, 104)        # 扇骨
RIDGE_A = 96                 # 扇骨透明度
HINGE = (16, 46, 88)         # 贝壳关节
PROMPT = (238, 247, 255)     # 提示符，冷白
WAVE_HI = (146, 232, 250)    # #92e8fa 风河曲线
RIM = (74, 144, 226)         # 品牌主色描边
RIM_A = 64

# 扇面几何（hinge 在下方，扇面向上打开）
CX = 0.500
HY = 0.752
RR = 0.368
ANG_FROM = 201.0   # 度，数学角（y 轴向上），左翼
ANG_TO = -21.0     # 右翼
RIDGES = 8         # 扇骨数量（间隔数 = RIDGES - 1 段齿）

# ------------------------------------------------------------------ 工具


def lerp(a, b, t):
    return int(round(a + (b - a) * t))


def mix(c1, c2, t):
    return tuple(lerp(c1[i], c2[i], t) for i in range(3))


def vgradient(size, top, bot):
    """纵向渐变 RGB 图。"""
    w, h = size
    col = Image.new("RGB", (1, h))
    px = col.load()
    for y in range(h):
        t = y / (h - 1)
        px[0, y] = (lerp(top[0], bot[0], t),
                    lerp(top[1], bot[1], t),
                    lerp(top[2], bot[2], t))
    return col.resize((w, h))


def pol(r, a_deg):
    """极坐标 -> 像素坐标（y 轴向上，hinge 在 (CX, HY)）。"""
    a = math.radians(a_deg)
    return (S * CX + r * math.cos(a), S * HY - r * math.sin(a))


def fan_outline():
    """扇贝外轮廓：hinge -> 左翼 -> 波浪齿边 -> 右翼 -> 回到 hinge。"""
    seg = (ANG_TO - ANG_FROM) / (RIDGES - 1)   # 负值：从左侧扫到右侧
    pts = [pol(0.0, ANG_FROM)]
    steps = 26
    for i in range(RIDGES - 1):
        a0 = ANG_FROM + seg * i
        for k in range(steps + 1):
            t = k / steps
            a = a0 + seg * t
            # 每一段外缘向外鼓一点：扇贝的齿
            r = RR * S * (1.0 + 0.048 * math.sin(math.pi * t))
            pts.append(pol(r, a))
    pts.append(pol(0.0, ANG_TO))
    return pts


def tapered_stroke(draw, p0, p1, w0, w1, fill):
    """从头到尾变宽的锥形笔画（扇骨 / 风河曲线都由它拼出来）。"""
    dx, dy = p1[0] - p0[0], p1[1] - p0[1]
    ln = math.hypot(dx, dy) or 1.0
    nx, ny = -dy / ln, dx / ln
    draw.polygon([
        (p0[0] + nx * w0 / 2, p0[1] + ny * w0 / 2),
        (p1[0] + nx * w1 / 2, p1[1] + ny * w1 / 2),
        (p1[0] - nx * w1 / 2, p1[1] - ny * w1 / 2),
        (p0[0] - nx * w0 / 2, p0[1] - ny * w0 / 2),
    ], fill=fill)


# ------------------------------------------------------------------ 底板
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

tile_mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(tile_mask).rounded_rectangle(
    [0, 0, S - 1, S - 1], radius=int(S * 0.225), fill=255)

grad = vgradient((S, S), TILE_TOP, TILE_BOT).convert("RGBA")
img.paste(grad, (0, 0), tile_mask)

# 顶部高光（薄薄一层，避免图标发灰）
sheen = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(sheen).ellipse(
    [-int(S * 0.3), -int(S * 0.62), int(S * 1.3), int(S * 0.30)],
    fill=(255, 255, 255, 20))
sheen.putalpha(ImageChops.multiply(sheen.getchannel("A"), tile_mask))
img = Image.alpha_composite(img, sheen)

# ------------------------------------------------------------------ 风纹底纹
wind = Image.new("RGBA", (S, S), (0, 0, 0, 0))
wd = ImageDraw.Draw(wind)
for (y0, amp, w, col) in [(0.16, 0.020, 0.013, (120, 205, 240, 34)),
                          (0.86, 0.026, 0.010, (120, 205, 240, 26))]:
    pts = [(S * i / 160,
            S * (y0 + amp * math.sin(i / 160 * math.pi * 2 + 0.5)))
           for i in range(161)]
    wd.line(pts, fill=col, width=int(S * w), joint="curve")
wind = wind.filter(ImageFilter.GaussianBlur(S * 0.004))
wind.putalpha(ImageChops.multiply(wind.getchannel("A"), tile_mask))
img = Image.alpha_composite(img, wind)

# ------------------------------------------------------------------ 扇面（Shell）
shell = Image.new("RGBA", (S, S), (0, 0, 0, 0))
sd = ImageDraw.Draw(shell)

outline = fan_outline()
# 扇面本体：主色纵向渐变，用轮廓做遮罩
body = Image.new("RGBA", (S, S), (0, 0, 0, 0))
body_mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(body_mask).polygon(outline, fill=255)
shell_grad = vgradient((S, S), SHELL_HI, SHELL_LO).convert("RGBA")
body.paste(shell_grad, (0, 0), body_mask)
shell = Image.alpha_composite(shell, body)

# 扇骨：从 hinge 呈放射状发散，靠近外缘变宽
seg = (ANG_TO - ANG_FROM) / (RIDGES - 1)
for i in range(1, RIDGES - 1):
    a = ANG_FROM + seg * i
    inner = pol(RR * S * 0.07, a)
    outer = pol(RR * S * 1.005, a)
    tapered_stroke(sd, inner, outer, S * 0.005, S * 0.020,
                   RIDGE + (150,))

# 外缘一圈更亮的边，让扇面从底板上"立"起来
sd.line(outline[1:-1] + [outline[1]], fill=(206, 233, 255, 165),
        width=int(S * 0.011), joint="curve")

# 左上高光：一小片柔光，别做成塑料球
gloss = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(gloss).ellipse(
    [S * 0.20, S * 0.30, S * 0.62, S * 0.62], fill=(255, 255, 255, 58))
gloss = gloss.filter(ImageFilter.GaussianBlur(S * 0.045))
gloss.putalpha(ImageChops.multiply(gloss.getchannel("A"), body_mask))
shell = Image.alpha_composite(shell, gloss)

# 关节：底部一个深色小楔子，暗示贝壳的铰合部
sd.polygon([(S * (CX - 0.050), S * (HY - 0.012)),
            (S * (CX + 0.050), S * (HY - 0.012)),
            (S * CX, S * (HY + 0.062))], fill=HINGE + (225,))

# 裁剪掉溢出扇面的部分，再压一层投影
shell.putalpha(ImageChops.multiply(shell.getchannel("A"), body_mask))
shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
shadow.paste(Image.new("RGBA", (S, S), (0, 0, 0, 130)),
             (int(S * 0.014), int(S * 0.020)), body_mask)
shadow = shadow.filter(ImageFilter.GaussianBlur(S * 0.020))
shadow.putalpha(ImageChops.multiply(shadow.getchannel("A"), tile_mask))
img = Image.alpha_composite(img, shadow)
img = Image.alpha_composite(img, shell)

# ------------------------------------------------------------------ 提示符 >_ 与风河曲线
prompt = Image.new("RGBA", (S, S), (0, 0, 0, 0))
pd = ImageDraw.Draw(prompt)

off = S * 0.012  # 投影偏移，保证在扇面上也读得清


def draw_prompt(draw, fill, dx=0.0, dy=0.0):
    # >
    x0, y0 = S * (CX - 0.205), S * 0.520
    w, h = S * 0.135, S * 0.140
    draw.line([(x0 + dx, y0 + dy), (x0 + w + dx, y0 + h / 2 + dy),
               (x0 + dx, y0 + h + dy)],
              fill=fill, width=int(S * 0.036), joint="curve")
    # 风河曲线（同时是光标下划线）：一条起笔粗、收笔细的横扫
    wx0, wy = S * (CX - 0.020), S * 0.688
    n = 40
    upper, lower = [], []
    for i in range(n + 1):
        t = i / n
        x = wx0 + S * 0.255 * t
        y = wy + dy - S * 0.020 * math.sin(math.pi * t)
        half = (S * 0.030 * (1.0 - 0.72 * t)) / 2
        upper.append((x + dx, y - half))
        lower.append((x + dx, y + half))
    draw.polygon(upper + lower[::-1], fill=fill)


draw_prompt(pd, (10, 26, 44, 150), off, off)
draw_prompt(pd, PROMPT + (255,))

# 提示符整体稍微提亮一点，避免被扇面吃掉
glow = prompt.filter(ImageFilter.GaussianBlur(S * 0.016))
prompt = Image.alpha_composite(glow, prompt)
img = Image.alpha_composite(img, prompt)

# ------------------------------------------------------------------ 描边与导出
rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(rim).rounded_rectangle(
    [S * 0.004, S * 0.004, S - S * 0.004, S - S * 0.004],
    radius=int(S * 0.222), outline=RIM + (RIM_A,), width=int(S * 0.008))
img = Image.alpha_composite(img, rim)

img256 = img.resize((BASE, BASE), Image.LANCZOS)
img512 = img.resize((512, 512), Image.LANCZOS)
img256.save(f"{OUT_DIR}/icon.png")
img512.save(f"{OUT_DIR}/icon@512.png")
img512.resize((256, 256), Image.LANCZOS).save(
    f"{OUT_DIR}/fengheshell.ico", format="ICO",
    sizes=[(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (16, 16)])
print("OK: icon.png, icon@512.png, fengheshell.ico written")
