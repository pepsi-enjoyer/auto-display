# auto-display

Dock your Windows laptop and the screens come up duplicated? auto-display fixes that. It:

- switches to extend
- makes the external monitor the main display
- sets external monitors to 150% scaling

It runs at sign-in and whenever you plug in a monitor. If you switch to duplicate yourself later, for
example to present, it leaves you alone.

## Layout

- **One monitor:** the laptop sits to its left, with the laptop's top edge level with the middle of
  the monitor.
- **Two monitors:** side by side, with the laptop below, centred between them.

Windows can't tell where your screens physically are, so a small card pops up asking whether the
layout is the right way round. Click **Swap sides** to fix it, or **Keep**. It keeps the layout on
its own after 10 seconds.

## Install

```powershell
cargo build --release
.\target\release\auto-display.exe install
```

This starts it now and at every sign-in. It doesn't need admin rights.

## Commands

| Command | What it does |
|---|---|
| `status` | Show your screens and whether auto-display is running |
| `swap` | Swap sides now |
| `apply` | Fix duplicated screens now |
| `preview` | Show the card with example screens (`dual`, `single`, `done`, `error`), without changing anything |
| `uninstall` | Stop it and remove it from sign-in |

The log is at `%LOCALAPPDATA%\auto-display\auto-display.log`.
