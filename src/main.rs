#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bringback;
mod config;
mod deploy;
mod drives;
mod esde;
mod export;
mod fsutil;
mod games;
mod gauge;
mod gauge_geom;
mod manifest;
mod paths;
mod plan;
mod power;
mod scan;
mod select;
mod sync;
mod ui;
mod update;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use eframe::egui;

pub const APP_NAME: &str = "EMU Assistant";
pub const ICON_PNG: &[u8] = include_bytes!("../Icon/EMU Assistant.png");

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--dump") {
        attach_console();
        let games = args.iter().any(|a| a == "--games");
        let root = args[2..]
            .iter()
            .find(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .or_else(scan::detect_es_root_near_exe)
            .context("usage: EMUAssistant --dump <ES-DE folder> [--games]")?;
        let cat = scan::scan(&root, &AtomicBool::new(false), &mut |s| eprintln!("{s}"))?;
        scan::print_catalog(&cat, games);
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--check-updates") {
        attach_console();
        let root = args
            .get(2)
            .map(PathBuf::from)
            .or_else(scan::detect_es_root_near_exe)
            .context("usage: EMUAssistant --check-updates <ES-DE folder>")?;
        return update::print_check(&root, &config::load().update_opts(&root));
    }
    if args.get(1).map(String::as_str) == Some("--export") {
        attach_console();
        std::process::exit(headless_export(&args[2..]));
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size([1240.0, 840.0])
        .with_min_inner_size([900.0, 560.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(APP_NAME, options, Box::new(|cc| Ok(Box::new(ui::App::new(cc)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

const EXPORT_USAGE: &str = "usage: EMUAssistant --export [--profile NAME] [--dest PATH] [ES-DE folder]";

/// `--export`: exit code 0 when done, 1 when files failed, 2 when it could not start.
fn headless_export(args: &[String]) -> i32 {
    let mut h = export::Headless::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => h.profile = it.next().cloned(),
            "--dest" => h.dest = it.next().map(PathBuf::from),
            "--help" | "-h" => {
                println!("{EXPORT_USAGE}");
                return 0;
            }
            s if s.starts_with("--") => {
                eprintln!("unknown option {s}\n{EXPORT_USAGE}");
                return 2;
            }
            s => h.es_root = Some(PathBuf::from(s)),
        }
    }
    let log = std::sync::Mutex::new(export::log_file());
    let out = |s: String| {
        println!("{s}");
        if let Some((_, f)) = log.lock().unwrap().as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{s}");
        }
    };
    if let Some((p, _)) = log.lock().unwrap().as_ref() {
        println!("Log: {}", p.display());
    }
    match export::headless(&h, config::load(), &out) {
        Ok(r) if r.failed.is_empty() => 0,
        Ok(_) => 1,
        Err(e) => {
            out(format!("Not started: {}", e.0));
            2
        }
    }
}

/// Release builds use the GUI subsystem; reattach to the parent console for `--dump`.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
