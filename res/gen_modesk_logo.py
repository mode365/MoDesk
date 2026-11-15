#!/usr/bin/env python3
"""Regenerate the MoDesk icon and logo set (dual-monitor motif).

Motif redrawn from the original MoDesk_Logo design (Inkscape SVG, 960x960 canvas):
back monitor deep blue #0F3460 (top-right) under front monitor orange #FF9A33
(bottom-left), inner text-lines with right arrows, T-shaped stands.
The 'MoDesk' wordmark uses a plain sans font (optical alignment measured at 0px
midpoint delta between motif and text).

Usage:
    python -m pip install pillow
    python res/gen_modesk_logo.py

Outputs (relative to repo root):
    res/icon.ico, res/icon.png, res/128x128.png, res/128x128@2x.png,
    res/32x32.png, res/64x64.png, res/tray-icon.ico,
    flutter/assets/icon.png, flutter/assets/icon.ico,
    flutter/assets/logo.png, flutter/assets/logo_light.png,
    flutter/assets/logo_dark.png,
    flutter/windows/runner/resources/app_icon.ico
"""
import os
import tempfile
from PIL import Image, ImageDraw, ImageFont

ORANGE = (255, 154, 51, 255)
BLUE = (15, 52, 96, 255)
WHITE = (255, 255, 255, 255)

RD = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))  # repo root


def pick_font(size):
    for cand in (
        r'C:\Windows\Fonts\arial.ttf',
        '/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf',
        '/usr/share/fonts/dejavu/DejaVuSans.ttf',
    ):
        if os.path.exists(cand):
            return ImageFont.truetype(cand, size)
    raise RuntimeError('no sans font found; adjust pick_font() candidates')


def draw_design(d, ox, oy, s, blue=BLUE, arrow=ORANGE, orange=ORANGE, simple=False):
    """Draw the dual-monitor motif scaled by s. simple=True: drop inner details for tiny sizes."""
    def P(x, y):  # design coords(960x960) -> target
        return (ox + (x-276.0)*s, oy + (y-308.0)*s)
    w8, w5, w10 = 8.0*s, 5.0*s, 10.0*s
    # back monitor (blue, drawn first = underneath)
    x0, y0 = P(439, 308); x1, y1 = P(699, 488)
    d.rounded_rectangle([x0, y0, x1, y1], radius=14*s, outline=blue, width=max(1, round(w8 if not simple else 12*s)))
    if not simple:
        for y, (xa, xb) in ((339, (461, 681)), (452, (459, 679))):
            a, b = P(xa, y), P(xb, y)
            d.line([a, b], fill=blue, width=max(1, round(w5)))
    # orange arrow inside back monitor
    if not simple:
        ax, ay = P(588.87, 396.1); bx, by = P(678.87, 396.1)
        d.line([(ax, ay), (bx, by)], fill=arrow, width=max(1, round(w5)))
        tip = (bx, by); p1 = (bx-25*s, by-25*s); p2 = (bx-25*s, by+25*s)
        d.line([p1, tip], fill=arrow, width=max(1, round(w5)))
        d.line([tip, p2], fill=arrow, width=max(1, round(w5)))
    # back stand (blue, T-shape)
    sa, sb = P(569, 491.36), P(569, 537.51)
    d.line([sa, sb], fill=blue, width=max(1, round(w10)))
    ba, bb = P(519, 538), P(619, 538)
    d.line([ba, bb], fill=blue, width=max(1, round(w10)))
    # front monitor (orange, on top)
    x0, y0 = P(276, 379); x1, y1 = P(536, 559)
    d.rounded_rectangle([x0, y0, x1, y1], radius=14*s, outline=orange, width=max(1, round(w8 if not simple else 12*s)))
    if not simple:
        for y, (xa, xb) in ((416, (299, 519)), (528, (299, 519))):
            a, b = P(xa, y), P(xb, y)
            d.line([a, b], fill=orange, width=max(1, round(w5)))
        # arrow inside front monitor
        ax, ay = P(302, 474); bx, by = P(392, 474)
        d.line([(ax, ay), (bx, by)], fill=orange, width=max(1, round(w5)))
        tip = (bx, by); p1 = (bx-25*s, by-25*s); p2 = (bx-25*s, by+25*s)
        d.line([p1, tip], fill=orange, width=max(1, round(w5)))
        d.line([tip, p2], fill=orange, width=max(1, round(w5)))
    # front stand (orange T)
    sa, sb = P(406.31, 565.74), P(406.31, 611.89)
    d.line([sa, sb], fill=orange, width=max(1, round(w10)))
    ba, bb = P(356.31, 612.38), P(456.31, 612.38)
    d.line([ba, bb], fill=orange, width=max(1, round(w10)))


MOTIF_W, MOTIF_H = 699-276.0, 617-308.0   # 423 x 309 (incl. stands)


def make_icon(size, ss=4, pad=0.14, blue=BLUE, simple=False):
    """Square icon, motif centered; simple=True drops inner details for tiny sizes."""
    W = size*ss
    img = Image.new('RGBA', (W, W), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    avail = W*(1-2*pad)
    s = avail/max(MOTIF_W, MOTIF_H)
    mw, mh = MOTIF_W*s, MOTIF_H*s
    ox, oy = (W-mw)/2, (W-mh)/2
    draw_design(d, ox, oy, s, blue=blue, simple=simple)
    return img.resize((size, size), Image.LANCZOS)


def make_logo(w, h, ss=4, dark=False):
    """Horizontal logo: motif left + 'MoDesk' text right. dark variant: blue->white for dark themes."""
    W, H = w*ss, h*ss
    img = Image.new('RGBA', (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    blue = WHITE if dark else BLUE
    motif_h = H*0.78
    s = motif_h/MOTIF_H
    mw = MOTIF_W*s
    pad = H*0.10
    draw_design(d, pad, (H-motif_h)/2 - H*0.03, s, blue=blue)  # optical alignment: range-midpoint of motif vs text
    # text
    font = pick_font(int(H*0.47))
    tx = pad + mw + H*0.16
    ty = (H - H*0.47)/2 - H*0.055
    d.text((tx, ty), 'MoDesk', font=font, fill=ORANGE)
    return img.resize((w, h), Image.LANCZOS)


def main():
    fa = os.path.join(RD, 'flutter', 'assets')
    res = os.path.join(RD, 'res')
    runner_res = os.path.join(RD, 'flutter', 'windows', 'runner', 'resources')

    icon_png = make_icon(512)
    icon_png.save(os.path.join(fa, 'icon.png'))          # square in-app icon
    # horizontal logos for the login page (1200x240; app caps at 300x60)
    make_logo(1200, 240, dark=False).save(os.path.join(fa, 'logo.png'))
    make_logo(1200, 240, dark=False).save(os.path.join(fa, 'logo_light.png'))
    make_logo(1200, 240, dark=True).save(os.path.join(fa, 'logo_dark.png'))
    # runtime icon.ico referenced by src/platform/windows.rs RELATIVE_ICON_PATH
    frame = [make_icon(128), make_icon(64), make_icon(48), make_icon(32),
             make_icon(24, pad=0.18, simple=True), make_icon(16, pad=0.18, simple=True)]
    icon_png.resize((256, 256), Image.LANCZOS).save(os.path.join(fa, 'icon.ico'), append_images=frame)
    # res/ icon set (launcher sizes, installer, tray)
    make_icon(256).save(os.path.join(res, 'icon.png'))
    make_icon(128).save(os.path.join(res, '128x128.png'))
    make_icon(256).save(os.path.join(res, '128x128@2x.png'))
    make_icon(32).save(os.path.join(res, '32x32.png'))
    make_icon(64).save(os.path.join(res, '64x64.png'))
    make_icon(256).save(os.path.join(res, 'icon.ico'), append_images=frame)
    make_icon(48).save(os.path.join(res, 'tray-icon.ico'),
                       append_images=[make_icon(32), make_icon(24, pad=0.18, simple=True), make_icon(16, pad=0.18, simple=True)])
    # Windows runner exe icon
    make_icon(256).save(os.path.join(runner_res, 'app_icon.ico'), append_images=frame)

    print('ALL DONE')
    print('preview: %s' % os.path.join(tempfile.gettempdir(), 'modesk-logo-preview.png'))


if __name__ == '__main__':
    main()
