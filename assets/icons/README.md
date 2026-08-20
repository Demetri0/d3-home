# Icons

`d3home.png` (512×512) is the master; `d3home.ico` carries 16 through 256 for
Windows, which is the one platform that cannot take a PNG for a notification.

## Where it came from

Generated for this project by its author. It arrived as artwork on an opaque
white background with a drop shadow beneath it — neither of which belongs in
an icon, since every desktop draws its own background and its own shadow.

## How it was cleaned up

The rounded square was measured rather than guessed: side 1056 px at (99, 94)
in the original, corner radius 256 px, which is a plain circular arc and not a
squircle — checked against both. Cutting it out with a geometric mask, rather
than making the white transparent, removes the shadow in the same pass and
leaves no pale halo where anti-aliased edge pixels had been mixed with the
background. The mask is drawn at 4× and scaled down for a smooth edge, and
inset 3 px so that no pixel still carrying a trace of the white makes it in.

To regenerate from a new original, adjust the four measurements at the top:

```python
from PIL import Image, ImageDraw

SRC = "original.png"
LEFT, TOP, SIDE, R, INSET, SS = 99, 94, 1056, 256, 3, 4

box = Image.open(SRC).convert("RGB").crop((LEFT, TOP, LEFT + SIDE, TOP + SIDE))
mask = Image.new("L", (SIDE * SS, SIDE * SS), 0)
ImageDraw.Draw(mask).rounded_rectangle(
    [INSET * SS, INSET * SS, (SIDE - INSET) * SS - 1, (SIDE - INSET) * SS - 1],
    radius=(R - INSET) * SS, fill=255)
master = box.convert("RGBA")
master.putalpha(mask.resize((SIDE, SIDE), Image.LANCZOS))

master.resize((512, 512), Image.LANCZOS).save("d3home.png", optimize=True)
master.resize((256, 256), Image.LANCZOS).save(
    "d3home.ico",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
```

Below 32 px the detail collapses into a blue blob, as it does for any icon
with this much inside it. Nothing is done about that here: the sizes that
matter are the ones a notification shows.

## Vendor icons

Manufacturers' logos are their trademarks, and any that are ever added here
will be listed in this file with their owner named, kept out of the project's
own licence, and removed on request. None are present yet.
