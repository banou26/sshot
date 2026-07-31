mod capture;
mod config;
mod geometry;
mod niri;
mod overlay;
mod tray;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};
use image::ImageEncoder;
use signal_hook::consts::{SIGUSR1, SIGUSR2, SIGTERM, SIGINT};
use signal_hook::iterator::Signals;

use crate::config::Config;
use crate::overlay::{CaptureResult, Selection};

/// Recursively find the most recently modified .png file under a directory.
///
/// Bounded in depth and does not follow directory symlinks, so a stray link
/// loop under the save directory cannot hang the tray.
fn find_latest_file(dir: &Path) -> Option<PathBuf> {
    const MAX_DEPTH: u32 = 8;

    fn walk(dir: &Path, depth: u32, best: &mut Option<(std::time::SystemTime, PathBuf)>) {
        if depth > MAX_DEPTH { return; }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() { continue; }
            let path = entry.path();
            if file_type.is_dir() {
                walk(&path, depth + 1, best);
            } else if path.extension().map(|e| e == "png").unwrap_or(false) {
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                        *best = Some((modified, path));
                    }
                }
            }
        }
    }

    let mut best = None;
    walk(dir, 0, &mut best);
    best.map(|(_, p)| p)
}

// ── Save & Clipboard ─────────────────────────────────────────────

/// Offer PNG bytes on the clipboard via wl-copy.
fn copy_png_to_clipboard(png: &[u8]) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["-t", "image/png"])
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run wl-copy (is wl-clipboard installed?)")?;

    child
        .stdin
        .take()
        .context("wl-copy stdin was not available")?
        .write_all(png)
        .context("failed to write image to wl-copy")?;

    let status = child.wait().context("failed to wait for wl-copy")?;
    if !status.success() {
        anyhow::bail!("wl-copy exited with {status}");
    }
    Ok(())
}

fn save_and_copy(result: &CaptureResult, config: &Config) -> Result<PathBuf> {
    match &result.selection {
        Selection::Window { title, id } => {
            // niri captures the window's own buffer, which is more accurate than
            // cropping the composite. It also puts the PNG on the clipboard
            // itself: --write-to-disk (default true) writes "in addition to"
            // the clipboard, and --path only redirects where the file lands.
            let dir = config.save_dir()?;
            let filename = config.format_filename(Some(title));
            let filepath = dir.join(&filename);

            let status = Command::new("niri")
                .args([
                    "msg", "action", "screenshot-window",
                    "--id", id,
                    "--path", &filepath.to_string_lossy(),
                ])
                .status()
                .context("Failed to run niri msg")?;

            if !status.success() {
                anyhow::bail!("niri screenshot-window failed");
            }

            eprintln!("Saved: {}", filepath.display());
            Ok(filepath)
        }
        Selection::Region { x, y, w, h } => {
            let s = result.scale;
            let ss_h = result.ss_data.len() as u32 / result.ss_stride;

            let px = ((x * s).round() as u32).min(result.ss_width.saturating_sub(1));
            let py = ((y * s).round() as u32).min(ss_h.saturating_sub(1));
            let pw = ((w * s).round() as u32).min(result.ss_width - px);
            let ph = ((h * s).round() as u32).min(ss_h - py);

            if pw == 0 || ph == 0 { anyhow::bail!("Selection is empty"); }

            // Crop from the frozen composite, BGRA to RGB. The alpha channel is
            // a constant 255 out of capture, so storing it would inflate the
            // buffer and the deflate stream by a third for no information.
            let mut rgb = vec![0u8; pw as usize * ph as usize * 3];
            for row in 0..ph {
                let src = ((py + row) * result.ss_stride + px * 4) as usize;
                let dst = (row * pw * 3) as usize;
                let src_row = &result.ss_data[src..src + pw as usize * 4];
                let dst_row = &mut rgb[dst..dst + pw as usize * 3];
                for (d, s) in dst_row.chunks_exact_mut(3).zip(src_row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            }

            // Encode once and reuse the bytes for both sinks.
            let mut png_buf = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png_buf)
                .write_image(&rgb, pw, ph, image::ExtendedColorType::Rgb8)
                .context("failed to encode PNG")?;

            // Clipboard first: if the disk write fails, the capture is still
            // recoverable by pasting it somewhere.
            if let Err(e) = copy_png_to_clipboard(&png_buf) {
                eprintln!("Clipboard failed: {e:#}");
            }

            let dir = config.save_dir()?;
            let filepath = dir.join(config.format_filename(None));
            std::fs::write(&filepath, &png_buf)
                .with_context(|| format!("failed to write {}", filepath.display()))?;

            eprintln!("Saved: {}", filepath.display());
            Ok(filepath)
        }
    }
}

// ── PID file ─────────────────────────────────────────────────────

fn pid_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    PathBuf::from(runtime_dir).join("sshot.pid")
}

fn write_pid() {
    let _ = std::fs::write(pid_path(), std::process::id().to_string());
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path()).ok()?.trim().parse().ok()
}

fn remove_pid() {
    let _ = std::fs::remove_file(pid_path());
}

/// Check if a process with the given PID is alive (sends signal 0).
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

// ── Actions ──────────────────────────────────────────────────────

/// Spawn a helper and reap it in the background, so the daemon does not
/// accumulate zombies over a long session.
fn spawn_detached(mut cmd: Command) {
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("Failed to run {:?}: {e}", cmd.get_program()),
    }
}

/// Run the overlay and persist whatever the user selected.
/// Returns the saved path so the caller can offer it without a directory walk.
fn take_screenshot(config: &Config) -> Option<PathBuf> {
    match overlay::run(config) {
        Ok(Some(result)) => match save_and_copy(&result, config) {
            Ok(path) => Some(path),
            Err(e) => {
                notify_error("Screenshot not saved", &format!("{e:#}"));
                eprintln!("Save error: {e:#}");
                None
            }
        },
        Ok(None) => None, // Cancelled
        Err(e) => {
            notify_error("Screenshot failed", &format!("{e:#}"));
            eprintln!("Screenshot error: {e:#}");
            None
        }
    }
}

/// Surface a failure to the user. The daemon's stderr goes nowhere, so without
/// this a failed capture is indistinguishable from a deliberate cancel.
fn notify_error(summary: &str, body: &str) {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "sshot", "-u", "critical", "-i", "camera-photo", summary, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_detached(cmd);
}

/// Capture the focused window using niri's built-in screenshot-window action.
fn take_window_screenshot(config: &Config) -> Result<()> {
    // Get focused window info for the filename
    let title = Command::new("niri")
        .args(["msg", "--json", "focused-window"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v["title"].as_str().map(String::from));

    let dir = config.save_dir()?;
    let filename = config.format_filename(title.as_deref());
    let filepath = dir.join(&filename);

    let status = Command::new("niri")
        .args([
            "msg", "action", "screenshot-window",
            "--path", &filepath.to_string_lossy(),
        ])
        .status()
        .context("Failed to run niri msg")?;

    if !status.success() {
        anyhow::bail!("niri screenshot-window failed (no focused window?)");
    }

    eprintln!("Saved: {}", filepath.display());
    Ok(())
}

fn open_config() {
    let path = Config::config_path();
    // A nix-managed config is a read-only store symlink, so opening it is a
    // dead end. Point at the option that actually controls it instead.
    if path.is_symlink() {
        notify_error(
            "Config is managed by nix",
            "Edit programs.sshot in your home-manager configuration, then Reload Config.",
        );
        eprintln!("Config is a nix store symlink: {}", path.display());
        return;
    }
    if !path.exists() {
        Config::default().save_to_disk();
    }
    let mut cmd = Command::new("xdg-open");
    cmd.arg(&path);
    spawn_detached(cmd);
}

/// Signal the running daemon. Reports whether a live daemon received it.
fn signal_daemon(sig: libc::c_int) -> bool {
    let Some(pid) = read_pid() else { return false };
    if !is_process_alive(pid) {
        remove_pid();
        return false;
    }
    unsafe { libc::kill(pid as i32, sig) == 0 }
}

// ── Entry point ──────────────────────────────────────────────────

const USAGE: &str = "\
Usage: sshot [--daemon | --trigger | --window | --settings | --oneshot]
  --daemon    Start as background daemon with system tray (default)
  --trigger   Signal running daemon to take a screenshot
  --window    Screenshot the focused window (via niri)
  --settings  Signal running daemon to open settings
  --oneshot   Take one screenshot and exit
  --version   Print version and exit
  --help      Print this message";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("--daemon");

    match mode {
        "--trigger" => {
            if !signal_daemon(SIGUSR1) {
                eprintln!("No running daemon found. Start with: sshot --daemon");
                std::process::exit(1);
            }
            return Ok(());
        }
        "--window" => {
            let config = Config::load();
            if let Err(e) = take_window_screenshot(&config) {
                eprintln!("Window screenshot error: {e:#}");
                std::process::exit(1);
            }
            return Ok(());
        }
        "--settings" => {
            if !signal_daemon(SIGUSR2) {
                open_config();
            }
            return Ok(());
        }
        "--oneshot" => {
            let config = Config::load();
            if let Some(path) = take_screenshot(&config) {
                println!("{}", path.display());
            }
            return Ok(());
        }
        "--debug-windows" => {
            // Print the reconstructed rectangles without showing an overlay,
            // so they can be compared against what is actually on screen.
            let config = Config::load();
            let (data, width, height, stride) = capture::capture_workspace()?;
            let snap = niri::snapshot()?;
            let cap = geometry::Capture { data: &data, width, height, stride };
            let gap = overlay::effective_gap(&config);
            let windows = geometry::layout_windows(&snap, gap, Some(&cap));
            println!("capture {width}x{height} gap={gap}");
            for w in &windows {
                println!(
                    "id={:<5} x={:<9.2} y={:<9.2} w={:<9.2} h={:<9.2} {}",
                    w.id, w.x, w.y, w.width, w.height, w.title
                );
            }
            return Ok(());
        }
        "--version" | "-V" => {
            println!("sshot {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        "--help" | "-h" => {
            println!("{USAGE}");
            return Ok(());
        }
        "--daemon" => {}
        other => {
            // Falling through to daemon mode here would start a second daemon,
            // steal the PID file, and disarm the real one's hotkey on exit.
            eprintln!("sshot: unknown option '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
    }

    // ── Daemon mode ──────────────────────────────────────────────
    // Single-instance check: if another daemon is already running, exit.
    if let Some(existing) = read_pid() {
        if is_process_alive(existing) {
            eprintln!("sshot daemon already running (pid {existing}), exiting.");
            return Ok(());
        }
        // Stale PID file - remove and continue
        remove_pid();
    }

    let mut config = Config::load();
    eprintln!("Screenshot daemon starting. Config: {}", Config::config_path().display());

    // Register before anything else claims the process. SIGUSR1's default
    // disposition is terminate, so if this failed silently in a background
    // thread the daemon's own hotkey would kill it.
    let mut signals = Signals::new([SIGUSR1, SIGUSR2, SIGTERM, SIGINT])
        .context("failed to register signal handlers")?;

    write_pid();

    let (tx, rx) = mpsc::channel::<tray::Action>();

    // System tray (background thread via ksni; retries until the watcher is up)
    tray::spawn(tx.clone());

    let sig_tx = tx.clone();
    std::thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGUSR1 => { let _ = sig_tx.send(tray::Action::TakeScreenshot); }
                SIGUSR2 => { let _ = sig_tx.send(tray::Action::OpenConfig); }
                // Do not break: leaving forever() unregisters every handler,
                // so a second SIGTERM would fall through to the default
                // disposition while the daemon is still shutting down.
                SIGTERM | SIGINT => { let _ = sig_tx.send(tray::Action::Quit); }
                _ => {}
            }
        }
    });

    eprintln!("Ready. Use 'sshot --trigger' or system tray to capture.");

    // Remembered so the tray does not have to walk the save tree for a path
    // the previous capture already returned.
    let mut last_saved: Option<PathBuf> = None;

    // Main event loop
    while let Ok(action) = rx.recv() {
        // A capture blocks this thread, so triggers pressed during one queue up.
        // Collapse a burst into a single capture rather than replaying the
        // overlay once per keypress.
        let action = match action {
            tray::Action::TakeScreenshot => {
                while let Ok(next) = rx.try_recv() {
                    if !matches!(next, tray::Action::TakeScreenshot) {
                        // Not a duplicate trigger, so handle it after this capture.
                        let _ = tx.send(next);
                        break;
                    }
                }
                tray::Action::TakeScreenshot
            }
            other => other,
        };

        match action {
            tray::Action::TakeScreenshot => {
                if let Some(path) = take_screenshot(&config) {
                    last_saved = Some(path);
                }
            }
            tray::Action::OpenLastScreenshot => {
                let base = Config::expand_path(&config.save.directory);
                let latest = last_saved
                    .as_ref()
                    .filter(|p| p.exists())
                    .cloned()
                    .or_else(|| find_latest_file(&base));

                match latest {
                    Some(path) => {
                        let mut cmd = Command::new(&config.save.file_manager);
                        // Most file managers take --select to reveal a file.
                        if config.save.file_manager_select_arg.is_empty() {
                            cmd.arg(path.parent().unwrap_or(&base));
                        } else {
                            cmd.arg(&config.save.file_manager_select_arg).arg(&path);
                        }
                        if cmd.spawn().is_err() {
                            let mut fallback = Command::new("xdg-open");
                            fallback.arg(path.parent().unwrap_or(&base));
                            spawn_detached(fallback);
                        }
                    }
                    None => {
                        let mut cmd = Command::new("xdg-open");
                        cmd.arg(&base);
                        spawn_detached(cmd);
                    }
                }
            }
            tray::Action::OpenFolder => {
                let dir = config.save_dir().unwrap_or_else(|_| PathBuf::from(&config.save.directory));
                let mut cmd = Command::new("xdg-open");
                cmd.arg(&dir);
                spawn_detached(cmd);
            }
            tray::Action::OpenConfig => open_config(),
            tray::Action::ReloadConfig => {
                config = Config::load();
                eprintln!("Config reloaded");
            }
            tray::Action::Quit => break,
        }
    }

    remove_pid();
    eprintln!("Daemon stopped");
    Ok(())
}
