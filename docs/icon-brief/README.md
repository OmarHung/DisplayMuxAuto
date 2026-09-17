# Icon brief — DisplayMuxAuto

Two icons are needed. Everything the app ships is generated from them.

DisplayMuxAuto shares one monitor between a Mac and a Windows PC, switching the monitor's
input over DDC/CI so neither computer has to be unplugged and no one has to reach for the
monitor's buttons. It runs quietly in the background on both machines and is mostly invoked
by a keyboard shortcut.

## What is being replaced

The icons in this folder are placeholders, and nothing about their shapes is load-bearing.

`current-app-icon.png` is a dark rounded square holding a monitor with a two-way arrow across
its screen — the switching idea, stated literally. `current-menubar@2x.png` is a monitor
outline drawn pixel by pixel in a script, which is exactly as good as that sounds.

The monitor motif is a reasonable starting point and the palette below is genuinely the
product's, but a stronger idea is welcome.

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

### The colour is discarded

macOS treats this as a **template image**: it reads only the alpha channel and repaints the
shape itself — near-black on a light menu bar, near-white on a dark one.

A glyph that relies on its own colours, on a gradient, or on a light shape against a dark
fill will arrive as a solid blob. The shape has to work as a silhouette.

The app loads this as raw pixels rather than decoding a PNG at runtime, so there is one
conversion step after the PNG lands. That step is on the engineering side — deliver the PNG
and nothing else.

## Palette

The product's own tokens, not a set assembled for this brief. The deep green carries the
interface in light mode; the mint replaces it in dark mode.

| Role | Light | Dark |
|---|---|---|
| Accent | `#137356` | `#6EE7B7` |
| Ink | `#17211E` | `#E6EDE9` |
| Canvas | `#E8ECE9` | `#0E1512` |

The interface pairs these with **Manrope**. The app icon is free to leave the palette if the
idea calls for it; the menu bar glyph has no colour to leave.

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
| `current-app-icon.png` | the placeholder app icon, 512 × 512 |
| `current-menubar@2x.png` | the placeholder menu bar glyph, 44 × 44 |

Referenced from `src-tauri/tauri.conf.json` (`bundle.icon`) and `src-tauri/src/lib.rs`
(`setup_macos_status_item`).
