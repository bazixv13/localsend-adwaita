#!/usr/bin/env python3
"""Render the LocalSend icon concepts to PNG and build preview sheets.

Needs python3-gi with Rsvg + cairo. Usage: python3 icon-concepts/render.py
"""
import os
import cairo
import gi

gi.require_version("Rsvg", "2.0")
from gi.repository import Rsvg  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))

ICONS = {
    # ---- batch 1: perspective experiments -----------------------------------
    "1-isometric-tile": """
  <defs>
    <linearGradient id="a1bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#5aa0f7"/><stop offset="1" stop-color="#1f68dd"/>
    </linearGradient>
    <linearGradient id="a1paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <ellipse cx="66" cy="112" rx="40" ry="8" fill="#000000" opacity="0.18"/>
  <path d="M20 34 L64 14 L108 34 L108 90 L64 110 L20 90 Z" fill="#134fa8"/>
  <path d="M20 34 L64 54 L108 34 L108 86 L64 106 L20 86 Z" fill="url(#a1bg)"/>
  <path d="M20 34 L64 14 L108 34 L64 54 Z" fill="#6fb0ff" opacity="0.9"/>
  <g stroke="#ffffff" stroke-opacity="0.35" stroke-width="4" stroke-linecap="round">
    <path d="M30 78h12"/><path d="M24 88h8"/>
  </g>
  <path d="M86 42 L40 62 L58 70 L66 90 L74 74 Z" fill="url(#a1paper)"/>
  <path d="M86 42 L58 70 L66 90 L74 74 Z" fill="#b9d2ff"/>
  <path d="M86 42 L58 70 L54 64 Z" fill="#eaf2ff"/>
""",
    "2-vault-tile-depth": """
  <defs>
    <linearGradient id="a2bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#559cf6"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a2paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="14" y="22" width="100" height="96" rx="24" fill="#0f469f"/>
  <rect x="14" y="14" width="100" height="96" rx="24" fill="url(#a2bg)"/>
  <rect x="18" y="18" width="92" height="40" rx="20" fill="#ffffff" opacity="0.12"/>
  <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5" stroke-linecap="round">
    <path d="M30 80h16"/><path d="M24 92h10"/>
  </g>
  <path d="M96 36 L30 62 L54 70 L62 94 L74 73 Z" fill="url(#a2paper)"/>
  <path d="M96 36 L54 70 L62 94 L74 73 Z" fill="#b9d2ff"/>
  <path d="M96 36 L54 70 L49 63 Z" fill="#eaf2ff"/>
""",
    "3-tilted-3d-card": """
  <defs>
    <linearGradient id="a3bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#5aa0f7"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a3paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <ellipse cx="70" cy="110" rx="38" ry="7" fill="#000" opacity="0.16"/>
  <g transform="skewY(-6) translate(0,8)">
    <rect x="18" y="18" width="88" height="84" rx="22" fill="#124da6"/>
    <rect x="18" y="12" width="88" height="84" rx="22" fill="url(#a3bg)"/>
    <rect x="22" y="16" width="80" height="30" rx="16" fill="#fff" opacity="0.10"/>
    <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5" stroke-linecap="round">
      <path d="M32 72h14"/><path d="M28 82h9"/>
    </g>
    <path d="M94 30 L34 54 L54 62 L62 84 L72 64 Z" fill="url(#a3paper)"/>
    <path d="M94 30 L54 62 L62 84 L72 64 Z" fill="#b9d2ff"/>
    <path d="M94 30 L54 62 L49 56 Z" fill="#eaf2ff"/>
  </g>
""",
    "4-extruded-rounded": """
  <defs>
    <linearGradient id="a4bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#4f97f5"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a4side" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#2f75e8"/><stop offset="1" stop-color="#124da6"/>
    </linearGradient>
    <linearGradient id="a4paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="12" y="20" width="104" height="96" rx="24" fill="url(#a4side)"/>
  <rect x="12" y="12" width="104" height="96" rx="24" fill="url(#a4bg)"/>
  <rect x="16" y="16" width="96" height="41" rx="20" fill="#ffffff" opacity="0.11"/>
  <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5.42" stroke-linecap="round">
    <path d="M21.75 89.25h10.83"/><path d="M28.25 76.25h17.33"/>
  </g>
  <path d="M99.75 26.42 L28.25 56.75 L54.25 65.42 L62.92 93.58 L75.92 69.75 Z" fill="url(#a4paper)"/>
  <path d="M99.75 26.42 L54.25 65.42 L62.92 93.58 L75.92 69.75 Z" fill="#b9d2ff"/>
  <path d="M99.75 26.42 L54.25 65.42 L48.83 57.83 Z" fill="#eaf2ff"/>
""",
    "5-layered-perspective": """
  <defs>
    <linearGradient id="a5bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#63a6ff"/><stop offset="1" stop-color="#1c66db"/>
    </linearGradient>
    <linearGradient id="a5mid" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#3e86ee"/><stop offset="1" stop-color="#1558c4"/>
    </linearGradient>
    <linearGradient id="a5paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <ellipse cx="66" cy="114" rx="44" ry="8" fill="#000" opacity="0.15"/>
  <rect x="30" y="34" width="86" height="78" rx="22" fill="#0b3781"/>
  <rect x="24" y="24" width="86" height="78" rx="22" fill="url(#a5mid)"/>
  <rect x="18" y="14" width="86" height="78" rx="22" fill="url(#a5bg)"/>
  <rect x="22" y="18" width="80" height="28" rx="14" fill="#fff" opacity="0.13"/>
  <g stroke="#ffffff" stroke-opacity="0.34" stroke-width="5" stroke-linecap="round">
    <path d="M32 74h14"/><path d="M27 84h9"/>
  </g>
  <path d="M94 32 L34 56 L56 64 L64 86 L74 66 Z" fill="url(#a5paper)"/>
  <path d="M94 32 L56 64 L64 86 L74 66 Z" fill="#b9d2ff"/>
  <path d="M94 32 L56 64 L51 58 Z" fill="#eaf2ff"/>
""",
    # ---- batch 2: variations on 2 and 4 -------------------------------------
    "6-deep-lip-flat": """
  <defs>
    <linearGradient id="a6bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#58a2f8"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a6paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="16" y="25" width="96" height="90" rx="24" fill="#0a3d92"/>
  <rect x="16" y="12" width="96" height="90" rx="24" fill="url(#a6bg)"/>
  <rect x="20" y="16" width="88" height="38" rx="19" fill="#ffffff" opacity="0.13"/>
  <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5" stroke-linecap="round">
    <path d="M32 78h16"/><path d="M26 90h10"/>
  </g>
  <path d="M98 34 L30 61 L55 69 L63 93 L75 72 Z" fill="url(#a6paper)"/>
  <path d="M98 34 L55 69 L63 93 L75 72 Z" fill="#b9d2ff"/>
  <path d="M98 34 L55 69 L50 62 Z" fill="#eaf2ff"/>
""",
    "7-gradient-lip-wide": """
  <defs>
    <linearGradient id="a7bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#4f97f5"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a7side" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#3479ea"/><stop offset="1" stop-color="#0f4599"/>
    </linearGradient>
    <linearGradient id="a7paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="15" y="23" width="98" height="90" rx="25" fill="url(#a7side)"/>
  <rect x="15" y="12" width="98" height="90" rx="25" fill="url(#a7bg)"/>
  <rect x="19" y="16" width="90" height="40" rx="20" fill="#ffffff" opacity="0.12"/>
  <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5" stroke-linecap="round">
    <path d="M31 78h16"/><path d="M25 90h10"/>
  </g>
  <path d="M99 34 L31 61 L56 69 L64 94 L76 73 Z" fill="url(#a7paper)"/>
  <path d="M99 34 L56 69 L64 94 L76 73 Z" fill="#b9d2ff"/>
  <path d="M99 34 L56 69 L51 62 Z" fill="#eaf2ff"/>
""",
    "8-two-tier-plinth": """
  <defs>
    <linearGradient id="a8bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#5aa0f7"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a8mid" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#2f74e6"/><stop offset="1" stop-color="#1a56bb"/>
    </linearGradient>
    <linearGradient id="a8paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="18" y="27" width="92" height="86" rx="23" fill="#0a3a89"/>
  <rect x="18" y="20" width="92" height="86" rx="23" fill="url(#a8mid)"/>
  <rect x="18" y="12" width="92" height="86" rx="23" fill="url(#a8bg)"/>
  <rect x="22" y="16" width="84" height="34" rx="17" fill="#ffffff" opacity="0.12"/>
  <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5" stroke-linecap="round">
    <path d="M32 74h16"/><path d="M26 86h10"/>
  </g>
  <path d="M96 32 L30 58 L54 66 L62 90 L74 70 Z" fill="url(#a8paper)"/>
  <path d="M96 32 L54 66 L62 90 L74 70 Z" fill="#b9d2ff"/>
  <path d="M96 32 L54 66 L49 60 Z" fill="#eaf2ff"/>
""",
    "9-squircle-soft": """
  <defs>
    <linearGradient id="a9bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#5ca3f9"/><stop offset="1" stop-color="#1c66dc"/>
    </linearGradient>
    <linearGradient id="a9paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="18" y="22" width="92" height="88" rx="30" fill="#12479b"/>
  <rect x="18" y="12" width="92" height="88" rx="30" fill="url(#a9bg)"/>
  <rect x="22" y="16" width="84" height="36" rx="24" fill="#ffffff" opacity="0.12"/>
  <g stroke="#ffffff" stroke-opacity="0.30" stroke-width="5" stroke-linecap="round">
    <path d="M32 76h15"/><path d="M26 87h9"/>
  </g>
  <path d="M96 32 L32 58 L55 66 L63 90 L74 70 Z" fill="url(#a9paper)"/>
  <path d="M96 32 L55 66 L63 90 L74 70 Z" fill="#b9d2ff"/>
  <path d="M96 32 L55 66 L50 60 Z" fill="#eaf2ff"/>
""",
    "10-edge-bevel": """
  <defs>
    <linearGradient id="a10bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#4f97f5"/><stop offset="1" stop-color="#1a63d8"/>
    </linearGradient>
    <linearGradient id="a10side" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#3479ea"/><stop offset="1" stop-color="#0f4599"/>
    </linearGradient>
    <linearGradient id="a10paper" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#d7e6ff"/>
    </linearGradient>
  </defs>
  <rect x="17" y="21" width="94" height="88" rx="20" fill="url(#a10side)"/>
  <rect x="17" y="12" width="94" height="88" rx="20" fill="url(#a10bg)"/>
  <rect x="38" y="99.4" width="52" height="1.6" rx="0.8" fill="#a8ccff" opacity="0.30"/>
  <rect x="21" y="16" width="86" height="36" rx="16" fill="#ffffff" opacity="0.11"/>
  <g stroke="#ffffff" stroke-opacity="0.32" stroke-width="5" stroke-linecap="round">
    <path d="M31 76h16"/><path d="M25 88h10"/>
  </g>
  <g transform="rotate(-4 64 64)">
    <path d="M98 32 L31 58 L55 66 L63 91 L75 70 Z" fill="url(#a10paper)"/>
    <path d="M98 32 L55 66 L63 91 L75 70 Z" fill="#b9d2ff"/>
    <path d="M98 32 L55 66 L50 60 Z" fill="#eaf2ff"/>
  </g>
""",
}

PREVIEWS = {
    "preview": ["1-isometric-tile", "2-vault-tile-depth", "3-tilted-3d-card",
                "4-extruded-rounded", "5-layered-perspective"],
    "preview-2": ["6-deep-lip-flat", "7-gradient-lip-wide", "8-two-tier-plinth",
                  "9-squircle-soft", "10-edge-bevel"],
}


def svg_doc(body: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" '
        'viewBox="0 0 128 128">' + body + "</svg>\n"
    )


def render(svg: str, path: str, out_w: int, out_h: int, doc_w: int, doc_h: int) -> None:
    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, out_w, out_h)
    ctx = cairo.Context(surface)
    ctx.scale(out_w / float(doc_w), out_h / float(doc_h))
    handle = Rsvg.Handle.new_from_data(svg.encode("utf-8"))
    handle.render_cairo(ctx)
    surface.write_to_png(path)


def sheet(names: list) -> str:
    width, height = 1120, 420
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}">',
        f'<rect width="{width}" height="{height}" fill="#f6f6f6"/>',
    ]
    for i, name in enumerate(names):
        body = ICONS[name]
        x = 40 + i * 216
        parts.append(f'<g transform="translate({x},40) scale(1.5)">{body}</g>')
        parts.append(f'<g transform="translate({x + 48},300) scale(0.28)">{body}</g>')
        parts.append(
            f'<text x="{x + 12}" y="{392}" font-family="Cantarell, sans-serif" '
            f'font-size="20" fill="#333">{name.split("-")[0]}</text>'
        )
    parts.append("</svg>")
    return "".join(parts)


def main() -> None:
    for name, body in ICONS.items():
        svg = svg_doc(body)
        with open(os.path.join(HERE, f"{name}.svg"), "w", encoding="utf-8") as fh:
            fh.write(svg)
        for size in (32, 64, 128, 256):
            render(svg, os.path.join(HERE, f"{name}-{size}.png"), size, size, 128, 128)
        print(f"rendered {name}")

    for filename, names in PREVIEWS.items():
        render(sheet(names), os.path.join(HERE, f"{filename}.png"), 1120, 420, 1120, 420)
        print(f"rendered {filename}.png")


if __name__ == "__main__":
    main()
