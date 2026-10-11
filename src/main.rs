#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod deploy;
mod drives;
mod esde;
mod fsutil;
mod gauge;
mod gauge_geom;
mod manifest;
mod paths;
mod plan;
mod scan;
mod sync;
mod ui;

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
        let root = args
            .get(2)
            .map(PathBuf::from)
            .or_else(scan::detect_es_root_near_exe)
            .context("usage: EMUAssistant --dump <ES-DE folder>")?;
        let cat = scan::scan(&root, &AtomicBool::new(false), &mut |s| eprintln!("{s}"))?;
        scan::print_catalog(&cat);
        return Ok(());
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

/// Release builds use the GUI subsystem; reattach to the parent console for `--dump`.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
