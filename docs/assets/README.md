# Assets

| File | What | Source |
|---|---|---|
| `brand/logo-{light,dark}.svg` | The Magnetite mark: an Fe₃O₄ element tile | `scripts/brand/build_brand.py` |
| `brand/banner-{light,dark}.svg` | README header: the tile with the name beneath | `scripts/brand/build_brand.py` |
| `brand/icon.svg` | Small "Fe" mark for favicons and avatars | `scripts/brand/build_brand.py` |
| `brand/social-preview.{svg,png}` | 1280×640 GitHub social preview | `scripts/brand/build_brand.py` |
| `boot.gif` | A real UEFI boot, recorded from the serial console with real timing | `scripts/brand/boot_gif.sh` |
| `architecture.svg` | Layer diagram of the kernel | `scripts/brand/arch_svg.py` |
| `boot-sequence.svg` | Boot flow from firmware to `kmain`'s steps | `scripts/brand/boot_svg.py` |
| `dev-toolchain.svg`, `interrupt-flow.svg`, `timer-preemption.svg` | Step-by-step flows in the development and interrupts docs | `scripts/brand/flow_svg.py` |
| `capabilities.svg`, `memory-layout.svg` | The capability model and the virtual address space | `scripts/brand/model_svg.py` |

All text in the brand SVGs is converted to outlines, so they render the same
everywhere without web fonts. To regenerate them you need Python with
`fonttools`, plus the OFL-licensed fonts
[Archivo](https://fonts.google.com/specimen/Archivo) (variable) and
[IBM Plex Mono](https://fonts.google.com/specimen/IBM+Plex+Mono):

```sh
python3 -m venv .venv && .venv/bin/pip install fonttools
.venv/bin/python scripts/brand/build_brand.py <fonts-dir> docs/assets/brand

# boot recording (needs qemu, agg from asciinema, and ffmpeg); build from a
# clean tree so the banner's version is not marked dirty
cargo xtask iso && scripts/brand/boot_gif.sh docs/assets/boot.gif

python3 scripts/brand/arch_svg.py docs/assets/architecture.svg
python3 scripts/brand/boot_svg.py docs/assets/boot-sequence.svg
python3 scripts/brand/flow_svg.py docs/assets
python3 scripts/brand/model_svg.py docs/assets
```

The PNG social preview is a screenshot of the SVG at 1280×640. Upload it under
the repository's **Settings → Social preview**.
