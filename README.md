# auto-display

Dock your Windows laptop and the screens come up duplicated? auto-display fixes that. It:

- switches to extend
- restores the layout and scaling Windows remembers for those displays
- applies its placement and scaling defaults when Windows has no saved setup

It runs at sign-in and whenever you plug in a monitor. The first time it sees more than one screen
after you start your PC, it sets them up and shows the card below, duplicated or not. After that it
only steps in, without the card, when plugging in a monitor leaves the screens duplicated. If you
switch to duplicate yourself later, for example to present, it leaves you alone.

Windows does not redraw the taskbar properly when the scaling changes, so auto-display restarts
Explorer after changing it. This closes open File Explorer windows.

## Layout

When Windows has a layout for the connected displays, the preview opens with that layout and the
saved scaling. The automatic layout defaults are:

- **One monitor:** the laptop sits to its left.
- **Two monitors:** side by side, with the right one as the main display and the laptop centred below.

The **Preview** card draws each display to scale using its reported physical dimensions. Drag a
display or use **Main** on a tile to edit the preview layout. **Keep** saves those edits to Windows.
Use the scaling menu in a tile to change that display's scale immediately. **Revert** restores
the settings from before setup. The card closes after 10 seconds if you leave it alone.

## Install

Run this in PowerShell on 64-bit Windows:

```powershell
irm https://raw.githubusercontent.com/pepsi-enjoyer/auto-display/master/install.ps1 | iex
```

This downloads the [latest release](https://github.com/pepsi-enjoyer/auto-display/releases/latest),
checks its SHA-256 checksum, installs it to `%LOCALAPPDATA%\auto-display`, and adds it to your user
PATH. It starts now and at every sign-in. No Rust or admin rights are needed. Run the same command
again to update; your settings are kept. Once installed, it checks for updates after one minute and
every six hours, verifies the download, and restarts itself without changing your current display layout.

You can also download `auto-display.exe` from the release and run `.\auto-display.exe install`,
then reopen PowerShell to pick up the PATH change.

Or build and install with [Rust](https://rustup.rs):

```powershell
cargo install --git https://github.com/pepsi-enjoyer/auto-display --locked
auto-display install
```

## Run it yourself

Once installed, you can set up your screens and bring up the card whenever you like:

```powershell
auto-display
```

If you installed an older version, rerun the installer once to add it to your PATH.

## Settings

`auto-display config` prints where the settings file is
(`%LOCALAPPDATA%\auto-display\config.toml`):

```toml
external_scale = 150   # scaling for a 4K external monitor, in percent
# laptop_scale = 250   # scaling for the laptop screen; leave out to keep what Windows chose
auto_update = true    # set to false to turn off automatic updates
restart_explorer = true   # set to false to leave Explorer alone after a scaling change
```

External monitors scale in proportion to their resolution's shorter side, using 2160 pixels as
the 4K baseline. Auto-display picks the nearest Windows-supported percentage. A setting of 150%
gives 100% on both 1440p and 1080p monitors.

## Commands

| Command | What it does |
|---|---|
| (none) | Set up your screens now and show the card, whether or not they are duplicated |
| `status` | Show your screens, their sizes, and whether auto-display is running |
| `update` | Check for and install a newer release now |
| `laptop left\|right\|below` | Move the laptop screen |
| `swap` | Swap two monitors |
| `layout` | Apply the layout and scaling now, without the card |
| `taskbar` | Restart Explorer if the taskbar is the wrong size |
| `preview` | Show the card with example screens (`dual`, `portrait`, `single`, `done`, `error`), without changing anything |
| `uninstall` | Stop it and remove it from sign-in and your user PATH |

The log is at `%LOCALAPPDATA%\auto-display\auto-display.log`.
