#!/usr/bin/env python3
"""Generate a 1024x1024 RGBA8 application icon PNG for Glyph3D Wall.

Uses only the Python standard library (struct, zlib, math, argparse).
- With `--source-png PATH` (an offscreen render of `--wall-test-pattern`), crops a
  square detail region from the top-left of the primary terminal + canvas paper and
  frames it inside a macOS-proportioned rounded squircle with a mint-green accent rim.
- Without `--source-png` (e.g. headless hosts with no GPU), procedurally renders a
  legible dark-grid terminal-canvas icon at the requested size.

To replace with hand-crafted artwork, place a 1024x1024 RGBA PNG at
`assets/icon/glyph3d-wall.png` or pass `--icon PATH` to `tools/wall-bundle.sh`.
"""

from __future__ import annotations

import argparse
import struct
import zlib
from pathlib import Path


def paeth(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def read_rgba_png(path: Path) -> tuple[int, int, bytearray]:
    """Decode an 8-bit non-interlaced RGB or RGBA PNG using stdlib zlib."""
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG file")
    pos = 8
    width = height = 0
    bit_depth = color_type = interlace = 0
    idat = bytearray()
    while pos + 8 <= len(data):
        length, ctype = struct.unpack(">I4s", data[pos : pos + 8])
        chunk = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if ctype == b"IHDR":
            width, height, bit_depth, color_type, _, _, interlace = struct.unpack(">IIBBBBB", chunk)
        elif ctype == b"IDAT":
            idat.extend(chunk)
        elif ctype == b"IEND":
            break
    if bit_depth != 8 or interlace != 0 or color_type not in (2, 6):
        raise ValueError(
            f"{path}: unsupported PNG format (depth={bit_depth}, color_type={color_type}, interlace={interlace})"
        )
    bpp = 4 if color_type == 6 else 3
    stride = width * bpp
    raw = zlib.decompress(bytes(idat))
    out = bytearray(width * height * 4)
    prev = bytearray(stride)
    cur = bytearray(stride)
    at = 0
    for y in range(height):
        filt = raw[at]
        row = raw[at + 1 : at + 1 + stride]
        at += 1 + stride
        if filt == 0:
            cur[:] = row
        elif filt == 1:
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                cur[i] = (row[i] + left) & 0xFF
        elif filt == 2:
            for i in range(stride):
                cur[i] = (row[i] + prev[i]) & 0xFF
        elif filt == 3:
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                cur[i] = (row[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif filt == 4:
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                up = prev[i]
                up_left = prev[i - bpp] if i >= bpp else 0
                cur[i] = (row[i] + paeth(left, up, up_left)) & 0xFF
        else:
            raise ValueError(f"{path}: unknown PNG filter {filt}")
        prev[:] = cur
        dst_base = y * width * 4
        if bpp == 4:
            out[dst_base : dst_base + width * 4] = cur
        else:
            for x in range(width):
                si = x * 3
                di = dst_base + x * 4
                out[di] = cur[si]
                out[di + 1] = cur[si + 1]
                out[di + 2] = cur[si + 2]
                out[di + 3] = 255
    return width, height, out


def write_rgba_png(path: Path, width: int, height: int, rgba: bytes | bytearray) -> None:
    def chunk(tag: bytes, body: bytes) -> bytes:
        crc = zlib.crc32(tag)
        crc = zlib.crc32(body, crc) & 0xFFFFFFFF
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", crc)

    stride = width * 4
    raw = bytearray((stride + 1) * height)
    for y in range(height):
        ro = y * (stride + 1)
        so = y * stride
        raw[ro] = 0
        raw[ro + 1 : ro + 1 + stride] = rgba[so : so + stride]

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    compressed = zlib.compress(bytes(raw), level=9)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", compressed) + chunk(b"IEND", b"")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(png)


def rounded_rect_sdf(x: float, y: float, cx: float, cy: float, half_w: float, half_h: float, radius: float) -> float:
    dx = abs(x - cx) - (half_w - radius)
    dy = abs(y - cy) - (half_h - radius)
    ox = max(dx, 0.0)
    oy = max(dy, 0.0)
    outside = (ox * ox + oy * oy) ** 0.5
    inside = min(max(dx, dy), 0.0)
    return outside + inside - radius


def sample_bilinear(src: bytearray, sw: int, sh: int, u: float, v: float) -> tuple[int, int, int]:
    fx = max(0.0, min(sw - 1.001, u * (sw - 1)))
    fy = max(0.0, min(sh - 1.001, v * (sh - 1)))
    x0 = int(fx)
    y0 = int(fy)
    tx = fx - x0
    ty = fy - y0
    i00 = (y0 * sw + x0) * 4
    i10 = i00 + 4
    i01 = i00 + sw * 4
    i11 = i01 + 4
    rgb = []
    for c in range(3):
        top = src[i00 + c] * (1.0 - tx) + src[i10 + c] * tx
        bot = src[i01 + c] * (1.0 - tx) + src[i11 + c] * tx
        rgb.append(int(top * (1.0 - ty) + bot * ty + 0.5))
    return rgb[0], rgb[1], rgb[2]


def crop_interesting_square(src: bytearray, sw: int, sh: int) -> tuple[int, int, bytearray]:
    """Crop a square region anchored on the upper-left focused mint-green terminal frame."""
    mint_x, mint_y = sw, sh
    bg_r, bg_g, bg_b = src[0], src[1], src[2]
    any_x, any_y = sw, sh
    for y in range(0, sh, 2):
        row = y * sw * 4
        for x in range(0, sw // 2, 2):
            i = row + x * 4
            r, g, b = src[i], src[i + 1], src[i + 2]
            if g >= 190 and g > r + 50 and b >= 110:
                if x < mint_x:
                    mint_x = x
                if y < mint_y:
                    mint_y = y
            if abs(r - bg_r) + abs(g - bg_g) + abs(b - bg_b) > 28:
                if x < any_x:
                    any_x = x
                if y < any_y:
                    any_y = y
    side = int(min(sw, sh) * 0.34)
    if mint_x < sw and mint_y < sh:
        pad_x = int(side * 0.14)
        pad_y = int(side * 0.20)
        x0 = max(0, min(sw - side, mint_x - pad_x))
        y0 = max(0, min(sh - side, mint_y - pad_y))
    elif any_x < sw and any_y < sh:
        pad = int(side * 0.11)
        x0 = max(0, min(sw - side, any_x - pad))
        y0 = max(0, min(sh - side, any_y - pad))
    else:
        x0 = (sw - side) // 2
        y0 = (sh - side) // 2
    cropped = bytearray(side * side * 4)
    for y in range(side):
        s_off = ((y0 + y) * sw + x0) * 4
        d_off = y * side * 4
        cropped[d_off : d_off + side * 4] = src[s_off : s_off + side * 4]
    return side, side, cropped


def build_procedural_tile(tile_size: int) -> bytearray:
    """Fallback procedural wall tile: dark drafting grid + two crisp terminal cards."""
    buf = bytearray(tile_size * tile_size * 4)
    minor = max(1, tile_size // 16)
    major = minor * 4

    for y in range(tile_size):
        for x in range(tile_size):
            r, g, b = 0x0B, 0x0F, 0x16
            if x % major == 0 or y % major == 0:
                r, g, b = 0x1B, 0x24, 0x34
            elif x % minor == 0 or y % minor == 0:
                r, g, b = 0x12, 0x18, 0x24
            o = (y * tile_size + x) * 4
            buf[o] = r
            buf[o + 1] = g
            buf[o + 2] = b
            buf[o + 3] = 255

    def fill_rect(x0: int, y0: int, x1: int, y1: int, color: tuple[int, int, int]) -> None:
        cr, cg, cb = color
        for yy in range(max(0, y0), min(tile_size, y1)):
            row = yy * tile_size * 4
            for xx in range(max(0, x0), min(tile_size, x1)):
                i = row + xx * 4
                buf[i] = cr
                buf[i + 1] = cg
                buf[i + 2] = cb

    s = tile_size
    # Back unfocused terminal card (top-right)
    bx0, by0, bx1, by1 = int(s * 0.34), int(s * 0.13), int(s * 0.88), int(s * 0.62)
    fill_rect(bx0 - 3, by0 - 3, bx1 + 3, by1 + 3, (0x3D, 0x4B, 0x63))
    fill_rect(bx0, by0, bx1, by1, (0x10, 0x15, 0x1F))
    fill_rect(bx0, by0, bx1, by0 + int(s * 0.065), (0x1B, 0x22, 0x30))
    fill_rect(bx0 + int(s * 0.04), by0 + int(s * 0.12), bx0 + int(s * 0.36), by0 + int(s * 0.155), (0x5E, 0x81, 0xAC))
    fill_rect(bx0 + int(s * 0.04), by0 + int(s * 0.20), bx0 + int(s * 0.44), by0 + int(s * 0.235), (0x88, 0xC0, 0xD0))

    # Front focused terminal card (bottom-left) with mint-green focus border + prompt + block cursor
    fx0, fy0, fx1, fy1 = int(s * 0.11), int(s * 0.29), int(s * 0.77), int(s * 0.87)
    rim = max(4, int(s * 0.014))
    fill_rect(fx0 - rim, fy0 - rim, fx1 + rim, fy1 + rim, (0x6E, 0xE7, 0xA0))
    fill_rect(fx0, fy0, fx1, fy1, (0x0D, 0x11, 0x17))
    bar_h = int(s * 0.075)
    fill_rect(fx0, fy0, fx1, fy0 + bar_h, (0x19, 0x26, 0x24))
    fill_rect(fx0 + int(s * 0.035), fy0 + int(bar_h * 0.33), fx0 + int(s * 0.24), fy0 + int(bar_h * 0.68), (0x6E, 0xE7, 0xA0))

    # Stylized `>` chevron + `_` + solid mint block cursor inside the front terminal
    cx = fx0 + int(s * 0.07)
    cy = fy0 + bar_h + int(s * 0.17)
    arm = int(s * 0.085)
    thick = max(4, int(s * 0.026))
    for dy in range(-arm, arm + 1):
        px = cx + (arm - abs(dy))
        fill_rect(px - thick // 2, cy + dy - thick // 2, px + thick // 2, cy + dy + thick // 2, (0x6E, 0xE7, 0xA0))

    # Code bars & solid block cursor
    kx = cx + arm + int(s * 0.05)
    fill_rect(kx, cy - thick // 2, kx + int(s * 0.19), cy + thick // 2 + 2, (0xE5, 0xE9, 0xF0))
    cur_x = kx + int(s * 0.22)
    fill_rect(cur_x, cy - int(s * 0.055), cur_x + int(s * 0.065), cy + int(s * 0.055), (0x6E, 0xE7, 0xA0))

    y2 = cy + int(s * 0.14)
    fill_rect(cx, y2, cx + int(s * 0.22), y2 + int(s * 0.032), (0xEB, 0xCB, 0x8B))
    fill_rect(cx + int(s * 0.25), y2, cx + int(s * 0.48), y2 + int(s * 0.032), (0x81, 0xA1, 0xC1))
    y3 = y2 + int(s * 0.075)
    fill_rect(cx, y3, cx + int(s * 0.36), y3 + int(s * 0.032), (0xA3, 0xBE, 0x8C))
    return buf


def compose_squircle_icon(size: int, tile: bytearray, tw: int, th: int) -> bytearray:
    """Mask `tile` into an Apple HIG-proportioned rounded squircle with a mint inner rim."""
    out = bytearray(size * size * 4)
    # Apple macOS Big Sur+ icon body occupies ~824/1024 of the canvas with R ~= 185/1024.
    half = size * (412.0 / 1024.0)
    radius = size * (185.0 / 1024.0)
    rim_w = max(2.0, size * (10.0 / 1024.0))
    cx = cy = size * 0.5
    shadow_dy = size * (12.0 / 1024.0)
    shadow_blur = max(2.0, size * (18.0 / 1024.0))

    mint_r, mint_g, mint_b = 0x6E, 0xE7, 0xA0

    for y in range(size):
        py = y + 0.5
        for x in range(size):
            px = x + 0.5
            d = rounded_rect_sdf(px, py, cx, cy, half, half, radius)

            # Soft drop shadow outside the icon body
            sd = rounded_rect_sdf(px, py - shadow_dy, cx, cy, half, half, radius)
            shadow_a = 0.0
            if sd < shadow_blur:
                t = max(0.0, min(1.0, 1.0 - sd / shadow_blur))
                shadow_a = 0.38 * (t * t)

            body_a = max(0.0, min(1.0, 0.5 - d))
            if body_a <= 0.0 and shadow_a <= 0.0:
                continue

            r = g = b = 0.0
            a = shadow_a
            if body_a > 0.0:
                u = (px - (cx - half)) / (2.0 * half)
                v = (py - (cy - half)) / (2.0 * half)
                tr, tg, tb = sample_bilinear(tile, tw, th, u, v)

                # Blend subtle mint accent border along the inner edge of the squircle
                rim_t = max(0.0, min(1.0, 1.0 - (-d) / rim_w))
                rim_mix = 0.72 * (rim_t * rim_t)
                br = tr * (1.0 - rim_mix) + mint_r * rim_mix
                bg = tg * (1.0 - rim_mix) + mint_g * rim_mix
                bb = tb * (1.0 - rim_mix) + mint_b * rim_mix

                out_a = body_a + shadow_a * (1.0 - body_a)
                if out_a > 0.0:
                    r = (br * body_a) / out_a
                    g = (bg * body_a) / out_a
                    b = (bb * body_a) / out_a
                a = out_a

            i = (y * size + x) * 4
            out[i] = int(max(0.0, min(255.0, r + 0.5)))
            out[i + 1] = int(max(0.0, min(255.0, g + 0.5)))
            out[i + 2] = int(max(0.0, min(255.0, b + 0.5)))
            out[i + 3] = int(max(0.0, min(255.0, a * 255.0 + 0.5)))
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", required=True, type=Path, help="Output RGBA PNG path")
    ap.add_argument("--size", type=int, default=1024, help="Output icon width/height in px (default: 1024)")
    ap.add_argument(
        "--source-png",
        type=Path,
        default=None,
        help="Optional offscreen --wall-test-pattern PNG to crop and frame into the icon",
    )
    args = ap.parse_args()

    if args.source_png is not None and args.source_png.is_file():
        sw, sh, raw = read_rgba_png(args.source_png)
        tw, th, tile = crop_interesting_square(raw, sw, sh)
    else:
        tw = th = max(256, min(1024, args.size))
        tile = build_procedural_tile(tw)

    icon = compose_squircle_icon(args.size, tile, tw, th)
    write_rgba_png(args.out, args.size, args.size, icon)


if __name__ == "__main__":
    main()
