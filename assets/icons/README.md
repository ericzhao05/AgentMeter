# AgentMeter icons

| File | Size | Use |
| --- | --- | --- |
| `app-icon.svg` | 1024×1024 source | App icon artwork: a cel-shaded glass tube with magenta neon caps and acid-green liquid on a dark rounded tile |
| `app-icon.png` | 1024×1024, RGBA | Input for `cargo tauri icon assets/icons/app-icon.png` (generates `.icns`, `.ico` and the PNG sizes) |
| `tray-icon.svg` | 22×22 source | Menu-bar glyph |
| `tray-icon.png` | 22×22, RGBA | Menu-bar icon, 1x |
| `tray-icon@2x.png` | 44×44, RGBA | Menu-bar icon, 2x (Retina) |

## Notes

- **App icon** follows the macOS grid: an 824px rounded tile centred on a transparent 1024 canvas, so it sits
  correctly next to other Dock icons. Checked at 32px: the caps, neon strips and liquid level stay readable.
- **Tray icon** is a *template image*: pure black plus alpha. Set `iconAsTemplate: true` (or
  `TrayIconBuilder::icon_as_template(true)`) so macOS tints it for light and dark menu bars. It is drawn on whole
  pixels so the 1x render has no half-pixel blur. Use `tray-icon@2x.png` when the API takes one image; macOS
  scales it down cleanly.
- Colours come from the burette mockup: outline `#0A0A0C`, cel greys `#2A2A2E / #38383E / #4A4A52 / #151517`,
  neon `#D946EF`, liquid `#8CFF3A → #3FCF1A`.

## Regenerating the PNGs

No project dependencies are needed. The PNGs were rendered with headless Google Chrome from the repo root:

```sh
C="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
D="$PWD/assets/icons"
"$C" --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 \
  --window-size=1024,1024 --screenshot="$D/app-icon.png" "file://$D/app-icon.svg"
"$C" --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 \
  --force-device-scale-factor=1 --window-size=22,22 --screenshot="$D/tray-icon.png" "file://$D/tray-icon.svg"
"$C" --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 \
  --force-device-scale-factor=2 --window-size=22,22 --screenshot="$D/tray-icon@2x.png" "file://$D/tray-icon.svg"
```
