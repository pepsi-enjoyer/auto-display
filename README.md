# auto-display

When a Windows laptop is docked and the screens come up duplicated, auto-display:

1. switches to extend
2. makes an external monitor the main display and arranges the screens (see below)
3. sets every external monitor to 150% scaling

It checks at sign-in and whenever a monitor is plugged in or removed. It only acts in the seconds after
the set of connected monitors changes, so if you choose duplicate yourself later (for example with
Win+P to present), it leaves that alone.

## On-screen card

While it works, a small card on the main display shows what it is doing and a live diagram of the
screens. When it used the two-monitor layout, the card asks whether the monitors are the right way
round: **Swap sides** reverses them (and restarts the countdown, so you can swap back), **Keep** or
waiting 10 seconds keeps them. Enter or Esc keeps, S swaps. Otherwise it shows "Displays ready" for a
few seconds and fades out.

The card runs as a separate short-lived process (`auto-display setup`). If it cannot start, the
watcher fixes the displays without it. Try it without touching your displays:

```powershell
auto-display preview dual     # also: single, error
```

## Layout

Positions are in physical pixels.

- **Two external monitors, neither ultrawide** (narrower than 2:1): side by side with tops aligned,
  in the order Windows already had them. The left one is the main display. The laptop screen, if on,
  sits below both, centred on the line between them.
- **Anything else** (for example one ultrawide): the first external monitor is the main display at
  (0, 0), any other external monitors follow it to the right with tops aligned, and the laptop screen
  sits to its left with the laptop's top edge halfway down the main monitor.

## Install

```powershell
cargo build --release
.\target\release\auto-display.exe install
```

`install` copies the program to `%LOCALAPPDATA%\auto-display`, adds a sign-in entry under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` and starts it. No admin rights are needed. Run
`install` again after rebuilding to replace the running copy.

## Commands

| Command | What it does |
|---|---|
| `install` | Install, run at every sign-in, and start now |
| `uninstall` | Stop it and remove the sign-in entry |
| `status` | Show connected displays, positions, scaling and whether the watcher is running |
| `apply` | Fix the displays now if they are duplicated, with the on-screen card |
| `preview [dual\|single\|error]` | Play the card with made-up screens; changes nothing |
| `layout` | Apply the layout and scaling now, even if not duplicated |
| `watch` | Run the watcher (what the sign-in entry runs) |

It is built as a windowed program so nothing flashes at sign-in, so output from these commands can
appear after your prompt returns.

The log is at `%LOCALAPPDATA%\auto-display\auto-display.log`.

## How it works

It uses the Windows display configuration API (`QueryDisplayConfig` / `SetDisplayConfig`).
Duplicate mode shows up as two active paths sharing one source. Scaling is set through the
undocumented `DisplayConfigSetDeviceInfo` DPI packet (type -4), the same one the Settings app uses,
which takes an offset from the monitor's recommended scaling step.
