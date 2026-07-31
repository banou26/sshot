//! Typed niri IPC.
//!
//! Everything here mirrors `niri-ipc`'s JSON shape. Fields are optional where
//! niri may omit them so that a niri upgrade adding or dropping a field
//! degrades instead of failing the whole capture.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

impl Timestamp {
    pub fn as_nanos(&self) -> i128 {
        self.secs as i128 * 1_000_000_000 + self.nanos as i128
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WindowLayout {
    /// (column index, tile index in column), both 1-based. None for floats.
    #[serde(default)]
    pub pos_in_scrolling_layout: Option<(usize, usize)>,
    /// Tile size including niri's decorations.
    #[serde(default)]
    pub tile_size: (f64, f64),
    /// Window size excluding niri's decorations.
    #[serde(default)]
    pub window_size: (f64, f64),
    /// Tile position within the workspace view. niri 25.11 only populates this
    /// for floating windows; it is null for every tiled window.
    #[serde(default)]
    pub tile_pos_in_workspace_view: Option<(f64, f64)>,
    #[serde(default)]
    pub window_offset_in_tile: (f64, f64),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    /// Not used for layout, kept because it is the only stable identifier when
    /// a window has no title.
    #[allow(dead_code)]
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<u64>,
    /// Compositor wide, so it cannot identify the active tab of an unfocused
    /// column. `focus_timestamp` is what the layout uses instead.
    #[allow(dead_code)]
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub is_floating: bool,
    #[serde(default)]
    pub layout: WindowLayout,
    #[serde(default)]
    pub focus_timestamp: Option<Timestamp>,
}

impl Window {
    /// Monotonic focus recency. Never-focused windows sort oldest.
    pub fn focus_nanos(&self) -> i128 {
        self.focus_timestamp.map(|t| t.as_nanos()).unwrap_or(i128::MIN)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: u64,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    /// The window displayed on top of this workspace, when there is one.
    #[serde(default)]
    pub active_window_id: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Logical {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    #[serde(default)]
    pub transform: Option<String>,
}

impl Logical {
    /// sshot samples the capture in unrotated output space, so a rotated
    /// output would need a different sampling path entirely.
    pub fn is_normal_transform(&self) -> bool {
        matches!(self.transform.as_deref(), None | Some("Normal") | Some("normal"))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Output {
    #[serde(default)]
    pub logical: Option<Logical>,
}

#[derive(Debug, Clone, Deserialize)]
struct OverviewState {
    #[serde(default)]
    is_open: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub windows: Vec<Window>,
    pub workspaces: Vec<Workspace>,
    pub outputs: HashMap<String, Output>,
    /// niri's zoomed-out overview. While it is open the composited image does
    /// not match the reconstructed layout at all.
    pub overview_open: bool,
}

impl Snapshot {
    /// The active workspace on a given output, if any.
    pub fn active_workspace_on(&self, output: &str) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|w| w.is_active && w.output.as_deref() == Some(output))
    }
}

/// Read `layout { gaps N }` out of niri's own config.
///
/// The tiling gap is niri's setting, not sshot's. Duplicating it in sshot's
/// config made two values that had to be kept in sync by hand, with nothing to
/// notice when they drifted, so read the real one.
pub fn config_gap() -> Option<f64> {
    let mut candidates = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        candidates.push(std::path::PathBuf::from(xdg).join("niri/config.kdl"));
    }
    if let Some(dir) = dirs::config_dir() {
        candidates.push(dir.join("niri/config.kdl"));
    }
    candidates.push(std::path::PathBuf::from("/etc/niri/config.kdl"));

    candidates
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|text| parse_gaps(&text))
}

/// Extract the `gaps` value from the top level `layout` node of a KDL config.
fn parse_gaps(text: &str) -> Option<f64> {
    let mut depth: usize = 0;
    // Depth at which a `gaps` node would be a direct child of `layout`.
    let mut layout_depth: Option<usize> = None;

    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() { continue; }

        if layout_depth == Some(depth) {
            if let Some(rest) = line.strip_prefix("gaps") {
                let tok = rest.trim().trim_end_matches(';').trim();
                if let Ok(v) = tok.parse::<f64>() {
                    return Some(v);
                }
            }
        }

        let opens = line.matches('{').count();
        let closes = line.matches('}').count();

        if layout_depth.is_none() && depth == 0 && line.starts_with("layout") && opens > 0 {
            layout_depth = Some(depth + 1);
        }

        depth += opens;
        depth = depth.saturating_sub(closes);

        if let Some(ld) = layout_depth {
            if depth < ld { layout_depth = None; }
        }
    }
    None
}

fn spawn_msg(kind: &str) -> Result<Child> {
    Command::new("niri")
        .args(["msg", "--json", kind])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `niri msg {kind}` (is niri running?)"))
}

fn collect<T: for<'de> Deserialize<'de>>(child: Child, kind: &str) -> Result<T> {
    let out = child
        .wait_with_output()
        .with_context(|| format!("failed to read `niri msg {kind}`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "niri msg {kind} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout)
        .with_context(|| format!("failed to parse `niri msg {kind}` output"))
}

/// Take one consistent-enough view of niri's state.
///
/// The four queries are spawned together and then collected, so the cost is one
/// process round trip rather than four sequential ones.
pub fn snapshot() -> Result<Snapshot> {
    let windows = spawn_msg("windows")?;
    let workspaces = spawn_msg("workspaces")?;
    let outputs = spawn_msg("outputs")?;
    // Added in a later niri than the minimum this supports, so a failure here
    // is not fatal: assume the overview is closed.
    let overview = spawn_msg("overview-state").ok();

    let windows: Vec<Window> = collect(windows, "windows")?;
    let workspaces: Vec<Workspace> = collect(workspaces, "workspaces")?;
    let outputs: HashMap<String, Output> = collect(outputs, "outputs")?;
    let overview_open = overview
        .and_then(|c| collect::<OverviewState>(c, "overview-state").ok())
        .map(|s| s.is_open)
        .unwrap_or(false);

    Ok(Snapshot { windows, workspaces, outputs, overview_open })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tiled_window_with_null_tile_pos() {
        let json = r#"{
            "id": 4, "title": "zed", "app_id": "dev.zed.Zed", "pid": 1,
            "workspace_id": 1, "is_focused": false, "is_floating": false,
            "is_urgent": false,
            "layout": {
                "pos_in_scrolling_layout": [1, 2],
                "tile_size": [2544.0, 1388.0],
                "window_size": [2544, 1388],
                "tile_pos_in_workspace_view": null,
                "window_offset_in_tile": [0.0, 0.0]
            },
            "focus_timestamp": {"secs": 26848, "nanos": 266324036}
        }"#;
        let w: Window = serde_json::from_str(json).unwrap();
        assert_eq!(w.id, 4);
        assert_eq!(w.layout.pos_in_scrolling_layout, Some((1, 2)));
        assert_eq!(w.layout.tile_size, (2544.0, 1388.0));
        assert!(w.layout.tile_pos_in_workspace_view.is_none());
        assert_eq!(w.focus_nanos(), 26848 * 1_000_000_000 + 266324036);
    }

    #[test]
    fn unknown_fields_and_missing_optionals_are_tolerated() {
        let w: Window = serde_json::from_str(
            r#"{"id": 1, "layout": {}, "some_future_field": 42}"#,
        )
        .unwrap();
        assert_eq!(w.id, 1);
        assert_eq!(w.focus_nanos(), i128::MIN);
        assert!(!w.is_floating);
    }

    #[test]
    fn parses_gaps_from_the_layout_block() {
        let cfg = r#"
input { keyboard { xkb { layout "us" } } }

layout {
    gaps 8
    preset-column-widths {
        proportion 0.333
    }
    focus-ring {
        width 2
    }
}
"#;
        assert_eq!(parse_gaps(cfg), Some(8.0));
    }

    #[test]
    fn ignores_gaps_outside_the_layout_block() {
        // `gaps` inside a nested node, and a commented out one, must not win.
        let cfg = r#"
layout {
    // gaps 99
    focus-ring {
        gaps 77
    }
    gaps 12
}
"#;
        assert_eq!(parse_gaps(cfg), Some(12.0));
    }

    #[test]
    fn missing_layout_block_yields_nothing() {
        assert_eq!(parse_gaps("input { }\n"), None);
        assert_eq!(parse_gaps(""), None);
    }

    #[test]
    fn transform_defaults_to_normal() {
        let l: Logical = serde_json::from_str(
            r#"{"x":0,"y":0,"width":2560,"height":1440,"scale":1.5}"#,
        )
        .unwrap();
        assert!(l.is_normal_transform());
        let r: Logical = serde_json::from_str(
            r#"{"x":0,"y":0,"width":2560,"height":1440,"scale":1.5,"transform":"90"}"#,
        )
        .unwrap();
        assert!(!r.is_normal_transform());
    }
}
