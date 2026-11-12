#![windows_subsystem = "windows"]

mod config;
mod display;
mod fix;
mod install;
mod log;
mod ui;
mod update;
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
  update      Check GitHub for a newer release and install it now
  apply       Fix the displays now if they are duplicated, with the on-screen card
  setup       Same as apply, without a console (what the watcher starts)
  layout      Apply the layout and scaling now, even if not duplicated
  laptop left|right|below
              Move the laptop screen to that side of the external monitors
  swap        Swap the two external monitors
  config      Create config.toml if needed and print its path
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
        "watch" if args.get(1).is_some_and(|a| a == "--updated") => match args.get(2).and_then(|a| a.parse().ok()) {
            Some(parent) => watch::resume_after_update(parent),
            None => Err("Missing process ID for update restart".into()),
        },
        "watch" => watch::run(),
        "install" => install::install(),
        "uninstall" => install::uninstall(),
        "status" => status(),
        "update" => update::run(),
        "--version" | "-V" => {
            println!("auto-display {}", update::CURRENT);
            Ok(())
        }
        "setup" => ui::run().map(|_| ()),
        "apply" => ui::run().map(|fixed| {
            if !fixed {
                println!("Displays are not duplicated; nothing to do.");
            }
        }),
        "layout" => fix::apply_layout().map(|_| ()),
        "laptop" => match args.get(1).map(String::as_str) {
            Some("left") => fix::arrange(Some(display::Side::Left), false).map(|_| ()),
            Some("right") => fix::arrange(Some(display::Side::Right), false).map(|_| ()),
            Some("below") => fix::arrange(Some(display::Side::Below), false).map(|_| ()),
            _ => Err("usage: auto-display laptop left|right|below".into()),
        },
        "swap" => fix::arrange(None, true).map(|a| {
            if !a.dual {
                println!("Only two side-by-side monitors can be swapped.");
            }
        }),
        "config" => config::ensure().map(|p| println!("{}", p.display())),
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
    println!("auto-display {}", update::CURRENT);
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
    for s in display::screens().unwrap_or_default() {
        match s.mm {
            Some((w, h)) => {
                let inches = (w * w + h * h).sqrt() / 25.4;
                println!("  {} is {w:.0} x {h:.0} mm ({inches:.1}\")", s.name);
            }
            None => println!("  {}: physical size unknown", s.name),
        }
    }
    println!("Watcher: {}", if watch::is_running() { "running" } else { "not running" });
    println!("Config: {}", config::path().display());
    println!("Log: {}", log::path().display());
    Ok(())
}
