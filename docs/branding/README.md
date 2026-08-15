# Branding

`logos/zorca-logo-master.png` is the high-resolution source of truth: the ZD
monogram, white on a navy rounded square cut diagonally by a magenta-to-coral
gradient. `logos/zorca-logo-transparent.png` is the reusable transparent export
used by the application and README.

## Regenerating the app icons

```sh
./docs/branding/generate-icons.sh
```

That writes every channel's `crates/zed/resources/app-icon*.png` (512 and 1024)
plus the Windows `.ico` files. macOS `.icns` and the Linux icon theme entries
are derived from those PNGs at bundle time, so nothing else needs regenerating.

The generated PNGs are also the reusable icon styles offered in Settings >
Appearance > App Icon:

| Style    | Asset                    | Treatment          |
| -------- | ------------------------ | ------------------ |
| Classic  | `app-icon.png`           | as authored        |
| Graphite | `app-icon-dev.png`       | desaturated to 35% |
| Aurora   | `app-icon-preview.png`   | hue +40°           |
| Neon     | `app-icon-nightly.png`   | hue +99°           |

## Palette

| Role            | Hex       |
| --------------- | --------- |
| Canvas          | `#012072` |
| Gradient start  | `#932DA0` |
| Gradient end    | `#F65254` |
| Foreground      | `#FFFFFF` |

Deliberately not Zed's blue (`#084CCF`): ZOrca is a separate application and
should never be mistaken for it in a dock.
