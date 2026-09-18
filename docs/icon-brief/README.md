# Icon brief — MuxSU

Two icons are needed. Everything the app ships is generated from them.

MuxSU shares one monitor between a Mac and a Windows PC, switching the monitor's
input over DDC/CI so neither computer has to be unplugged and no one has to reach for the
monitor's buttons. It runs quietly in the background on both machines and is mostly invoked
by a keyboard shortcut.

## Current mark

The selected mark uses an ivory uncle silhouette on deep navy. Its round glasses contain a
violet laptop and a turquoise desktop tower, joined by a small amber bridge to express two
computers sharing one display. The swept hair, brows and moustache carry the "Mark Uncle"
identity without drawing a conventional face.

## Deliverable A — application icon

One file. It appears in Finder, in the Windows taskbar, in the installer, and in the app's
About window.

| | |
|---|---|
| **Filename** | `app-icon.png` |
| **Size** | 1024 × 1024 px, square |
| **Format** | PNG with an alpha channel (SVG also accepted) |
| **Shape** | Draw the full artwork **including its own rounded-square container**. macOS does not add one, and the generator does not crop or mask. |
| **Bleed** | Keep the design inside roughly the middle 80%, so it survives being scaled to 16 px in a Finder list. |

Everything else is generated from it:

```bash
pnpm tauri icon app-icon.png
```

That writes `icon.icns` (macOS), `icon.ico` (Windows) and the PNG sizes from 32 px up, all
into `src-tauri/icons/`.

## Deliverable B — macOS menu bar glyph

On macOS the app has **no Dock icon at all**. This glyph in the menu bar is the only thing on
screen pointing at it, and the only way to open its window or quit it.

| | |
|---|---|
| **Filename** | `menubar@2x.png` |
| **Size** | 44 × 44 px — a 22 pt slot at @2x |
| **Glyph** | Around 32–36 px within that box, optically centred. The margin is what keeps it from crowding its neighbours. |
| **Colour** | None. Draw in solid black — see the constraint below. |
| **Strokes** | Even weight, roughly 3–4 px at this scale. Avoid detail that closes up: a 2 px gap here is one pixel on a non-Retina display. |

The final glyph simplifies the selected app icon to its swept hair, round glasses with a
central bridge node, and moustache. The coloured lenses and computer details are omitted so
the uncle signature remains readable in the 22 pt menu-bar slot.

### The colour is discarded

macOS treats this as a **template image**: it reads only the alpha channel and repaints the
shape itself — near-black on a light menu bar, near-white on a dark one.

A glyph that relies on its own colours, on a gradient, or on a light shape against a dark
fill will arrive as a solid blob. The shape has to work as a silhouette.

The app loads this as raw pixels rather than decoding a PNG at runtime, so there is one
conversion step after the PNG lands. That step is on the engineering side — deliver the PNG
and nothing else.

## Palette

The application icon and interface deliberately use separate colour systems. The icon uses
deep navy and ivory with violet, turquoise and amber device accents. The application chrome
keeps its neutral liquid-glass treatment inspired by macOS: translucent white or graphite
surfaces, soft blue/violet ambient light, fine highlight borders and restrained system-blue
actions.

| Role | Light | Dark |
|---|---|---|
| Action | `#087BEA` | `#65B7FF` |
| Selection | `#5E6FF5` | `#AAB4FF` |
| Ink | `#182235` | `#F3F7FF` |
| Base canvas | `#DCE5F0` | `#080D17` |
| Glass panel | `rgba(255,255,255,.58)` | `rgba(31,43,63,.58)` |

The interface pairs these materials with **Manrope**. The menu bar glyph remains colourless
because macOS supplies its colour from the current menu bar appearance.

## Handing the files back

Two files, named as above:

```
app-icon.png        1024 × 1024, transparent   — the app icon master
menubar@2x.png      44 × 44, alpha only        — the menu bar glyph
```

For reference, where they end up in the repository:

| Path | What |
|---|---|
| `src-tauri/icons/` | `icon.icns`, `icon.ico` and the PNG sizes, all generated |
| `src-tauri/icons/menubar.rgba` | converted from the delivered PNG, 7,744 bytes |

## How each one gets checked

- **App icon** — viewed at 16 px in a Finder list and at 32 px in the Windows taskbar. If the
  idea only reads at 512 px, it has not landed.
- **Menu bar glyph** — installed and read on both a light and a dark menu bar, beside the
  other status items already there, which are line glyphs of similar weight.
- **Both** — checked on a built `.app`, not a development build. The two differ in ways that
  have already caught us out once: the menu bar item does not exist at all in a dev build,
  because it depends on the bundle.

## Files in this folder

| File | What |
|---|---|
| `app-icon.png` | the final 1024 × 1024 MuxSU application icon |
| `menubar@2x.png` | the final 44 × 44 MuxSU template glyph |

Referenced from `src-tauri/tauri.conf.json` (`bundle.icon`) and `src-tauri/src/lib.rs`
(`setup_macos_status_item`).
