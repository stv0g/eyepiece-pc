//! eyepiece-gui — desktop UI for Eyepiece (stream the PC screen to DJI Goggles 3 over USB,
//! head tracking back). Runs the same session code as the `eyepiece-pc` CLI in-process,
//! on its own threads; the UI only reads stats/pose snapshots and flips control atomics.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod caps;
mod engine;
mod plot;
mod pose;
mod preview;
mod settings;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "eyepiece-gui", version, about = "Desktop UI for Eyepiece (DJI Goggles 3 as a USB display + head tracker)",
    after_help = "Options after `--` are eyepiece-pc options; they replace the saved settings for this run, e.g.\n  \
eyepiece-gui --start -- --source test --encoder x264")]
struct GuiArgs {
    /// settings file [default: the platform config dir, e.g. ~/.config/eyepiece/eyepiece-gui.toml]
    #[arg(long)]
    config: Option<PathBuf>,
    /// start streaming right away
    #[arg(long)]
    start: bool,
    /// turn the in-window preview on
    #[arg(long)]
    preview: bool,
    /// stop and close after S seconds (measurements)
    #[arg(long, value_name = "S")]
    exit_after: Option<f64>,
    /// with --exit-after: save a screenshot of the window (PPM) just before closing
    #[arg(long, hide = true)]
    screenshot: Option<PathBuf>,
    /// initial tab: overview | compare | advanced | log
    #[arg(long, hide = true)]
    tab: Option<String>,
    /// eyepiece-pc options (after `--`)
    #[arg(last = true)]
    pc_args: Vec<String>,
}

fn main() {
    // before any other thread exists: every thread created from here starts without
    // CAP_SYS_ADMIN in effect (only KMS session threads raise it, see caps.rs)
    caps::lower_effective();
    let g = GuiArgs::parse();
    let config = g.config.clone().or_else(settings::default_path);
    let mut status = None;
    let mut saved = match config.as_deref().map(settings::Saved::load) {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            eprintln!("[eyepiece-gui] {e} (using defaults)");
            status = Some(format!("settings not loaded: {e}"));
            settings::Saved::default()
        }
        None => settings::Saved::default(),
    };
    if !g.pc_args.is_empty() {
        let argv = std::iter::once("eyepiece-pc".to_string()).chain(g.pc_args.iter().cloned());
        match eyepiece_pc::cli::Args::try_parse_from(argv) {
            Ok(mut a) => {
                if !g.pc_args.iter().any(|x| x.starts_with("--stats-interval")) {
                    a.stats_interval = saved.args.stats_interval;
                }
                saved.args = a;
            }
            Err(e) => {
                e.print().ok();
                std::process::exit(2);
            }
        }
    }
    if g.preview {
        saved.ui.preview = true;
    }
    let opts = app::Options { config, start: g.start, exit_after: g.exit_after, screenshot: g.screenshot, tab: g.tab };
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Eyepiece")
            .with_app_id("eyepiece-gui")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 480.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("Eyepiece", native, Box::new(move |cc| Ok(Box::new(app::App::new(cc, saved, opts, status))))) {
        eprintln!("[eyepiece-gui] {e}");
        std::process::exit(1);
    }
}
