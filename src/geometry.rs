//! Reconstruct on-screen window rectangles from niri's IPC state.
//!
//! niri's IPC gives exact window and tile *sizes* and their order in the
//! scrolling layout, but it does not expose two things this needs:
//!
//!  * where the working area starts vertically (bars and docks take an
//!    exclusive zone that no IPC field reports), and
//!  * how far the scrolling layout is scrolled horizontally.
//!
//! Both are a single scalar per output. Rather than guess them from the
//! compositor's configuration (the old code assumed `center-focused-column
//! "always"`, which is not niri's default, and scanned pixel brightness for the
//! bar height, which a fullscreen window defeats), both are recovered from the
//! frozen capture with a matched filter: the layout predicts exactly where tile
//! edges must fall for a given offset, so the offset that lines those
//! predictions up with real edges in the image is the right one.
//!
//! Everything except that recovery step is pure arithmetic and unit tested.

use std::collections::BTreeMap;

use crate::niri::{Snapshot, Window};

/// A window rectangle in logical desktop coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub title: String,
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Maps logical desktop coordinates onto the composited capture.
///
/// grim lays every output out at its logical position scaled by the largest
/// scale in use, with the bounding box origin at pixel (0, 0).
#[derive(Debug, Clone, Copy)]
pub struct CaptureSpace {
    pub min_x: f64,
    pub min_y: f64,
    pub scale: f64,
}

impl CaptureSpace {
    pub fn from_snapshot(snap: &Snapshot) -> Self {
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut scale: f64 = 1.0;
        for out in snap.outputs.values() {
            if let Some(l) = &out.logical {
                min_x = min_x.min(l.x);
                min_y = min_y.min(l.y);
                scale = scale.max(l.scale);
            }
        }
        if !min_x.is_finite() { min_x = 0.0; }
        if !min_y.is_finite() { min_y = 0.0; }
        Self { min_x, min_y, scale }
    }

    pub fn to_phys_x(self, logical_x: f64) -> f64 { (logical_x - self.min_x) * self.scale }
    pub fn to_phys_y(self, logical_y: f64) -> f64 { (logical_y - self.min_y) * self.scale }
}

/// The frozen capture, used only to measure edge strength.
pub struct Capture<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Capture<'_> {
    #[inline]
    fn lum(&self, x: u32, y: u32) -> Option<i32> {
        if x >= self.width || y >= self.height { return None; }
        let i = (y * self.stride + x * 4) as usize;
        let p = self.data.get(i..i + 3)?;
        Some(p[0] as i32 + p[1] as i32 + p[2] as i32)
    }

    /// Mean absolute difference between physical row `y` and the row above it,
    /// sampled across `[x0, x1)`. A tile's top or bottom edge shows up as a
    /// local maximum.
    ///
    /// Measured against the previous row, matching `col_edge`, so an edge is
    /// reported at the first row of the new region rather than the last row of
    /// the old one.
    fn row_edge(&self, y: u32, x0: u32, x1: u32) -> f64 {
        if x1 <= x0 || y == 0 { return 0.0; }
        let step = ((x1 - x0) / SAMPLES).max(1);
        let mut diffs = Vec::with_capacity(SAMPLES as usize + 1);
        let mut x = x0;
        while x < x1 {
            if let (Some(a), Some(b)) = (self.lum(x, y), self.lum(x, y - 1)) {
                diffs.push((a - b).unsigned_abs());
            }
            x += step;
        }
        low_quantile(&mut diffs)
    }

    /// Mean absolute difference between physical column `x` and the column to
    /// its left, sampled down `[y0, y1)`.
    fn col_edge(&self, x: u32, y0: u32, y1: u32) -> f64 {
        if y1 <= y0 || x == 0 { return 0.0; }
        let step = ((y1 - y0) / SAMPLES).max(1);
        let mut diffs = Vec::with_capacity(SAMPLES as usize + 1);
        let mut y = y0;
        while y < y1 {
            if let (Some(a), Some(b)) = (self.lum(x, y), self.lum(x - 1, y)) {
                diffs.push((a - b).unsigned_abs());
            }
            y += step;
        }
        low_quantile(&mut diffs)
    }
}

/// How many points to sample along an edge.
const SAMPLES: u32 = 96;

/// A low quantile of the sampled differences.
///
/// A tile boundary differs from its neighbour along nearly its whole length,
/// while content inside a window (a toolbar, a text run, a panel divider)
/// differs only over part of it. Taking a low quantile rather than the mean
/// therefore scores full length edges far above local ones, which is what
/// separates a real tile boundary from whatever happens to be drawn nearby.
fn low_quantile(diffs: &mut [u32]) -> f64 {
    if diffs.is_empty() { return 0.0; }
    diffs.sort_unstable();
    diffs[diffs.len() / 4] as f64
}

/// One column of the scrolling layout, after tab detection.
#[derive(Debug, Clone)]
struct Column<'a> {
    width: f64,
    /// Tiles in row order.
    tiles: Vec<&'a Window>,
    /// A tabbed column overlays its tiles, so only one is ever on screen.
    tabbed: bool,
    /// Vertical extent actually occupied on screen.
    content_h: f64,
}

impl<'a> Column<'a> {
    /// The tile the compositor is displaying.
    ///
    /// niri exposes no per-column tab state (`is_active_in_column` is a window
    /// rule matcher only, and `active_window_id` is one value for the whole
    /// workspace, so it cannot speak for an unfocused column). The most
    /// recently focused tile in the column is the one on top.
    fn visible_tile(&self, active_window_id: Option<u64>) -> Option<&'a Window> {
        if !self.tabbed {
            return self.tiles.first().copied();
        }
        if let Some(id) = active_window_id {
            if let Some(w) = self.tiles.iter().find(|w| w.id == id) {
                return Some(w);
            }
        }
        self.tiles.iter().copied().max_by_key(|w| w.focus_nanos())
    }
}

fn build_columns<'a>(tiled: &[&'a Window], gap: f64, output_h: f64) -> Vec<Column<'a>> {
    let mut grouped: BTreeMap<usize, Vec<&'a Window>> = BTreeMap::new();
    for w in tiled {
        let col = w.layout.pos_in_scrolling_layout.map(|p| p.0).unwrap_or(0);
        grouped.entry(col).or_default().push(w);
    }

    grouped
        .into_values()
        .map(|mut tiles| {
            tiles.sort_by_key(|w| w.layout.pos_in_scrolling_layout.map(|p| p.1).unwrap_or(0));
            let width = tiles.iter().map(|w| w.layout.tile_size.0).fold(0.0f64, f64::max);
            let heights: Vec<f64> = tiles.iter().map(|w| w.layout.tile_size.1).collect();
            let stacked_h = heights.iter().sum::<f64>() + gap * (heights.len() as f64 - 1.0);

            // niri sizes a stacked column to fit the working area. A column
            // whose tiles cannot fit stacked is displaying them as tabs, all
            // occupying the same rect.
            let tabbed = tiles.len() > 1 && stacked_h > output_h + 0.5;
            let content_h = if tabbed {
                heights.iter().cloned().fold(0.0f64, f64::max)
            } else {
                stacked_h
            };

            Column { width, tiles, tabbed, content_h }
        })
        .collect()
}

/// An edge strength profile over a physical pixel range, already widened so a
/// lookup tolerates the few pixels of focus ring and the rounding between
/// logical and physical coordinates.
struct Profile {
    start: i64,
    values: Vec<f64>,
}

impl Profile {
    fn build(start: i64, len: usize, tol: usize, mut measure: impl FnMut(u32) -> f64) -> Self {
        let raw: Vec<f64> = (0..len)
            .map(|i| {
                let v = start + i as i64;
                if v < 0 { 0.0 } else { measure(v as u32) }
            })
            .collect();
        // Widen each sample to the strongest value within `tol`.
        let values = (0..len)
            .map(|i| {
                let lo = i.saturating_sub(tol);
                let hi = (i + tol + 1).min(len);
                raw[lo..hi].iter().cloned().fold(0.0, f64::max)
            })
            .collect();
        Self { start, values }
    }

    fn at(&self, pos: f64) -> f64 {
        let i = pos.round() as i64 - self.start;
        if i < 0 { return 0.0; }
        self.values.get(i as usize).copied().unwrap_or(0.0)
    }

    /// Strength above which a sample counts as a real boundary.
    ///
    /// Tile boundaries are rare (a handful out of thousands of columns), so a
    /// high percentile of the profile sits at genuine edge strength whatever
    /// the wallpaper and theme happen to be.
    fn hit_threshold(&self) -> f64 {
        let mut sorted: Vec<f64> = self.values.iter().copied().filter(|v| *v > 0.0).collect();
        if sorted.is_empty() { return f64::INFINITY; }
        sorted.sort_by(f64::total_cmp);
        let p99 = sorted[sorted.len() * 99 / 100];
        (0.3 * p99).max(10.0)
    }
}

/// Combine tap strengths so that every tap has to be present.
///
/// A plain sum lets one very strong spurious edge outvote several correct but
/// modest ones. Summing logs approximates a product while tolerating a tap
/// that legitimately measures zero.
fn combine(taps: impl Iterator<Item = f64>) -> f64 {
    taps.map(|v| (1.0 + v).ln()).sum()
}

/// Pick the offset in `0..=max_off` physical pixels whose predicted edges line
/// up best with real edges in the capture.
///
/// `exclusion` is how many neighbouring offsets belong to the same peak.
/// Adjacent offsets necessarily score alike, so the winner is compared against
/// the best *rival* peak rather than against its own shoulder.
///
/// Returns `None` when no peak stands out, which means the image does not
/// actually support an answer and a caller should not invent one.
fn best_offset(max_off: u32, exclusion: u32, margin: f64, floor: f64, score: impl Fn(u32) -> f64) -> Option<u32> {
    if max_off == 0 { return Some(0); }
    let scores: Vec<f64> = (0..=max_off).map(score).collect();

    let (best_i, best_v) = scores
        .iter()
        .enumerate()
        .fold((0usize, f64::NEG_INFINITY), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });

    // Reject only when nothing matched at all. This has to stay well below the
    // score of a single hit: set too high it discards partial but correct
    // matches, which is indistinguishable from "no edges found".
    if best_v <= floor { return None; }

    // Widening the profile turns each peak into a plateau, so the true edge
    // sits at the middle of the winning run, not at its first index.
    let threshold = best_v * 0.999;
    let mut lo = best_i;
    while lo > 0 && scores[lo - 1] >= threshold { lo -= 1; }
    let mut hi = best_i;
    while hi + 1 < scores.len() && scores[hi + 1] >= threshold { hi += 1; }

    let rival = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| *i + (exclusion as usize) < lo || *i > hi + exclusion as usize)
        .map(|(_, &v)| v)
        .fold(f64::NEG_INFINITY, f64::max);

    if rival.is_finite() && best_v < rival * margin {
        return None;
    }
    Some(((lo + hi) / 2) as u32)
}

/// Reconstruct every on-screen window rectangle.
///
/// `cap` is the frozen capture. Without it the horizontal offset cannot be
/// recovered for a scrolled workspace, and that workspace's tiled windows are
/// omitted rather than reported at a guessed position.
pub fn layout_windows(snap: &Snapshot, gap: f64, cap: Option<&Capture>) -> Vec<WindowInfo> {
    // The overview zooms the whole layout out, so nothing reconstructed here
    // would match the pixels the user is looking at.
    if snap.overview_open {
        return Vec::new();
    }

    let space = CaptureSpace::from_snapshot(snap);
    let mut result = Vec::new();

    for (name, out) in &snap.outputs {
        let Some(l) = &out.logical else { continue };
        if !l.is_normal_transform() {
            eprintln!("sshot: output {name} has a non-normal transform, skipping window detection");
            continue;
        }
        let Some(ws) = snap.active_workspace_on(name) else { continue };

        let wins: Vec<&Window> = snap
            .windows
            .iter()
            .filter(|w| w.workspace_id == Some(ws.id))
            .collect();
        if wins.is_empty() { continue; }

        let tiled: Vec<&Window> = wins.iter().copied().filter(|w| !w.is_floating).collect();
        let columns = build_columns(&tiled, gap, l.height);
        let content_max = columns.iter().map(|c| c.content_h).fold(0.0f64, f64::max);

        // Resolving the horizontal layout also measures the real tiling gap,
        // which the vertical stacking below then uses.
        let mut column_x: Vec<f64> = Vec::with_capacity(columns.len());
        let mut tile_top = l.y;
        let mut resolved_gap = gap;

        if !columns.is_empty() {
            // A first estimate of the tile top, good enough to pick the rows
            // that the column edge measurement samples over.
            let rough_top = resolve_tile_top(cap, &space, l, &columns, content_max, l.x, gap);
            // Which column the compositor is keeping in view.
            let focused_col = ws.active_window_id.and_then(|id| {
                columns.iter().position(|c| c.tiles.iter().any(|w| w.id == id))
            });
            match resolve_horizontal(
                cap, &space, l, &columns, gap, rough_top, content_max, focused_col,
            ) {
                Some(h) => {
                    resolved_gap = h.gap;
                    // Recompute the top now that the columns are placed, so the
                    // sample window covers the tiles and not the wallpaper.
                    tile_top = resolve_tile_top(cap, &space, l, &columns, content_max, h.x0, h.gap);
                    let mut x = h.x0;
                    for c in &columns {
                        column_x.push(x);
                        x += c.width + h.gap;
                    }
                }
                None => {
                    // A scrolled workspace whose offset could not be measured.
                    // Reporting guessed rectangles here is what made every
                    // highlight wrong after scrolling, so report none.
                    eprintln!(
                        "sshot: could not determine scroll position on {name}, \
                         window selection disabled for that output"
                    );
                }
            }
        }

        // ── Emit tiles ───────────────────────────────────────────────
        for (col, &x) in columns.iter().zip(column_x.iter()) {
            if col.tabbed {
                // Only the displayed tab exists on screen. The others used to
                // be emitted stacked below it, which both mislabelled the
                // visible rect and left phantom rects near the screen edge.
                if let Some(w) = col.visible_tile(ws.active_window_id) {
                    push_window(&mut result, w, x, tile_top, l);
                }
            } else {
                let mut y = tile_top;
                for w in &col.tiles {
                    push_window(&mut result, w, x, y, l);
                    y += w.layout.tile_size.1 + resolved_gap;
                }
            }
        }

        // ── Emit floats last ─────────────────────────────────────────
        // niri composites floats above tiles. Hit testing scans this list in
        // reverse, so floats have to come after tiles or a float over a tile
        // can never be picked. Oldest first so the newest float wins.
        let mut floats: Vec<&Window> = wins.iter().copied().filter(|w| w.is_floating).collect();
        floats.sort_by_key(|w| w.focus_nanos());
        for w in floats {
            let Some(pos) = w.layout.tile_pos_in_workspace_view else { continue };
            let ox = l.x + pos.0 + w.layout.window_offset_in_tile.0;
            let oy = tile_top - resolved_gap.min(tile_top - l.y) + pos.1
                + w.layout.window_offset_in_tile.1;
            result.push(WindowInfo {
                title: w.title.clone().unwrap_or_default(),
                id: w.id.to_string(),
                x: ox,
                y: oy,
                width: w.layout.window_size.0,
                height: w.layout.window_size.1,
            });
        }
    }

    result
}

fn push_window(out: &mut Vec<WindowInfo>, w: &Window, tile_x: f64, tile_y: f64, l: &crate::niri::Logical) {
    let x = tile_x + w.layout.window_offset_in_tile.0;
    let y = tile_y + w.layout.window_offset_in_tile.1;
    let (width, height) = w.layout.window_size;
    // Cull anything scrolled off this output.
    if x + width <= l.x || x >= l.x + l.width || y + height <= l.y || y >= l.y + l.height {
        return;
    }
    out.push(WindowInfo {
        title: w.title.clone().unwrap_or_default(),
        id: w.id.to_string(),
        x,
        y,
        width,
        height,
    });
}

/// Where a workspace's columns actually sit.
struct Horizontal {
    /// Left edge of the first column, in logical coordinates.
    x0: f64,
    /// Spacing between adjacent columns, in logical pixels.
    gap: f64,
}

/// Recover the horizontal layout: the first column's position and the gap.
///
/// The gap is measured rather than taken from config. niri's `gaps` and
/// sshot's `tiling_gap` were two independent settings that had to be kept in
/// sync by hand, and nothing detected it when they drifted apart.
///
/// Returns `None` when the workspace is scrolled and the image cannot say by
/// how much, because a guessed offset moves every rectangle on the output.
#[allow(clippy::too_many_arguments)]
fn resolve_horizontal(
    cap: Option<&Capture>,
    space: &CaptureSpace,
    l: &crate::niri::Logical,
    columns: &[Column],
    fallback_gap: f64,
    tile_top: f64,
    content_max: f64,
    focused_col: Option<usize>,
) -> Option<Horizontal> {
    let widths: Vec<f64> = columns.iter().map(|c| c.width).collect();
    let single = widths.len() < 2;

    let total_for = |gap: f64| widths.iter().sum::<f64>() + gap * (widths.len() as f64 - 1.0);

    let Some(cap) = cap else {
        let total_w = total_for(fallback_gap);
        let slack = (l.width - total_w).max(0.0);
        // Nothing to measure against. Only a layout that fits can be placed
        // by arithmetic alone.
        return if total_w <= l.width + 0.5 {
            Some(Horizontal { x0: l.x + fallback_gap.min(slack / 2.0), gap: fallback_gap })
        } else {
            None
        };
    };

    let gap = fallback_gap;
    let total_w = total_for(gap);
    let fits = total_w <= l.width + 0.5;
    let slack = (l.width - total_w).max(0.0);
    let arithmetic = Horizontal { x0: l.x + gap.min(slack / 2.0), gap };

    // Everything fits and a single column cannot be scrolled, so arithmetic
    // already has the answer and there is nothing worth measuring.
    if fits && single {
        return Some(arithmetic);
    }

    let tol = space.scale.ceil() as usize + 1;
    let x_start = space.to_phys_x(l.x).round() as i64;
    let x_len = (l.width * space.scale).round() as usize + 1;
    let y0 = space.to_phys_y(tile_top).max(0.0) as u32;
    let y1 = space.to_phys_y(tile_top + content_max) as u32;
    let profile = Profile::build(x_start, x_len, tol, |x| cap.col_edge(x, y0, y1));

    // Edges the layout predicts, relative to the first column's left edge.
    let mut rel = Vec::with_capacity(widths.len() * 2);
    let mut x = 0.0;
    for w in &widths {
        rel.push(x);
        rel.push(x + w);
        x += w + gap;
    }

    // Legal positions for the first column.
    //
    // When the layout overflows it is NOT necessarily scrolled: the columns can
    // still sit one gap in from the left with the overflow hanging off the
    // right. Clamping the range to `l.x` assumed the first column always
    // starts at or before the left edge, which put the unscrolled position
    // outside the search entirely, so it could never be found.
    let (full_lo, full_hi) = if fits {
        (l.x, l.x + slack)
    } else {
        (l.x + l.width - total_w - gap, l.x + gap)
    };

    // Score the mean over the edges that are actually inside the output.
    //
    // A scrolled layout puts its outermost edges past the screen, where they
    // cannot be measured. Summing over all predicted edges scored those
    // absences as misses, so the correct offset lost to a rival purely because
    // more of its layout was off screen, and the resolver gave up exactly at
    // the extremes of the scroll range.
    // Score by what fraction of the predicted boundaries actually exist, with
    // strength only as a tiebreak.
    //
    // Ranking by strength alone lets a single bright edge inside a window (a
    // side panel divider, say) outweigh two predictions that landed on
    // nothing, so a hypothesis with more dead predictions could still win.
    // What matters is whether each boundary the layout requires is really
    // there.
    let threshold = profile.hit_threshold();
    let score_at = |x0: f64| {
        let mut hits = 0u32;
        let mut strength = 0.0;
        let mut seen = 0u32;
        for r in &rel {
            let lx = x0 + r;
            // An edge needs a pixel on each side to be measurable, so one
            // sitting on the output boundary is unknowable, not a miss.
            if lx < l.x + 1.0 || lx > l.x + l.width - 1.0 { continue; }
            let v = profile.at(space.to_phys_x(lx));
            if v >= threshold { hits += 1; }
            strength += (1.0 + v).ln();
            seen += 1;
        }
        if seen < 2 { return 0.0; }
        // Primary: what fraction of the required boundaries exist. Then, when
        // two hypotheses both explain everything they can see, prefer the one
        // that explains more boundaries rather than the one that happens to
        // have fewer of them on screen. Strength only breaks remaining ties.
        hits as f64 / seen as f64 + 0.08 * hits as f64 + 0.01 * (strength / seen as f64)
    };

    let search = |lo: f64, hi: f64| -> Option<f64> {
        let max_off = ((hi - lo) * space.scale).round().max(0.0) as u32;
        best_offset(max_off, tol as u32 * 2, 1.06, 0.15, |off| {
            score_at(lo + off as f64 / space.scale)
        })
        .map(|off| lo + off as f64 / space.scale)
    };

    // niri keeps the focused column fully in view, which pins the offset to a
    // far narrower range than "anywhere it could be scrolled to". Searching
    // that range first removes rival alignments the compositor would never
    // actually scroll to, which is what made the resolver refuse to choose.
    let constrained = focused_col.and_then(|k| {
        let rel_k = rel[k * 2];
        let w_k = columns[k].width;
        let need_lo = l.x - rel_k;
        let need_hi = l.x + l.width - rel_k - w_k;
        // A column wider than the view cannot be fully contained.
        if need_hi < need_lo { return None; }
        let lo = full_lo.max(need_lo);
        let hi = full_hi.min(need_hi);
        if hi < lo { return None; }
        search(lo, hi)
    });

    // Fall back to the whole range if the constraint yielded nothing, so a
    // stale or unusual active window cannot make things worse than not having
    // the hint at all.
    let found = constrained.or_else(|| search(full_lo, full_hi));

    match found {
        Some(x0) => Some(Horizontal { x0, gap }),
        // No clear answer. Arithmetic is right whenever nothing is scrolled
        // off; when something is, a guess would move every rectangle.
        None if fits => Some(arithmetic),
        None => None,
    }
}

/// Recover the y of the first tile row.
///
/// Searched directly rather than as "inset plus gap" so the answer does not
/// depend on the gap being known.
fn resolve_tile_top(
    cap: Option<&Capture>,
    space: &CaptureSpace,
    l: &crate::niri::Logical,
    columns: &[Column],
    content_max: f64,
    x0: f64,
    fallback_gap: f64,
) -> f64 {
    let slack = (l.height - content_max).max(0.0);
    // No room above or below: a fullscreen tile covers the whole output.
    if slack <= 0.5 { return l.y; }
    // Nothing to measure: assume no top bar, which puts the tile one gap down.
    let Some(cap) = cap else { return l.y + fallback_gap.min(slack / 2.0) };

    // Sample across the widest column, which is most likely to have a clean
    // horizontal edge at both ends.
    let widest = columns.iter().map(|c| c.width).fold(0.0f64, f64::max);
    let sx0 = space.to_phys_x(x0).max(0.0) as u32;
    let sx1 = space.to_phys_x(x0 + widest.min(l.width)) as u32;

    let tol = space.scale.ceil() as usize + 1;
    let y_start = space.to_phys_y(l.y).round() as i64;
    let y_len = (l.height * space.scale).round() as usize + 1;
    let profile = Profile::build(y_start, y_len, tol, |y| cap.row_edge(y, sx0, sx1));

    let base = space.to_phys_y(l.y);
    let height_p = content_max * space.scale;
    let max_off = (slack * space.scale).round() as u32;

    // Two taps: the top of the first tile and the bottom of the last. Their
    // separation is known exactly, so a spurious edge only wins if a second
    // spurious edge sits exactly the right distance away.
    let score = |off: u32| {
        let top = base + off as f64;
        combine([profile.at(top), profile.at(top + height_p)].into_iter())
    };

    // With no top bar the tile sits one gap down, which is by far the most
    // common arrangement. Only displace it when the image says so clearly,
    // otherwise a strong edge inside a window (a toolbar, a tab strip) can
    // drag every rectangle on the output downwards.
    let default_off = (fallback_gap.min(slack / 2.0) * space.scale).round() as u32;
    let default_score = score(default_off.min(max_off));

    match best_offset(max_off, tol as u32 * 2, 1.06, 1.0, score) {
        Some(off) if score(off) > default_score * 1.3 => l.y + off as f64 / space.scale,
        _ => l.y + default_off as f64 / space.scale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::niri::{Logical, Output, Workspace};

    fn win(id: u64, ws: u64, col: usize, row: usize, w: f64, h: f64, focus: i64) -> Window {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": format!("win{id}"),
            "workspace_id": ws,
            "is_focused": false,
            "is_floating": false,
            "layout": {
                "pos_in_scrolling_layout": [col, row],
                "tile_size": [w, h],
                "window_size": [w, h],
                "tile_pos_in_workspace_view": null,
                "window_offset_in_tile": [0.0, 0.0]
            },
            "focus_timestamp": {"secs": focus, "nanos": 0}
        }))
        .unwrap()
    }

    fn snap(windows: Vec<Window>) -> Snapshot {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "DP-1".to_string(),
            Output {
                logical: Some(Logical {
                    x: 0.0,
                    y: 0.0,
                    width: 2560.0,
                    height: 1440.0,
                    scale: 1.5,
                    transform: None,
                }),
            },
        );
        Snapshot {
            windows,
            workspaces: vec![Workspace {
                id: 1,
                output: Some("DP-1".into()),
                is_active: true,
                active_window_id: None,
            }],
            outputs,
            overview_open: false,
        }
    }

    /// The reported bug: a tabbed column reported the first tile, not the
    /// visible one.
    #[test]
    fn tabbed_column_reports_only_the_visible_tab() {
        // Two full-height tiles in one column cannot be stacked in 1440.
        let s = snap(vec![
            win(3, 1, 1, 1, 2544.0, 1388.0, 100),
            win(4, 1, 1, 2, 2544.0, 1388.0, 200), // focused more recently
        ]);
        let out = layout_windows(&s, 8.0, None);
        assert_eq!(out.len(), 1, "only the displayed tab is on screen");
        assert_eq!(out[0].id, "4");
    }

    #[test]
    fn tabbed_column_prefers_the_workspace_active_window() {
        let mut s = snap(vec![
            win(3, 1, 1, 1, 2544.0, 1388.0, 100),
            win(4, 1, 1, 2, 2544.0, 1388.0, 200),
        ]);
        s.workspaces[0].active_window_id = Some(3);
        let out = layout_windows(&s, 8.0, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "3");
    }

    /// The phantom rect: hidden tabs used to be emitted below the visible one
    /// and still passed the on-screen test near the bottom edge.
    #[test]
    fn tabbed_column_emits_no_phantom_rect_near_the_bottom() {
        let s = snap(vec![
            win(3, 1, 1, 1, 2544.0, 1388.0, 100),
            win(4, 1, 1, 2, 2544.0, 1388.0, 200),
        ]);
        let out = layout_windows(&s, 8.0, None);
        assert!(out.iter().all(|w| w.y + w.height <= 1440.0));
    }

    #[test]
    fn stacked_column_still_stacks() {
        // Two half-height tiles do fit stacked, so both are on screen.
        let s = snap(vec![
            win(1, 1, 1, 1, 2544.0, 690.0, 100),
            win(2, 1, 1, 2, 2544.0, 690.0, 200),
        ]);
        let out = layout_windows(&s, 8.0, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "1");
        assert!(out[1].y > out[0].y, "second tile sits below the first");
        assert_eq!(out[1].y - (out[0].y + out[0].height), 8.0, "one gap between tiles");
    }

    #[test]
    fn columns_that_fit_are_left_aligned_one_gap_in() {
        let s = snap(vec![win(1, 1, 1, 1, 2544.0, 1388.0, 100)]);
        let out = layout_windows(&s, 8.0, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].x, 8.0);
        assert_eq!(out[0].y, 8.0);
    }

    /// The second reported bug: with no capture to measure, a scrolled
    /// workspace must report nothing rather than guess.
    #[test]
    fn scrolled_workspace_reports_nothing_without_a_capture() {
        let s = snap(vec![
            win(1, 1, 1, 1, 2000.0, 1388.0, 100),
            win(2, 1, 2, 1, 2000.0, 1388.0, 200),
            win(3, 1, 3, 1, 2000.0, 1388.0, 300),
        ]);
        let out = layout_windows(&s, 8.0, None);
        assert!(out.is_empty(), "guessing a scroll offset is worse than nothing");
    }

    #[test]
    fn fullscreen_window_sits_at_the_output_origin() {
        let s = snap(vec![win(1, 1, 1, 1, 2560.0, 1440.0, 100)]);
        let out = layout_windows(&s, 8.0, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].y, 0.0, "a fullscreen tile has no bar above it");
        assert_eq!(out[0].height, 1440.0);
    }

    #[test]
    fn overview_suppresses_everything() {
        let mut s = snap(vec![win(1, 1, 1, 1, 2544.0, 1388.0, 100)]);
        s.overview_open = true;
        assert!(layout_windows(&s, 8.0, None).is_empty());
    }

    #[test]
    fn floats_are_emitted_after_tiles() {
        let mut float = win(9, 1, 1, 1, 800.0, 600.0, 500);
        float.is_floating = true;
        float.layout.pos_in_scrolling_layout = None;
        float.layout.tile_pos_in_workspace_view = Some((100.0, 100.0));
        let s = snap(vec![win(1, 1, 1, 1, 2544.0, 1388.0, 100), float]);
        let out = layout_windows(&s, 8.0, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out.last().unwrap().id, "9", "floats must win the reverse hit scan");
    }

    #[test]
    fn non_normal_transform_is_skipped() {
        let mut s = snap(vec![win(1, 1, 1, 1, 2544.0, 1388.0, 100)]);
        if let Some(o) = s.outputs.get_mut("DP-1") {
            o.logical.as_mut().unwrap().transform = Some("90".into());
        }
        assert!(layout_windows(&s, 8.0, None).is_empty());
    }

    /// Paint an image where each tile is a bright block on a dark background,
    /// so tile boundaries are the only strong edges.
    fn synth_capture(w: u32, h: u32, tiles: &[(i64, i64)], top: i64, bottom: i64) -> Vec<u8> {
        let mut data = vec![0x20u8; (w * h * 4) as usize];
        for &(x0, x1) in tiles {
            for y in top.max(0)..bottom.min(h as i64) {
                for x in x0.max(0)..x1.min(w as i64) {
                    let i = ((y as u32 * w + x as u32) * 4) as usize;
                    data[i..i + 4].copy_from_slice(&[0xC0, 0xC0, 0xC0, 0xFF]);
                }
            }
        }
        data
    }

    fn one_output(x: f64, y: f64, w: f64, h: f64, scale: f64) -> std::collections::HashMap<String, Output> {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "DP-1".to_string(),
            Output { logical: Some(Logical { x, y, width: w, height: h, scale, transform: None }) },
        );
        outputs
    }

    /// The second reported bug, with an image to measure against: a scrolled
    /// workspace must recover its real offset, not assume a centered column.
    #[test]
    fn scrolled_workspace_recovers_its_offset_from_the_capture() {
        // Three 400 wide columns, gap 10, in a 1000 wide output: 1220 total, so
        // 220 is scrolled off. Put the first column at -150.
        let x0 = -150i64;
        let tiles = [(x0, x0 + 400), (x0 + 410, x0 + 810), (x0 + 820, x0 + 1220)];
        let data = synth_capture(1000, 500, &tiles, 10, 460);

        let mut s = snap(vec![
            win(1, 1, 1, 1, 400.0, 450.0, 100),
            win(2, 1, 2, 1, 400.0, 450.0, 200),
            win(3, 1, 3, 1, 400.0, 450.0, 300),
        ]);
        s.outputs = one_output(0.0, 0.0, 1000.0, 500.0, 1.0);

        let cap = Capture { data: &data, width: 1000, height: 500, stride: 4000 };
        let out = layout_windows(&s, 10.0, Some(&cap));

        // The leftmost column is scrolled off screen and culled; the other two
        // must land exactly where they were painted.
        let mut xs: Vec<f64> = out.iter().map(|w| w.x).collect();
        xs.sort_by(f64::total_cmp);
        assert_eq!(xs, vec![-150.0, 260.0, 670.0], "recovered scroll offset");
        assert!(out.iter().all(|w| w.y == 10.0), "tile top recovered: {out:?}");
    }

    /// Sweep every legal scroll position and report which ones the resolver
    /// cannot recover. Scrolling must not be a coin flip.
    #[test]
    fn every_scroll_offset_is_recoverable() {
        let widths = [400.0, 640.0, 400.0, 520.0];
        let gap = 10.0;
        let total: f64 = widths.iter().sum::<f64>() + gap * (widths.len() as f64 - 1.0);
        let (ow, oh) = (1000.0f64, 500.0f64);
        assert!(total > ow, "the point of this test is an overflowing layout");

        let mut failures = Vec::new();
        let mut wrong = Vec::new();
        // Includes the unscrolled position, where the columns sit one gap in
        // from the left and the overflow hangs off the right edge.
        let lo = (ow - total - gap) as i64;
        for x0 in (lo..=gap as i64).step_by(3) {
            let mut tiles = Vec::new();
            let mut x = x0;
            for w in widths {
                tiles.push((x, x + w as i64));
                x += w as i64 + gap as i64;
            }
            let data = synth_capture(1000, 500, &tiles, 10, 460);

            let mut s = snap(
                widths
                    .iter()
                    .enumerate()
                    .map(|(i, &w)| win(i as u64 + 1, 1, i + 1, 1, w, 450.0, 100 + i as i64))
                    .collect(),
            );
            s.outputs = one_output(0.0, 0.0, ow, oh, 1.0);
            // niri only guarantees the focused column is fully in view, so the
            // hint is only set when some column actually is.
            s.workspaces[0].active_window_id = {
                let mut x = x0 as f64;
                let mut pick = None;
                for (i, w) in widths.iter().enumerate() {
                    if x >= 0.0 && x + w <= ow { pick = Some(i as u64 + 1); break; }
                    x += w + gap;
                }
                pick
            };

            let cap = Capture { data: &data, width: 1000, height: 500, stride: 4000 };
            let out = layout_windows(&s, gap, Some(&cap));
            if out.is_empty() {
                failures.push(x0);
                continue;
            }
            let got = out.iter().map(|w| w.x).fold(f64::INFINITY, f64::min);
            // The leftmost emitted window may be culled, so compare against the
            // first column that is actually on screen.
            let expected = {
                let mut x = x0 as f64;
                let mut first = None;
                for w in widths {
                    if x + w > 0.0 && first.is_none() { first = Some(x); }
                    x += w + gap;
                }
                first.unwrap()
            };
            if (got - expected).abs() > 1.5 {
                wrong.push((x0, got, expected));
            }
        }
        assert!(
            failures.is_empty() && wrong.is_empty(),
            "gave up at offsets {failures:?}; misplaced at {wrong:?}"
        );
    }

    /// A top bar displaces every tile downwards; the old brightness scan got
    /// this wrong whenever a fullscreen window removed the wallpaper gap.
    #[test]
    fn top_bar_is_detected_from_the_capture() {
        // Output 1000x500, a 60px bar at the top, one 980x420 tile at y=70.
        let data = synth_capture(1000, 500, &[(10, 990)], 70, 490);
        let mut s = snap(vec![win(1, 1, 1, 1, 980.0, 420.0, 100)]);
        s.outputs = one_output(0.0, 0.0, 1000.0, 500.0, 1.0);

        let cap = Capture { data: &data, width: 1000, height: 500, stride: 4000 };
        let out = layout_windows(&s, 10.0, Some(&cap));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].y, 70.0, "tile sits below the bar, not one gap from the top");
    }

    #[test]
    fn capture_space_uses_the_bounding_box_and_max_scale() {
        let mut s = snap(vec![]);
        s.outputs.insert(
            "DP-2".into(),
            Output {
                logical: Some(Logical {
                    x: -1920.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                    scale: 2.0,
                    transform: None,
                }),
            },
        );
        let space = CaptureSpace::from_snapshot(&s);
        assert_eq!(space.min_x, -1920.0);
        assert_eq!(space.scale, 2.0);
        assert_eq!(space.to_phys_x(-1920.0), 0.0);
        assert_eq!(space.to_phys_x(0.0), 3840.0);
    }
}
