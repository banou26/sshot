# sshot

A Wayland screenshot utility with a system tray, built for [niri](https://github.com/YaLTeR/niri).

Freeze the screen, then either drag a region or click a window. Whatever you pick
is written to disk and put on the clipboard.

Screenshots only. Despite the name, there is no screen recording; use
`wf-recorder` or `gpu-screen-recorder` for that.

## Usage

```
sshot [--daemon | --trigger | --window | --settings | --oneshot]
  --daemon    Start as background daemon with system tray (default)
  --trigger   Signal running daemon to take a screenshot
  --window    Screenshot the focused window (via niri)
  --settings  Signal running daemon to open settings
  --oneshot   Take one screenshot and exit
  --version   Print version and exit
  --help      Print this message
```

In the overlay: drag for a region, click a window to capture it, right click or
Escape to cancel. `--oneshot` prints the saved path on stdout.

`--debug-windows` prints the window rectangles sshot has detected, without
showing an overlay. Useful when a highlight lands in the wrong place.

## Runtime dependencies

`grim`, `wl-clipboard`, `xdg-utils`, and `libnotify`. niri is required for
window detection; region capture works without it. The nix package wraps all of
these onto `PATH`.

## Configuration

JSON at `~/.config/sshot/config.json`, created on first run. Via the
home-manager module, set `programs.sshot.*` instead.

| Key | Meaning |
| --- | --- |
| `save.directory` | Base directory, `~` is expanded |
| `save.subfolder` | strftime subfolder, e.g. `%Y-%m` |
| `save.window_format` | Filename for window captures. `{title}`, `{random}`, strftime |
| `save.region_format` | Filename for region captures. `{random}`, strftime |
| `save.file_manager` | Program the tray uses to reveal a capture |
| `save.file_manager_select_arg` | Argument that selects the file, e.g. `--select` |
| `appearance.dim_factor` | 0.0 is black, 1.0 is no dimming |
| `appearance.border_width` | Highlight border, in pixels |
| `appearance.window_border_color` | `[R, G, B]` |
| `appearance.region_border_color` | `[R, G, B]` |
| `appearance.tiling_gap` | Fallback only, see below |

sshot reads `layout { gaps }` out of niri's own config, so the tiling gap does
not have to be configured twice. `appearance.tiling_gap` applies only when
niri's config cannot be read.

## How window detection works

niri's IPC reports exact window and tile sizes and their order in the scrolling
layout, but not two things sshot needs: where the working area starts
vertically (bars and docks take an exclusive zone that no IPC field reports) and
how far the layout is scrolled horizontally.

Both are one scalar per output, and both are recovered from the frozen capture.
The layout predicts exactly where tile edges must fall for a given offset, so
the offset whose predictions line up with real edges in the image is the right
one. When no offset stands out and the layout is scrolled, sshot reports no
windows for that output rather than highlighting the wrong ones; region
selection is unaffected.

Tabbed columns overlay their tiles, so only one is on screen. niri exposes no
per-column tab state, so the most recently focused tile in a column is taken as
the visible one.

## Development

```
nix develop
cargo test
cargo clippy --all-targets
```

`cargo build` outside the dev shell fails: `xkbcommon` will not be on
`PKG_CONFIG_PATH`.
