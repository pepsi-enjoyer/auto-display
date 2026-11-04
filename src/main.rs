#![windows_subsystem = "windows"]

mod display;
mod fix;
mod install;
mod log;
mod ui;
mod watch;

use windows_sys::Win32::System::Console::AttachConsole;

const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

const USAGE: &str = "\
auto-display: when a docked laptop comes up in duplicate mode, switch to extend, make the external
display primary with the laptop screen to its left, and set external displays to 150%.

Usage: auto-display <command>

  install     Copy to %LOCALAPPDATA%\\auto-display, run at every sign-in, and start now
  uninstall   Stop it and remove the sign-in entry
  status      Show the connected displays and whether the watcher is running
  apply       Fix the displays now if they are duplicated, with the on-screen card
  setup       Same as apply, without a console (what the watcher starts)
  layout      Apply the layout and scaling now, even if not duplicated
  swap        Swap sides: the two monitors, or the laptop to the other side of the monitor
  watch       Run the watcher in this session (what the sign-in entry runs)
  preview [dual|single|done|error]
              Play the on-screen card with made-up screens; changes nothing";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("watch");
    if command != "watch" && command != "setup" {
        // Built as a GUI program so it never flashes a console at sign-in; borrow the caller's.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
        println!();
    }

    let result = match command {
        "watch" => watch::run(),
        "install" => install::install(),
        "uninstall" => install::uninstall(),
        "status" => status(),
        "setup" => ui::run().map(|_| ()),
        "apply" => ui::run().map(|fixed| {
            if !fixed {
                println!("Displays are not duplicated; nothing to do.");
            }
        }),
        "layout" => fix::apply_layout().map(|_| ()),
        "swap" => fix::arrange(true).map(|a| {
            if !a.swappable {
                println!("Nothing to swap with this set of screens.");
            }
        }),
        "preview" => ui::preview(args.get(1).map(String::as_str).unwrap_or("dual")),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    };
    if let Err(e) = result {
        log::line(&format!("error: {e}"));
        std::process::exit(1);
    }
}

fn status() -> Result<(), String> {
    let cfg = display::active()?;
    println!(
        "Mode: {}",
        if display::is_duplicate(&cfg.paths) { "duplicate" } else { "extend (or single display)" }
    );
    for p in &cfg.paths {
        let m = cfg.modes.get(p.source.mode_info_idx as usize).map(|m| m.source_mode());
        let scale = display::scale(p).map(|s| format!("{}% (recommended {}%)", s.current, s.recommended));
        match m {
            Some(m) => println!(
                "  {} {:<24} {}x{} at ({}, {}){}  scale {}",
                display::gdi_name(p),
                display::monitor_name(p),
                m.width,
                m.height,
                m.x,
                m.y,
                if m.x == 0 && m.y == 0 { " main" } else { "" },
                scale.unwrap_or_else(|e| e)
            ),
            None => println!("  {} {}", display::gdi_name(p), display::monitor_name(p)),
        }
    }
    println!("Watcher: {}", if watch::is_running() { "running" } else { "not running" });
    println!("Log: {}", log::path().display());
    Ok(())
}
