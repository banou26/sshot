use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use crate::config::Config;
use crate::geometry::{self, CaptureSpace, WindowInfo};

/// Pointer travel before a press becomes a region drag rather than a click.
const DRAG_THRESHOLD: f64 = 5.0;
/// Smallest region worth saving. Below this a drag is treated as a click, so a
/// twitch during a window pick cannot save a stray few-pixel image.
const MIN_REGION: f64 = 16.0;

// ── Public types ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Selection {
    Window { title: String, id: String },
    Region { x: f64, y: f64, w: f64, h: f64 },
}

/// Result of a screenshot overlay session.
pub struct CaptureResult {
    /// Raw BGRA pixel data of the full workspace at physical resolution.
    pub ss_data: Vec<u8>,
    pub ss_width: u32,
    pub ss_stride: u32,
    /// What the user selected.
    pub selection: Selection,
    /// How logical coordinates map onto the capture. Carries the bounding box
    /// origin as well as the scale, which a bare scale factor loses: an output
    /// at a negative logical coordinate puts that origin somewhere other than
    /// zero, and every crop is then offset by it.
    pub space: CaptureSpace,
}

// ── Internal types ───────────────────────────────────────────────

/// A buffer plus what is currently painted in it, so a redraw only has to
/// repair the difference instead of repainting the whole output.
struct BufferState {
    buffer: Buffer,
    initialized: bool,
    drawn_hl: Option<Rect>,
}

struct OutputSurface {
    layer: LayerSurface,
    wl_surface: wl_surface::WlSurface,
    viewport: Option<wp_viewport::WpViewport>,
    pool: SlotPool,
    /// Retained so the compositor is never reading a buffer we overwrite.
    buffers: Vec<BufferState>,
    logical_x: i32,
    logical_y: i32,
    phys_w: u32,
    phys_h: u32,
    scale: f64,
    ss_offset_x: u32,
    ss_offset_y: u32,
    dimmed: Vec<u8>,
    configured: bool,
    needs_redraw: bool,
    frame_pending: bool,
    /// Highlight last committed for this output.
    shown_hl: Option<Rect>,
}

// ── Rendering ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Rect {
    fn inflate(self, n: u32) -> Rect {
        Rect { x: self.x - n as i32, y: self.y - n as i32, w: self.w + 2 * n, h: self.h + 2 * n }
    }

    /// Clip to a `w` by `h` buffer, as (x0, y0, x1, y1). None when empty.
    fn clip(self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let x0 = self.x.max(0).min(w as i32) as u32;
        let y0 = self.y.max(0).min(h as i32) as u32;
        let x1 = (self.x as i64 + self.w as i64).clamp(0, w as i64) as u32;
        let y1 = (self.y as i64 + self.h as i64).clamp(0, h as i64) as u32;
        if x0 >= x1 || y0 >= y1 { None } else { Some((x0, y0, x1, y1)) }
    }

    fn union(a: Rect, b: Rect) -> Rect {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        let x2 = (a.x + a.w as i32).max(b.x + b.w as i32);
        let y2 = (a.y + a.h as i32).max(b.y + b.h as i32);
        Rect { x, y, w: (x2 - x).max(0) as u32, h: (y2 - y).max(0) as u32 }
    }
}

/// The region to report as damaged when the highlight changes.
///
/// Wayland keeps whatever was committed before outside the damage, so this has
/// to cover every pixel where the new buffer differs from what is ON SCREEN,
/// which is the last committed highlight and not whatever this buffer happened
/// to hold. Measuring it against the buffer instead left the previous outline
/// standing whenever the new highlight fell inside it, which is exactly what
/// happens when the pointer moves from a tile onto a float sitting on top of
/// it: both outlines showed at once.
fn damage_between(shown: Option<Rect>, next: Option<Rect>, bw: u32) -> Option<Rect> {
    match (shown, next) {
        (None, None) => None,
        (Some(a), None) => Some(a.inflate(bw)),
        (None, Some(b)) => Some(b.inflate(bw)),
        (Some(a), Some(b)) => Some(Rect::union(a.inflate(bw), b.inflate(bw))),
    }
}

/// Precompute the dimmed backdrop for one output.
///
/// Only the dimmed copy is kept. The undimmed pixels are read straight out of
/// the capture when a highlight needs them, so there is no second full
/// resolution copy of the screen per output.
fn precompute_dimmed(
    src: &geometry::Capture,
    ss_offset_x: u32, ss_offset_y: u32,
    phys_w: u32, phys_h: u32, dim_factor: f32,
) -> Vec<u8> {
    let (ss_data, ss_stride, ss_width) = (src.data, src.stride, src.width);
    let mut dimmed = vec![0u8; phys_w as usize * phys_h as usize * 4];
    let ss_h = ss_data.len() as u32 / ss_stride.max(1);
    let lut: [u8; 256] = std::array::from_fn(|i| (i as f32 * dim_factor) as u8);
    let copy_w = phys_w.min(ss_width.saturating_sub(ss_offset_x)) as usize;
    if copy_w == 0 { return dimmed; }

    for py in 0..phys_h {
        let sy = ss_offset_y + py;
        if sy >= ss_h { break; }
        let src = (sy as usize * ss_stride as usize) + ss_offset_x as usize * 4;
        let dst = py as usize * phys_w as usize * 4;
        let Some(src_row) = ss_data.get(src..src + copy_w * 4) else { break };
        let dst_row = &mut dimmed[dst..dst + copy_w * 4];
        for (d, s) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
            d[0] = lut[s[0] as usize];
            d[1] = lut[s[1] as usize];
            d[2] = lut[s[2] as usize];
            d[3] = s[3];
        }
    }
    dimmed
}

/// Copy the dimmed backdrop back over a region.
fn restore_dim(canvas: &mut [u8], dimmed: &[u8], phys_w: u32, r: (u32, u32, u32, u32)) {
    let (x0, y0, x1, y1) = r;
    let row = (x1 - x0) as usize * 4;
    for y in y0..y1 {
        let off = (y as usize * phys_w as usize + x0 as usize) * 4;
        if off + row > canvas.len() || off + row > dimmed.len() { break; }
        canvas[off..off + row].copy_from_slice(&dimmed[off..off + row]);
    }
}

/// Copy undimmed capture pixels into a region.
fn blit_undimmed(
    canvas: &mut [u8], ss_data: &[u8], ss_stride: u32,
    ss_offset_x: u32, ss_offset_y: u32, phys_w: u32,
    r: (u32, u32, u32, u32),
) {
    let (x0, y0, x1, y1) = r;
    let row = (x1 - x0) as usize * 4;
    for y in y0..y1 {
        let src = ((ss_offset_y + y) as usize * ss_stride as usize)
            + (ss_offset_x + x0) as usize * 4;
        let dst = (y as usize * phys_w as usize + x0 as usize) * 4;
        let (Some(s), true) = (ss_data.get(src..src + row), dst + row <= canvas.len()) else {
            continue;
        };
        canvas[dst..dst + row].copy_from_slice(s);
    }
}

fn fill_rect(canvas: &mut [u8], phys_w: u32, phys_h: u32, r: Rect, color: [u8; 4]) {
    let Some((x0, y0, x1, y1)) = r.clip(phys_w, phys_h) else { return };
    for y in y0..y1 {
        let s = (y as usize * phys_w as usize + x0 as usize) * 4;
        let e = (y as usize * phys_w as usize + x1 as usize) * 4;
        if e > canvas.len() { break; }
        for px in canvas[s..e].chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
    }
}

/// Draw a border around `inner` as four strips.
///
/// The previous version swept the whole inflated bounding box and rejected the
/// interior per pixel, touching two orders of magnitude more pixels than it
/// painted.
fn draw_border(canvas: &mut [u8], phys_w: u32, phys_h: u32, inner: Rect, bw: u32, color: [u8; 4]) {
    if bw == 0 { return; }
    let outer = inner.inflate(bw);
    let top = Rect { x: outer.x, y: outer.y, w: outer.w, h: bw };
    let bottom = Rect { x: outer.x, y: inner.y + inner.h as i32, w: outer.w, h: bw };
    let left = Rect { x: outer.x, y: inner.y, w: bw, h: inner.h };
    let right = Rect { x: inner.x + inner.w as i32, y: inner.y, w: bw, h: inner.h };
    for r in [top, bottom, left, right] {
        fill_rect(canvas, phys_w, phys_h, r, color);
    }
}

// ── Wayland App ──────────────────────────────────────────────────

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    viewporter: Option<wp_viewporter::WpViewporter>,
    config: Config,

    ss_data: Vec<u8>,
    ss_width: u32,
    ss_stride: u32,
    windows: Vec<WindowInfo>,
    surfaces: Vec<OutputSurface>,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    mouse_global: (f64, f64),
    pressed: bool,
    dragging: bool,
    press_global: (f64, f64),
    drag_start: (f64, f64),
    drag_end: (f64, f64),
    hovered_window: Option<usize>,

    /// Maps logical coordinates onto the composited capture.
    space: CaptureSpace,
    exit: bool,
    selection: Option<Selection>,
}

impl App {
    fn create_overlays(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.output_state.outputs()
            .map(|o| (o.clone(), self.output_state.info(&o)))
            .collect();

        let dim = self.config.appearance.dim_factor;

        for (output, info) in &outputs {
            let Some(info) = info else { continue };
            let Some((log_w, log_h)) = info.logical_size.map(|(w, h)| (w as u32, h as u32)) else {
                eprintln!("sshot: output has no logical size, skipping");
                continue;
            };
            let (phys_w, phys_h) = info.modes.iter().find(|m| m.current)
                .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
                .unwrap_or((log_w, log_h));
            let (log_x, log_y) = info.logical_position.unwrap_or((0, 0));
            let scale = if log_w > 0 { phys_w as f64 / log_w as f64 } else { 1.0 };

            // Position in the composite is derived from the shared capture
            // space, not from this output's own scale, so a mixed DPI layout
            // still samples the right pixels.
            let ss_offset_x = self.space.to_phys_x(log_x as f64).round().max(0.0) as u32;
            let ss_offset_y = self.space.to_phys_y(log_y as f64).round().max(0.0) as u32;

            let src = geometry::Capture {
                data: &self.ss_data,
                width: self.ss_width,
                height: self.ss_data.len() as u32 / self.ss_stride.max(1),
                stride: self.ss_stride,
            };
            let dimmed = precompute_dimmed(&src, ss_offset_x, ss_offset_y, phys_w, phys_h, dim);

            let surface = self.compositor.create_surface(qh);
            let layer = self.layer_shell.create_layer_surface(
                qh, surface.clone(), Layer::Overlay, Some("screenshot"), Some(output),
            );
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_size(0, 0);
            layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            layer.set_exclusive_zone(-1);
            layer.commit();

            let viewport = self.viewporter.as_ref().map(|vp| {
                let v = vp.get_viewport(&surface, qh, ());
                v.set_destination(log_w as i32, log_h as i32);
                v
            });

            // Two full frames, since two buffers are retained so that one can
            // be painted while the compositor still holds the other.
            let pool_size = (phys_w as usize * phys_h as usize * 4 * 2).max(1);
            let pool = match SlotPool::new(pool_size, &self.shm) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("sshot: could not allocate SHM pool for an output: {e}");
                    continue;
                }
            };

            self.surfaces.push(OutputSurface {
                layer, wl_surface: surface, viewport, pool,
                buffers: Vec::new(),
                logical_x: log_x, logical_y: log_y,
                phys_w, phys_h, scale,
                ss_offset_x, ss_offset_y,
                dimmed, configured: false, needs_redraw: false, frame_pending: false,
                shown_hl: None,
            });
        }
    }

    fn window_at(&self, gx: f64, gy: f64) -> Option<usize> {
        for (i, w) in self.windows.iter().enumerate().rev() {
            if gx >= w.x && gx < w.x + w.width && gy >= w.y && gy < w.y + w.height {
                return Some(i);
            }
        }
        None
    }

    /// The highlight this output should show, in its own buffer coordinates.
    fn highlight_for(&self, idx: usize) -> Option<(Rect, [u8; 4])> {
        let surf = &self.surfaces[idx];
        let s = surf.scale;
        let ap = &self.config.appearance;
        let win_border = [ap.window_border_color[2], ap.window_border_color[1], ap.window_border_color[0], 255];
        let reg_border = [ap.region_border_color[2], ap.region_border_color[1], ap.region_border_color[0], 255];

        // Scale the edges rather than the origin and size independently, so
        // the drawn rectangle and the saved crop cannot disagree by a pixel.
        let to_rect = |x: f64, y: f64, w: f64, h: f64| {
            let x0 = ((x - surf.logical_x as f64) * s).round();
            let y0 = ((y - surf.logical_y as f64) * s).round();
            let x1 = ((x + w - surf.logical_x as f64) * s).round();
            let y1 = ((y + h - surf.logical_y as f64) * s).round();
            Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0).max(0.0) as u32, h: (y1 - y0).max(0.0) as u32 }
        };

        if self.dragging {
            let (sx, sy) = self.drag_start;
            let (ex, ey) = self.drag_end;
            let (x, y) = (sx.min(ex), sy.min(ey));
            let (w, h) = ((sx - ex).abs(), (sy - ey).abs());
            if w < 1.0 || h < 1.0 { return None; }
            Some((to_rect(x, y, w, h), reg_border))
        } else if let Some(widx) = self.hovered_window {
            let w = &self.windows[widx];
            // A window rect is clipped to one output, so every other surface
            // has nothing of it to draw. Without this the border strips, which
            // are placed just outside the rect, landed on the neighbouring
            // monitor whenever a window ended at the shared edge.
            let (sx0, sy0) = (surf.logical_x as f64, surf.logical_y as f64);
            let (sx1, sy1) = (sx0 + surf.phys_w as f64 / s, sy0 + surf.phys_h as f64 / s);
            if w.x >= sx1 || w.x + w.width <= sx0 || w.y >= sy1 || w.y + w.height <= sy0 {
                return None;
            }
            Some((to_rect(w.x, w.y, w.width, w.height), win_border))
        } else {
            None
        }
    }

    fn draw_surface(&mut self, idx: usize, qh: &QueueHandle<Self>) {
        if !self.surfaces[idx].configured { return; }
        let border_width = self.config.appearance.border_width;
        let (hl, border_color) = match self.highlight_for(idx) {
            Some((r, c)) => (Some(r), c),
            None => (None, [0u8; 4]),
        };

        // Split the borrow so the capture and this surface can be touched at once.
        let App { ss_data, ss_stride, surfaces, .. } = self;
        let surf = &mut surfaces[idx];
        let (phys_w, phys_h) = (surf.phys_w, surf.phys_h);
        let stride = phys_w * 4;

        // Pick a buffer the compositor is not currently reading.
        let mut chosen = None;
        for i in 0..surf.buffers.len() {
            if surf.buffers[i].buffer.canvas(&mut surf.pool).is_some() {
                chosen = Some(i);
                break;
            }
        }
        let bi = match chosen {
            Some(i) => i,
            None => {
                if surf.buffers.len() >= 2 {
                    // Both in flight. The frame callback will come back to this.
                    surf.needs_redraw = true;
                    return;
                }
                match surf.pool.create_buffer(
                    phys_w as i32, phys_h as i32, stride as i32,
                    // The capture is fully opaque, so there is no alpha to blend.
                    wl_shm::Format::Xrgb8888,
                ) {
                    Ok((buffer, _)) => {
                        surf.buffers.push(BufferState { buffer, initialized: false, drawn_hl: None });
                        surf.buffers.len() - 1
                    }
                    Err(e) => {
                        eprintln!("sshot: buffer allocation failed: {e}");
                        return;
                    }
                }
            }
        };

        let was_initialized = surf.buffers[bi].initialized;
        // What this buffer already holds, which is what has to be painted over.
        let previous_hl = surf.buffers[bi].drawn_hl;
        // What the compositor is currently showing, which is what the damage
        // has to be measured against. With two buffers in rotation the two
        // disagree by a frame, and they are not interchangeable.
        let shown_hl = surf.shown_hl;

        let dimmed = &surf.dimmed;
        let (ox, oy) = (surf.ss_offset_x, surf.ss_offset_y);
        let Some(canvas) = surf.buffers[bi].buffer.canvas(&mut surf.pool) else { return };

        if !was_initialized {
            // First use of this buffer: it has no contents to repair.
            let len = canvas.len().min(dimmed.len());
            canvas[..len].copy_from_slice(&dimmed[..len]);
        } else if let Some(prev) = previous_hl {
            // Erase only what the previous frame painted into this buffer.
            if let Some(r) = prev.inflate(border_width).clip(phys_w, phys_h) {
                restore_dim(canvas, dimmed, phys_w, r);
            }
        }

        if let Some(hl) = hl {
            if let Some(r) = hl.clip(phys_w, phys_h) {
                blit_undimmed(canvas, ss_data, *ss_stride, ox, oy, phys_w, r);
            }
            draw_border(canvas, phys_w, phys_h, hl, border_width, border_color);
        }

        // Damage only what actually changed on screen.
        let damage = if !was_initialized {
            Some(Rect { x: 0, y: 0, w: phys_w, h: phys_h })
        } else {
            damage_between(shown_hl, hl, border_width)
        };

        surf.buffers[bi].initialized = true;
        surf.buffers[bi].drawn_hl = hl;

        if surf.buffers[bi].buffer.attach_to(&surf.wl_surface).is_err() {
            eprintln!("sshot: failed to attach buffer");
            return;
        }
        if let Some(d) = damage {
            if let Some((x0, y0, x1, y1)) = d.clip(phys_w, phys_h) {
                surf.wl_surface.damage_buffer(
                    x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32,
                );
            }
        }
        surf.wl_surface.frame(qh, surf.wl_surface.clone());
        surf.wl_surface.commit();
        surf.needs_redraw = false;
        surf.frame_pending = true;
        surf.shown_hl = hl;
    }

    /// Redraw only the outputs whose highlight actually changed.
    ///
    /// Marking every surface dirty on every pointer event meant the idle
    /// monitor repainted a byte-identical frame for each motion event.
    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        for i in 0..self.surfaces.len() {
            let wanted = self.highlight_for(i).map(|(r, _)| r);
            if wanted != self.surfaces[i].shown_hl {
                self.surfaces[i].needs_redraw = true;
            }
        }
        for i in 0..self.surfaces.len() {
            if self.surfaces[i].needs_redraw && !self.surfaces[i].frame_pending {
                self.draw_surface(i, qh);
            }
        }
    }

    fn handle_selection(&mut self) {
        // A drag engages after DRAG_THRESHOLD, so committing anything smaller
        // than MIN_REGION would save a stray sliver from a twitchy click.
        let committed = if self.dragging {
            let (sx, sy) = self.drag_start;
            let (ex, ey) = self.drag_end;
            let x = sx.min(ex); let y = sy.min(ey);
            let w = (sx - ex).abs(); let h = (sy - ey).abs();
            if w >= MIN_REGION && h >= MIN_REGION {
                self.selection = Some(Selection::Region { x, y, w, h });
                true
            } else {
                false
            }
        } else {
            false
        };

        if !committed {
            let (gx, gy) = self.mouse_global;
            if let Some(idx) = self.window_at(gx, gy) {
                let w = &self.windows[idx];
                self.selection = Some(Selection::Window { title: w.title.clone(), id: w.id.clone() });
            }
        }
        self.exit = true;
    }
}

// ── SCTK Handlers ────────────────────────────────────────────────

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(idx) = self.surfaces.iter().position(|s| &s.wl_surface == surface) {
            self.surfaces[idx].frame_pending = false;
            if self.surfaces[idx].needs_redraw {
                self.draw_surface(idx, qh);
            }
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: smithay_client_toolkit::seat::Capability) {
        use smithay_client_toolkit::seat::Capability;
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: smithay_client_toolkit::seat::Capability) {
        use smithay_client_toolkit::seat::Capability;
        if capability == Capability::Keyboard { self.keyboard.take(); }
        if capability == Capability::Pointer { self.pointer.take(); }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let surf_idx = match self.surfaces.iter().position(|s| s.wl_surface == event.surface) {
                Some(i) => i, None => continue,
            };
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.mouse_global = (
                        event.position.0 + self.surfaces[surf_idx].logical_x as f64,
                        event.position.1 + self.surfaces[surf_idx].logical_y as f64,
                    );
                    if self.pressed {
                        if !self.dragging {
                            let dx = self.mouse_global.0 - self.press_global.0;
                            let dy = self.mouse_global.1 - self.press_global.1;
                            if dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD { self.dragging = true; }
                        }
                        if self.dragging { self.drag_end = self.mouse_global; self.request_redraw(qh); }
                    } else {
                        let old = self.hovered_window;
                        self.hovered_window = self.window_at(self.mouse_global.0, self.mouse_global.1);
                        if old != self.hovered_window { self.request_redraw(qh); }
                    }
                }
                PointerEventKind::Press { button: 272, .. } => {
                    self.pressed = true;
                    self.press_global = self.mouse_global;
                    self.drag_start = self.mouse_global;
                    self.drag_end = self.mouse_global;
                }
                PointerEventKind::Release { button: 272, .. } => {
                    // Ignore a release with no matching press (possible if the
                    // overlay mapped while a button was already held).
                    if self.pressed { self.handle_selection(); }
                }
                PointerEventKind::Press { button: 273, .. } => { self.exit = true; }
                PointerEventKind::Leave { .. } => {
                    // Drop a stale hover highlight when the pointer moves to
                    // an output this surface does not cover.
                    if !self.pressed && self.hovered_window.take().is_some() {
                        self.request_redraw(qh);
                    }
                }
                _ => {}
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if event.keysym.raw() == 0xff1b || event.raw_code == 1 { self.exit = true; }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: u32) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) { self.exit = true; }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if let Some(idx) = self.surfaces.iter().position(|s| &s.layer == layer) {
            let (w, h) = configure.new_size;
            if w > 0 && h > 0 {
                if let Some(ref vp) = self.surfaces[idx].viewport {
                    vp.set_destination(w as i32, h as i32);
                }
            }
            let first = !self.surfaces[idx].configured;
            self.surfaces[idx].configured = true;
            // A repeat configure does not by itself change what is drawn, so
            // only force a paint the first time.
            if first {
                self.surfaces[idx].needs_redraw = true;
            }
            if self.surfaces[idx].needs_redraw && !self.surfaces[idx].frame_pending {
                self.draw_surface(idx, qh);
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for App {
    fn event(_: &mut Self, _: &wp_viewporter::WpViewporter, _: wp_viewporter::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wp_viewport::WpViewport, ()> for App {
    fn event(_: &mut Self, _: &wp_viewport::WpViewport, _: wp_viewport::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_keyboard!(App);
delegate_layer!(App);
delegate_shm!(App);
delegate_registry!(App);

// ── Public entry point ───────────────────────────────────────────

/// The tiling gap to lay windows out with.
///
/// niri's own config is the source of truth. `appearance.tiling_gap` only
/// applies when that cannot be read, so the two can no longer disagree.
pub fn effective_gap(config: &Config) -> f64 {
    crate::niri::config_gap().unwrap_or(config.appearance.tiling_gap as f64)
}

/// Run the screenshot overlay. Returns a CaptureResult if the user made a selection.
pub fn run(config: &Config) -> Result<Option<CaptureResult>> {
    let gap = effective_gap(config);
    let (ss_data, ss_width, ss_height, ss_stride) =
        crate::capture::capture_workspace().context("Failed to capture screenshot")?;

    // Window detection is a convenience on top of region selection. If niri is
    // unreachable the overlay still works, it just cannot offer windows.
    let (windows, space) = match crate::niri::snapshot() {
        Ok(snap) => {
            let space = CaptureSpace::from_snapshot(&snap);
            let cap = geometry::Capture {
                data: &ss_data,
                width: ss_width,
                height: ss_height,
                stride: ss_stride,
            };
            (geometry::layout_windows(&snap, gap, Some(&cap)), space)
        }
        Err(e) => {
            eprintln!("Warning: window detection failed: {e:#}");
            (Vec::new(), CaptureSpace { min_x: 0.0, min_y: 0.0, scale: 1.0 })
        }
    };

    let conn = Connection::connect_to_env().context("No Wayland connection")?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let viewporter = globals.bind::<wp_viewporter::WpViewporter, _, _>(&qh, 1..=1, ()).ok();

    let mut app = App {
        registry_state, seat_state, output_state, compositor, layer_shell, shm, viewporter,
        config: config.clone(),
        ss_data, ss_width, ss_stride,
        windows, surfaces: Vec::new(),
        keyboard: None, pointer: None,
        mouse_global: (0.0, 0.0), pressed: false, dragging: false,
        press_global: (0.0, 0.0), drag_start: (0.0, 0.0), drag_end: (0.0, 0.0),
        hovered_window: None, space,
        exit: false, selection: None,
    };

    event_queue.roundtrip(&mut app)?;
    app.create_overlays(&qh);
    event_queue.roundtrip(&mut app)?;

    // With no surface there is no way to receive a key, a click or a close, so
    // the dispatch loop below would block forever and wedge the daemon.
    if app.surfaces.is_empty() {
        anyhow::bail!("no usable outputs to place the overlay on");
    }

    loop {
        event_queue.blocking_dispatch(&mut app)?;
        if app.exit { break; }
    }

    match app.selection {
        Some(sel) => Ok(Some(CaptureResult {
            ss_data: app.ss_data, ss_width: app.ss_width, ss_stride: app.ss_stride,
            selection: sel, space: app.space,
        })),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covers(damage: Rect, r: Rect, bw: u32) -> bool {
        let bw = bw as i32;
        damage.x <= r.x - bw
            && damage.y <= r.y - bw
            && damage.x + damage.w as i32 >= r.x + r.w as i32 + bw
            && damage.y + damage.h as i32 >= r.y + r.h as i32 + bw
    }

    /// The reported bug: hovering a float that sits inside a tile left both
    /// outlines on screen. The new highlight is wholly inside the old one, so
    /// damaging only the new one leaves the old one untouched and visible.
    #[test]
    fn moving_onto_a_float_repaints_the_tile_outline_away() {
        let tile = Rect { x: 12, y: 12, w: 3816, h: 2082 };
        let float = Rect { x: 711, y: 579, w: 2166, h: 1254 };
        let bw = 3;

        let damage = damage_between(Some(tile), Some(float), bw).expect("the highlight moved");
        assert!(covers(damage, tile, bw), "the outline still on screen must be repainted");
        assert!(covers(damage, float, bw), "the new outline must be painted");
    }

    #[test]
    fn losing_the_highlight_repaints_it_away() {
        let tile = Rect { x: 12, y: 12, w: 3816, h: 2082 };
        let damage = damage_between(Some(tile), None, 3).expect("the highlight went away");
        assert!(covers(damage, tile, 3));
    }

    #[test]
    fn an_unchanged_empty_highlight_damages_nothing() {
        assert_eq!(damage_between(None, None, 3), None);
    }
}
