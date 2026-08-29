"""Turn one logo into every icon NotDiscord needs.

    python scripts/make-icons.py assets/logo.png

Source should be a square PNG, 512px or larger — the app icon, the exe icon,
the tray icon and the phone's home-screen icon all come out of it, so anything
smaller shows its edges on a high-DPI display.

Outputs (all committed, so a normal build needs none of this):
    crates/client/assets/app.ico        the exe and window icon
    crates/client/assets/logo-256.png   the tray icon, badged at runtime
    crates/webclient/pwa/icon-192.png   home screen / notification icon
    crates/webclient/pwa/icon-512.png   splash + install prompt
"""

import os
import sys

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# Windows picks the nearest of these for the taskbar, alt-tab, explorer and
# the title bar; missing a size means Windows scales one badly.
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]


def square(img):
    """Centre-crop to a square, so an off-square export doesn't stretch."""
    w, h = img.size
    if w == h:
        return img
    edge = min(w, h)
    left, top = (w - edge) // 2, (h - edge) // 2
    return img.crop((left, top, left + edge, top + edge))


def main():
    source = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "assets", "logo.png")
    if not os.path.exists(source):
        sys.exit(
            f"no logo at {source}\n"
            "Save the logo there (square PNG, 512px or bigger) and run this again."
        )

    logo = square(Image.open(source).convert("RGBA"))
    if min(logo.size) < 256:
        print(f"warning: source is only {logo.size[0]}px; 512+ looks better on a phone")

    outputs = [
        (os.path.join(ROOT, "crates", "client", "assets", "app.ico"), None),
        (os.path.join(ROOT, "crates", "client", "assets", "logo-256.png"), 256),
        (os.path.join(ROOT, "crates", "webclient", "pwa", "icon-192.png"), 192),
        (os.path.join(ROOT, "crates", "webclient", "pwa", "icon-512.png"), 512),
    ]
    for path, size in outputs:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        if path.endswith(".ico"):
            logo.save(path, sizes=[(s, s) for s in ICO_SIZES])
        else:
            logo.resize((size, size), Image.LANCZOS).save(path)
        print(f"wrote {os.path.relpath(path, ROOT)}")

    print("\nRebuild the client for the exe icon; run scripts/release-webapp.ps1 for the phone.")


if __name__ == "__main__":
    main()
