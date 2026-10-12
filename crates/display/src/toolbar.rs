//! The quick-actions bar that slides in at the top edge of a VM window: shut
//! down, restart, pause, suspend, fullscreen, 1:1, release input, power off.
//!
//! # When it shows
//!
//! Only where the top edge means "I want the VM's controls" rather than "I am
//! on my way to the title bar": in **fullscreen**, and while the input is
//! **grabbed** (the pointer is confined to the window, so it cannot be heading
//! anywhere else). The pointer touching the top [`REVEAL_EDGE`] pixels shows
//! the bar; leaving the bar by more than [`HIDE_MARGIN`] hides it. While the
//! pointer is over the bar the guest gets no pointer events and the host
//! cursor is shown, so the bar is used with the host's own arrow.
//!
//! # Destructive buttons ask twice
//!
//! "Power off" pulls the plug and "Restart" reboots; a stray click on either
//! loses work, and the bar appears exactly where a fullscreen user aims at a
//! guest's own top panel. Each needs a second click within [`CONFIRM_WINDOW`];
//! the first one only arms it and says so.
//!
//! # Drawing
//!
//! Pure CPU raster, like the bundled cursor: the bar is rendered into a
//! premultiplied BGRA image whenever its state changes, and the renderer draws
//! that image over the frame (`present::CursorOverlay`, the cursor plane's
//! pipeline). Icons are signed-distance shapes, so they are crisp at any DPI;
//! labels use a 5×7 bitmap font scaled by a whole factor. No font file, no
//! theme, nothing the host may lack.

use std::time::{Duration, Instant};

/// How close to the top edge, in logical pixels, the pointer must come.
pub const REVEAL_EDGE: f64 = 3.0;
/// How far below the bar (and its label), in logical pixels, the pointer may
/// wander before the bar hides.
pub const HIDE_MARGIN: f64 = 28.0;
/// How long an armed destructive button waits for its second click.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

const BAR_HEIGHT: f64 = 44.0;
const BUTTON_W: f64 = 40.0;
const BUTTON_H: f64 = 34.0;
const GAP: f64 = 4.0;
const GROUP_GAP: f64 = 14.0;
const PADDING: f64 = 6.0;
const RADIUS: f64 = 10.0;
const LABEL_TOP_GAP: f64 = 6.0;

/// One button on the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// The guest's ACPI power button.
    Shutdown,
    /// Reboot in place (asks twice).
    Restart,
    /// Freeze or continue.
    Pause,
    /// Write the VM to its snapshot file and stop.
    Suspend,
    /// Borderless fullscreen on and off.
    Fullscreen,
    /// Fit to the window, or 1:1 pixels.
    Scale,
    /// Save a PNG of the guest's screen in the host's Pictures folder.
    Screenshot,
    /// Type the host clipboard's text into the guest as keystrokes.
    TypeClipboard,
    /// Give the keyboard and pointer back to the host.
    Release,
    /// Stop the VM now (asks twice).
    PowerOff,
}

impl Button {
    /// Left to right; a group gap separates the guest's lifecycle, the
    /// window's own controls and the plug.
    pub const ALL: [Button; 10] = [
        Button::Shutdown,
        Button::Restart,
        Button::Pause,
        Button::Suspend,
        Button::Fullscreen,
        Button::Scale,
        Button::Screenshot,
        Button::TypeClipboard,
        Button::Release,
        Button::PowerOff,
    ];

    /// Whether a group gap comes before this button.
    fn starts_group(self) -> bool {
        matches!(self, Button::Fullscreen | Button::PowerOff)
    }

    /// Needs a second click (see the module docs).
    pub fn needs_confirm(self) -> bool {
        matches!(self, Button::Restart | Button::PowerOff)
    }

    /// The label shown under the hovered button.
    pub fn label(self) -> &'static str {
        match self {
            Button::Shutdown => "SHUT DOWN",
            Button::Restart => "RESTART  CTRL+ALT+R",
            Button::Pause => "PAUSE / RESUME  CTRL+ALT+P",
            Button::Suspend => "SUSPEND TO DISK  CTRL+ALT+S",
            Button::Fullscreen => "FULLSCREEN  F11",
            Button::Scale => "FIT / 1:1  CTRL+ALT+O",
            Button::Screenshot => "SCREENSHOT TO PICTURES",
            Button::TypeClipboard => "TYPE THE CLIPBOARD  CTRL+ALT+V",
            Button::Release => "RELEASE INPUT  CTRL+ALT",
            Button::PowerOff => "POWER OFF (HARD)",
        }
    }

    /// The label while armed, waiting for the second click.
    fn armed_label(self) -> &'static str {
        match self {
            Button::Restart => "CLICK AGAIN TO RESTART",
            Button::PowerOff => "CLICK AGAIN TO POWER OFF",
            other => other.label(),
        }
    }

    /// What a (confirmed) click asks for.
    pub fn action(self) -> ToolbarAction {
        match self {
            Button::Shutdown => ToolbarAction::PowerButton,
            Button::Restart => ToolbarAction::Reset,
            Button::Pause => ToolbarAction::PauseToggle,
            Button::Suspend => ToolbarAction::Suspend,
            Button::Fullscreen => ToolbarAction::ToggleFullscreen,
            Button::Scale => ToolbarAction::ToggleScale,
            Button::Screenshot => ToolbarAction::Screenshot,
            Button::TypeClipboard => ToolbarAction::TypeClipboard,
            Button::Release => ToolbarAction::ReleaseInput,
            Button::PowerOff => ToolbarAction::PowerOff,
        }
    }
}

/// What the window does for a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarAction {
    /// The guest's ACPI power button: a clean shutdown the guest performs.
    PowerButton,
    /// Stop the VM now (the window's close button, `Ctrl+Alt+Q`).
    PowerOff,
    /// Freeze the VM, or let a frozen one continue.
    PauseToggle,
    /// Reboot the VM in place.
    Reset,
    /// Suspend to the snapshot file.
    Suspend,
    /// Borderless fullscreen on and off.
    ToggleFullscreen,
    /// Fit or 1:1.
    ToggleScale,
    /// Release the input grab.
    ReleaseInput,
    /// Type the host clipboard's text into the guest (`virtio_input::typing`).
    TypeClipboard,
    /// Save a PNG of the guest's screen in the host's Pictures folder.
    Screenshot,
}

/// A rectangle in physical window pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Where everything goes, for a window of a given width and DPI scale.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The bar itself.
    pub bar: Rect,
    /// One per [`Button::ALL`], in order.
    pub buttons: Vec<(Button, Rect)>,
    /// The DPI scale it was laid out for.
    pub scale: f64,
}

impl Layout {
    /// Centred at the top of a `window_w`-wide window. `scale` is the window's
    /// DPI scale factor; every size above is in logical pixels.
    pub fn new(window_w: u32, scale: f64) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let groups = Button::ALL.iter().filter(|b| b.starts_group()).count() as f64;
        let n = Button::ALL.len() as f64;
        let width =
            (2.0 * PADDING + n * BUTTON_W + (n - 1.0 - groups) * GAP + groups * GROUP_GAP) * scale;
        let height = BAR_HEIGHT * scale;
        let x = ((f64::from(window_w) - width) / 2.0).max(0.0).round();
        let bar = Rect {
            x,
            y: 0.0,
            w: width.round(),
            h: height.round(),
        };
        let mut cursor = x + PADDING * scale;
        let top = ((BAR_HEIGHT - BUTTON_H) / 2.0 * scale).round();
        let mut buttons = Vec::with_capacity(Button::ALL.len());
        for (i, button) in Button::ALL.iter().enumerate() {
            if i > 0 {
                cursor += if button.starts_group() {
                    GROUP_GAP
                } else {
                    GAP
                } * scale;
            }
            buttons.push((
                *button,
                Rect {
                    x: cursor.round(),
                    y: top,
                    w: (BUTTON_W * scale).round(),
                    h: (BUTTON_H * scale).round(),
                },
            ));
            cursor += BUTTON_W * scale;
        }
        Self {
            bar,
            buttons,
            scale,
        }
    }

    /// The button under `(x, y)`, if any.
    pub fn hit(&self, x: f64, y: f64) -> Option<Button> {
        self.buttons
            .iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(b, _)| *b)
    }
}

/// The bar's state: whether it shows, what is hovered, what is armed.
#[derive(Debug)]
pub struct Toolbar {
    layout: Layout,
    window_h: u32,
    visible: bool,
    hover: Option<Button>,
    /// Whether the pointer is over the bar (buttons or not): the guest gets
    /// nothing while it is.
    over_bar: bool,
    armed: Option<(Button, Instant)>,
    /// Bumped on every change that alters the picture.
    serial: u64,
    /// The rendered picture and the serial it was drawn at.
    cache: Option<(u64, Overlay)>,
}

/// The rendered bar, ready for the renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    /// Changes whenever the pixels do.
    pub serial: u64,
    /// Left edge in physical window pixels.
    pub x: u32,
    /// Top edge in physical window pixels.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Premultiplied BGRA, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl Toolbar {
    /// A hidden bar for a window of `window` physical pixels at DPI `scale`.
    pub fn new(window: (u32, u32), scale: f64) -> Self {
        Self {
            layout: Layout::new(window.0, scale),
            window_h: window.1,
            visible: false,
            hover: None,
            over_bar: false,
            armed: None,
            serial: 1,
            cache: None,
        }
    }

    /// The window changed size or DPI scale.
    pub fn set_window(&mut self, window: (u32, u32), scale: f64) {
        let layout = Layout::new(window.0, scale);
        if layout != self.layout || window.1 != self.window_h {
            self.layout = layout;
            self.window_h = window.1;
            self.changed();
        }
    }

    /// Whether the bar is showing.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Changes whenever the picture would: compare to know when to redraw.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// True while the pointer is over the bar: the host shows its cursor and
    /// the guest is told nothing.
    pub fn captures_pointer(&self) -> bool {
        self.visible && self.over_bar
    }

    /// Where the bar and its buttons are.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn changed(&mut self) {
        self.serial = self.serial.wrapping_add(1);
    }

    /// The region the pointer may stay in without the bar hiding: the bar,
    /// its label, and [`HIDE_MARGIN`] below both.
    fn keep_region(&self) -> Rect {
        let s = self.layout.scale;
        let below = self.layout.bar.h + (LABEL_TOP_GAP + 7.0 * 2.0 + 12.0 + HIDE_MARGIN) * s;
        Rect {
            x: self.layout.bar.x - HIDE_MARGIN * s,
            y: 0.0,
            w: self.layout.bar.w + 2.0 * HIDE_MARGIN * s,
            h: below,
        }
    }

    /// The pointer moved to `(x, y)` (physical window pixels). `eligible` is
    /// whether the window is in a state where the bar may show (fullscreen or
    /// grabbed). Answers whether the pointer is over the bar — the caller then
    /// keeps the motion from the guest.
    pub fn on_pointer(&mut self, x: f64, y: f64, eligible: bool, now: Instant) -> bool {
        self.expire(now);
        let was = (self.visible, self.hover, self.over_bar);
        if !eligible {
            self.visible = false;
        } else if y <= REVEAL_EDGE * self.layout.scale {
            self.visible = true;
        } else if self.visible && !self.keep_region().contains(x, y) {
            self.visible = false;
        }
        if self.visible {
            self.over_bar = self.layout.bar.contains(x, y);
            self.hover = self.layout.hit(x, y);
        } else {
            self.over_bar = false;
            self.hover = None;
            self.armed = None;
        }
        if was != (self.visible, self.hover, self.over_bar) {
            self.changed();
        }
        self.captures_pointer()
    }

    /// The pointer left the window, or the window lost focus or eligibility.
    pub fn hide(&mut self) {
        if self.visible || self.armed.is_some() {
            self.visible = false;
            self.hover = None;
            self.over_bar = false;
            self.armed = None;
            self.changed();
        }
    }

    fn expire(&mut self, now: Instant) {
        if let Some((_, at)) = self.armed {
            if now.duration_since(at) > CONFIRM_WINDOW {
                self.armed = None;
                self.changed();
            }
        }
    }

    /// A left click at the pointer's last position. Answers the action to
    /// perform, if this click completes one.
    pub fn on_click(&mut self, now: Instant) -> Option<ToolbarAction> {
        self.expire(now);
        let button = self.hover.filter(|_| self.visible)?;
        if button.needs_confirm() {
            match self.armed {
                Some((armed, _)) if armed == button => {
                    self.armed = None;
                    self.changed();
                }
                _ => {
                    self.armed = Some((button, now));
                    self.changed();
                    return None;
                }
            }
        } else if self.armed.take().is_some() {
            self.changed();
        }
        // An action that changes the window (fullscreen, release) is followed
        // by a hide: the pointer's next move re-reveals it if it stays put.
        if matches!(
            button.action(),
            ToolbarAction::ToggleFullscreen | ToolbarAction::ReleaseInput
        ) {
            self.hide();
        }
        Some(button.action())
    }

    /// The picture to draw, or `None` while hidden. Re-rendered only when the
    /// state changed since the last call.
    pub fn overlay(&mut self) -> Option<&Overlay> {
        if !self.visible {
            return None;
        }
        let stale = self.cache.as_ref().map(|(s, _)| *s) != Some(self.serial);
        if stale {
            let overlay = render(
                &self.layout,
                self.hover,
                self.armed.map(|(b, _)| b),
                self.serial,
            );
            self.cache = Some((self.serial, overlay));
        }
        self.cache.as_ref().map(|(_, o)| o)
    }
}

// ---------------------------------------------------------------- raster

/// Colours, straight (not premultiplied) RGBA — the manager's theme tokens.
const PANEL: [f32; 4] = [
    0x0b as f32 / 255.0,
    0x10 as f32 / 255.0,
    0x21 as f32 / 255.0,
    0.93,
];
const STROKE: [f32; 4] = [
    0x2b as f32 / 255.0,
    0x3d as f32 / 255.0,
    0x63 as f32 / 255.0,
    1.0,
];
const HOVER: [f32; 4] = [
    0x17 as f32 / 255.0,
    0x21 as f32 / 255.0,
    0x3d as f32 / 255.0,
    1.0,
];
const TEXT: [f32; 4] = [
    0xdd as f32 / 255.0,
    0xe6 as f32 / 255.0,
    0xf7 as f32 / 255.0,
    1.0,
];
const CYAN: [f32; 4] = [
    0x35 as f32 / 255.0,
    0xe2 as f32 / 255.0,
    0xf0 as f32 / 255.0,
    1.0,
];
const ERR: [f32; 4] = [
    0xff as f32 / 255.0,
    0x5d as f32 / 255.0,
    0x73 as f32 / 255.0,
    1.0,
];
const WARN: [f32; 4] = [
    0xff as f32 / 255.0,
    0xb4 as f32 / 255.0,
    0x54 as f32 / 255.0,
    1.0,
];

/// A premultiplied BGRA canvas with source-over painting.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<f32>, // premultiplied RGBA
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0.0; w * h * 4],
        }
    }

    /// Paints `colour` at `(x, y)` with `coverage` in 0..=1.
    fn blend(&mut self, x: usize, y: usize, colour: [f32; 4], coverage: f32) {
        if x >= self.w || y >= self.h || coverage <= 0.0 {
            return;
        }
        let a = (colour[3] * coverage.min(1.0)).clamp(0.0, 1.0);
        let i = (y * self.w + x) * 4;
        for (dst, src) in self.px[i..i + 3].iter_mut().zip(colour) {
            *dst = src * a + *dst * (1.0 - a);
        }
        self.px[i + 3] = a + self.px[i + 3] * (1.0 - a);
    }

    /// Fills every pixel whose centre's signed distance (negative inside) is
    /// below zero, anti-aliased over one pixel, within `bounds`.
    fn fill(&mut self, bounds: Rect, colour: [f32; 4], sdf: impl Fn(f64, f64) -> f64) {
        let x0 = bounds.x.floor().max(0.0) as usize;
        let y0 = bounds.y.floor().max(0.0) as usize;
        let x1 = ((bounds.x + bounds.w).ceil() as usize).min(self.w);
        let y1 = ((bounds.y + bounds.h).ceil() as usize).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = sdf(x as f64 + 0.5, y as f64 + 0.5);
                let coverage = (0.5 - d).clamp(0.0, 1.0) as f32;
                self.blend(x, y, colour, coverage);
            }
        }
    }

    fn into_bgra(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.w * self.h * 4);
        for p in self.px.chunks_exact(4) {
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            out.extend_from_slice(&[q(p[2]), q(p[1]), q(p[0]), q(p[3])]);
        }
        out
    }
}

fn sd_round_rect(px: f64, py: f64, r: Rect, radius: f64) -> f64 {
    let cx = r.x + r.w / 2.0;
    let cy = r.y + r.h / 2.0;
    let hx = r.w / 2.0 - radius;
    let hy = r.h / 2.0 - radius;
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

fn sd_segment(px: f64, py: f64, a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let (wx, wy) = (px - a.0, py - a.1);
    let len2 = vx * vx + vy * vy;
    let t = if len2 > 0.0 {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((wx - t * vx).powi(2) + (wy - t * vy).powi(2)).sqrt()
}

/// A ring of radius `r` and half-thickness `t`, open where the angle from
/// straight up is within `gap` radians (0 for a full ring).
fn sd_arc(px: f64, py: f64, c: (f64, f64), r: f64, t: f64, gap: f64, gap_dir: f64) -> f64 {
    let (dx, dy) = (px - c.0, py - c.1);
    let ring = ((dx * dx + dy * dy).sqrt() - r).abs() - t;
    if gap <= 0.0 {
        return ring;
    }
    // Angle measured from the gap direction (radians, 0 = up, clockwise).
    let angle = dx.atan2(-dy) - gap_dir;
    let angle = angle.sin().atan2(angle.cos()).abs();
    if angle < gap {
        // Distance to the nearer arc end, capped round.
        let end = |sign: f64| {
            let a = gap_dir + sign * gap;
            (c.0 + r * a.sin(), c.1 - r * a.cos())
        };
        let (e1, e2) = (end(1.0), end(-1.0));
        let d1 = ((px - e1.0).powi(2) + (py - e1.1).powi(2)).sqrt();
        let d2 = ((px - e2.0).powi(2) + (py - e2.1).powi(2)).sqrt();
        d1.min(d2) - t
    } else {
        ring
    }
}

/// Inside of a convex polygon given clockwise in screen space: a cheap SDF
/// approximation (exact enough for anti-aliasing small icons).
fn sd_convex(px: f64, py: f64, points: &[(f64, f64)]) -> f64 {
    let mut d = f64::MIN;
    for i in 0..points.len() {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        let (ex, ey) = (b.0 - a.0, b.1 - a.1);
        let len = (ex * ex + ey * ey).sqrt().max(1e-9);
        // Outward normal for a clockwise (screen) polygon.
        let (nx, ny) = (ey / len, -ex / len);
        d = d.max((px - a.0) * nx + (py - a.1) * ny);
    }
    d
}

fn draw_icon(canvas: &mut Canvas, button: Button, r: Rect, colour: [f32; 4], s: f64) {
    let c = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let t = 1.1 * s; // stroke half-width
    let k = 7.5 * s; // icon "radius"
    match button {
        Button::Shutdown | Button::PowerOff => {
            canvas.fill(r, colour, |x, y| {
                let arc = sd_arc(x, y, c, k, t, 0.75, 0.0);
                let bar = sd_segment(x, y, (c.0, c.1 - k - 1.0 * s), (c.0, c.1 - 1.5 * s)) - t;
                arc.min(bar)
            });
        }
        Button::Restart => {
            let gap_dir = 0.9_f64; // gap up and to the right
            canvas.fill(r, colour, |x, y| sd_arc(x, y, c, k * 0.9, t, 0.5, gap_dir));
            // Arrowhead at the clockwise end of the arc.
            let a = gap_dir - 0.5;
            let tip = (c.0 + k * 0.9 * a.sin(), c.1 - k * 0.9 * a.cos());
            let head = 3.6 * s;
            let pts = [
                (tip.0 - head, tip.1 - head * 0.2),
                (tip.0 + head * 0.9, tip.1 - head * 0.9),
                (tip.0 + head * 0.3, tip.1 + head * 1.1),
            ];
            canvas.fill(r, colour, |x, y| sd_convex(x, y, &pts));
        }
        Button::Pause => {
            let h = k * 0.95;
            canvas.fill(r, colour, |x, y| {
                let left = sd_round_rect(
                    x,
                    y,
                    Rect {
                        x: c.0 - 5.0 * s,
                        y: c.1 - h,
                        w: 3.4 * s,
                        h: 2.0 * h,
                    },
                    1.0 * s,
                );
                let right = sd_round_rect(
                    x,
                    y,
                    Rect {
                        x: c.0 + 1.6 * s,
                        y: c.1 - h,
                        w: 3.4 * s,
                        h: 2.0 * h,
                    },
                    1.0 * s,
                );
                left.min(right)
            });
        }
        Button::Suspend => {
            // A crescent moon.
            canvas.fill(r, colour, |x, y| {
                let outer = ((x - c.0).powi(2) + (y - c.1).powi(2)).sqrt() - k;
                let inner = ((x - (c.0 + 3.6 * s)).powi(2) + (y - (c.1 - 2.6 * s)).powi(2)).sqrt()
                    - k * 0.8;
                outer.max(-inner)
            });
        }
        Button::Fullscreen => {
            let e = k;
            let l = 4.0 * s;
            canvas.fill(r, colour, |x, y| {
                let mut d = f64::MAX;
                for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    let corner = (c.0 + sx * e, c.1 + sy * e);
                    d = d.min(sd_segment(x, y, corner, (corner.0 - sx * l, corner.1)));
                    d = d.min(sd_segment(x, y, corner, (corner.0, corner.1 - sy * l)));
                }
                d - t
            });
        }
        Button::Scale => {
            let k_font = font_scale(s);
            let text = "1:1";
            let (w, h) = text_size(text, k_font);
            draw_text(
                canvas,
                text,
                (c.0 - w / 2.0).round(),
                (c.1 - h / 2.0).round(),
                k_font,
                colour,
            );
        }
        Button::Screenshot => {
            // A camera: a rounded body, a lens, and the viewfinder bump.
            let body = Rect {
                x: c.0 - 6.0 * s,
                y: c.1 - k + 2.0 * s,
                w: 12.0 * s,
                h: 2.0 * k - 3.0 * s,
            };
            let bump = Rect {
                x: c.0 - 2.5 * s,
                y: c.1 - k,
                w: 5.0 * s,
                h: 3.0 * s,
            };
            canvas.fill(r, colour, |x, y| {
                let outline = sd_round_rect(x, y, body, 1.8 * s).abs() - t;
                let bump = sd_round_rect(x, y, bump, 1.0 * s);
                let lens_c = (c.0, c.1 + 1.0 * s);
                let lens =
                    (((x - lens_c.0).powi(2) + (y - lens_c.1).powi(2)).sqrt() - 2.6 * s).abs() - t;
                outline.min(bump).min(lens)
            });
        }
        Button::TypeClipboard => {
            // A clipboard: a board with a clip at the top and two lines of text.
            let board = Rect {
                x: c.0 - 5.0 * s,
                y: c.1 - k + 1.0 * s,
                w: 10.0 * s,
                h: 2.0 * k - 1.0 * s,
            };
            let clip = Rect {
                x: c.0 - 2.5 * s,
                y: c.1 - k - 0.5 * s,
                w: 5.0 * s,
                h: 3.0 * s,
            };
            canvas.fill(r, colour, |x, y| {
                let outline = sd_round_rect(x, y, board, 1.5 * s).abs() - t;
                let clip = sd_round_rect(x, y, clip, 1.0 * s);
                let line1 = sd_segment(x, y, (c.0 - 2.5 * s, c.1), (c.0 + 2.5 * s, c.1)) - t;
                let line2 = sd_segment(
                    x,
                    y,
                    (c.0 - 2.5 * s, c.1 + 3.0 * s),
                    (c.0 + 1.0 * s, c.1 + 3.0 * s),
                ) - t;
                outline.min(clip).min(line1).min(line2)
            });
        }
        Button::Release => {
            // A mouse: rounded body, split at the top.
            let body = Rect {
                x: c.0 - 5.5 * s,
                y: c.1 - k,
                w: 11.0 * s,
                h: 2.0 * k,
            };
            canvas.fill(r, colour, |x, y| {
                let shell = sd_round_rect(x, y, body, 5.5 * s).abs() - t;
                let split = sd_segment(x, y, (c.0, c.1 - k), (c.0, c.1 - 2.0 * s)) - t;
                shell.min(split)
            });
        }
    }
}

// ---------------------------------------------------------------- font

/// A 5×7 bitmap font: rows top to bottom, bit 4 the leftmost column.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'B' => [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e],
        'C' => [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e],
        'D' => [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
        'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
        'F' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10],
        'G' => [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0f],
        'H' => [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'I' => [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0c],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
        'M' => [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'P' => [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10],
        'Q' => [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d],
        'R' => [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
        'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
        'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0a],
        'X' => [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f],
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
        '6' => [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
        ':' => [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x0c, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1f, 0x04, 0x04, 0x00],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c],
        _ => [0; 7],
    }
}

/// Whole pixels per font pixel at a DPI scale: 2 at 100 %, 6 at 300 %.
fn font_scale(scale: f64) -> f64 {
    (2.0 * scale).round().max(1.0)
}

/// Width and height of `text` at font scale `k`: 5 columns plus 1 spacing.
fn text_size(text: &str, k: f64) -> (f64, f64) {
    let n = text.chars().count() as f64;
    ((n * 6.0 - 1.0).max(0.0) * k, 7.0 * k)
}

fn draw_text(canvas: &mut Canvas, text: &str, x: f64, y: f64, k: f64, colour: [f32; 4]) {
    let k_px = k.max(1.0) as usize;
    for (i, c) in text.chars().enumerate() {
        let rows = glyph(c);
        let gx = x + i as f64 * 6.0 * k;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5 {
                if bits & (0x10 >> col) == 0 {
                    continue;
                }
                let px = (gx + col as f64 * k) as usize;
                let py = (y + row as f64 * k) as usize;
                for dy in 0..k_px {
                    for dx in 0..k_px {
                        canvas.blend(px + dx, py + dy, colour, 1.0);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- render

fn render(layout: &Layout, hover: Option<Button>, armed: Option<Button>, serial: u64) -> Overlay {
    let s = layout.scale;
    let k = font_scale(s);
    let label = hover.map(|b| {
        if armed == Some(b) {
            b.armed_label()
        } else {
            b.label()
        }
    });
    // The label box hangs below the bar, centred on it, as wide as it needs.
    let (label_w, label_h) = label.map_or((0.0, 0.0), |l| text_size(l, k));
    let pad = 6.0 * s;
    let label_box = label.map(|_| Rect {
        x: 0.0,
        y: layout.bar.h + LABEL_TOP_GAP * s,
        w: (label_w + 2.0 * pad).round(),
        h: (label_h + 2.0 * pad).round(),
    });
    let width = layout.bar.w.max(label_box.map_or(0.0, |b| b.w)).ceil();
    let height = label_box.map_or(layout.bar.h, |b| b.y + b.h).ceil();
    // The overlay's origin: the bar's left edge, unless the label is wider.
    let origin_x = (layout.bar.x - (width - layout.bar.w) / 2.0)
        .max(0.0)
        .floor();
    let dx = layout.bar.x - origin_x;
    let mut canvas = Canvas::new(width as usize, height as usize);

    // The bar.
    let bar = Rect {
        x: dx,
        y: -RADIUS * s,
        w: layout.bar.w,
        h: layout.bar.h + RADIUS * s,
    };
    let radius = RADIUS * s;
    canvas.fill(bar, PANEL, |x, y| sd_round_rect(x, y, bar, radius));
    canvas.fill(bar, STROKE, |x, y| {
        sd_round_rect(x, y, bar, radius).abs() - 0.5 * s.max(1.0)
    });

    for (button, r) in &layout.buttons {
        let r = Rect {
            x: r.x - origin_x,
            ..*r
        };
        let is_armed = armed == Some(*button);
        if hover == Some(*button) || is_armed {
            let fill = if is_armed {
                mix(HOVER, ERR, 0.35)
            } else {
                HOVER
            };
            canvas.fill(r, fill, |x, y| sd_round_rect(x, y, r, 7.0 * s));
        }
        let colour = match button {
            Button::PowerOff => ERR,
            Button::Shutdown => CYAN,
            _ if is_armed => WARN,
            _ => TEXT,
        };
        draw_icon(&mut canvas, *button, r, colour, s);
    }

    if let (Some(text), Some(b)) = (label, label_box) {
        let b = Rect {
            x: ((width - b.w) / 2.0).round(),
            ..b
        };
        canvas.fill(b, PANEL, |x, y| sd_round_rect(x, y, b, 6.0 * s));
        canvas.fill(b, STROKE, |x, y| {
            sd_round_rect(x, y, b, 6.0 * s).abs() - 0.5 * s.max(1.0)
        });
        let colour = if hover.is_some() && armed == hover {
            WARN
        } else {
            TEXT
        };
        draw_text(&mut canvas, text, b.x + pad, b.y + pad, k, colour);
    }

    Overlay {
        serial,
        x: origin_x as u32,
        y: 0,
        width: width as u32,
        height: height as u32,
        pixels: canvas.into_bgra(),
    }
}

fn mix(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> Toolbar {
        Toolbar::new((1920, 1080), 1.0)
    }

    fn centre(layout: &Layout, button: Button) -> (f64, f64) {
        let (_, r) = layout
            .buttons
            .iter()
            .find(|(b, _)| *b == button)
            .copied()
            .expect("button laid out");
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    #[test]
    fn the_layout_is_centred_and_every_button_inside_the_bar() {
        for (w, scale) in [(1920u32, 1.0), (3840, 3.0), (800, 1.25)] {
            let layout = Layout::new(w, scale);
            let mid = layout.bar.x + layout.bar.w / 2.0;
            assert!((mid - f64::from(w) / 2.0).abs() <= 1.0, "{w} @ {scale}");
            let mut last_right = layout.bar.x;
            for (_, r) in &layout.buttons {
                assert!(r.x >= last_right, "buttons overlap");
                assert!(r.x + r.w <= layout.bar.x + layout.bar.w + 0.5);
                assert!(r.y >= 0.0 && r.y + r.h <= layout.bar.h);
                last_right = r.x + r.w;
            }
        }
    }

    #[test]
    fn it_shows_only_at_the_top_edge_and_only_when_eligible() {
        let now = Instant::now();
        let mut t = bar();
        assert!(
            !t.on_pointer(960.0, 1.0, false, now),
            "windowed and ungrabbed: never"
        );
        assert!(!t.visible());
        t.on_pointer(960.0, 300.0, true, now);
        assert!(!t.visible(), "the middle of the screen does not reveal it");
        t.on_pointer(960.0, 1.0, true, now);
        assert!(t.visible(), "the top edge does");
        // Moving down onto a button keeps it, and captures the pointer.
        let (x, y) = centre(t.layout(), Button::Pause);
        assert!(t.on_pointer(x, y, true, now));
        // Wandering far below hides it.
        t.on_pointer(x, 600.0, true, now);
        assert!(!t.visible());
        assert!(!t.captures_pointer());
    }

    #[test]
    fn a_plain_button_acts_on_the_first_click() {
        let now = Instant::now();
        let mut t = bar();
        t.on_pointer(960.0, 0.0, true, now);
        let (x, y) = centre(t.layout(), Button::Shutdown);
        t.on_pointer(x, y, true, now);
        assert_eq!(t.on_click(now), Some(ToolbarAction::PowerButton));
    }

    #[test]
    fn power_off_needs_a_second_click_within_the_window() {
        let now = Instant::now();
        let mut t = bar();
        t.on_pointer(960.0, 0.0, true, now);
        let (x, y) = centre(t.layout(), Button::PowerOff);
        t.on_pointer(x, y, true, now);
        assert_eq!(t.on_click(now), None, "the first click only arms");
        assert_eq!(
            t.on_click(now + Duration::from_millis(500)),
            Some(ToolbarAction::PowerOff)
        );
        // Too slow: the arm expires and the click arms again.
        assert_eq!(t.on_click(now + Duration::from_secs(10)), None);
        assert_eq!(
            t.on_click(now + Duration::from_secs(20)),
            None,
            "expired again"
        );
    }

    #[test]
    fn arming_one_button_does_not_confirm_another() {
        let now = Instant::now();
        let mut t = bar();
        t.on_pointer(960.0, 0.0, true, now);
        let (rx, ry) = centre(t.layout(), Button::Restart);
        let (px, py) = centre(t.layout(), Button::PowerOff);
        t.on_pointer(rx, ry, true, now);
        assert_eq!(t.on_click(now), None);
        t.on_pointer(px, py, true, now);
        assert_eq!(t.on_click(now), None, "power off arms on its own");
    }

    #[test]
    fn clicking_the_bar_between_buttons_does_nothing() {
        let now = Instant::now();
        let mut t = bar();
        t.on_pointer(960.0, 0.0, true, now);
        let b = t.layout().bar;
        assert!(
            t.on_pointer(b.x + 1.0, b.y + 1.0, true, now),
            "over the bar"
        );
        assert_eq!(t.on_click(now), None);
    }

    #[test]
    fn the_overlay_renders_premultiplied_and_only_when_visible() {
        let now = Instant::now();
        let mut t = bar();
        assert!(t.overlay().is_none());
        t.on_pointer(960.0, 0.0, true, now);
        let (x, y) = centre(t.layout(), Button::Suspend);
        t.on_pointer(x, y, true, now);
        let o = t.overlay().expect("visible").clone();
        assert_eq!(o.pixels.len() as u32, o.width * o.height * 4);
        assert!(o.height > t.layout().bar.h as u32, "the label hangs below");
        for p in o.pixels.chunks_exact(4) {
            let a = p[3];
            assert!(p[0] <= a && p[1] <= a && p[2] <= a, "premultiplied");
        }
        // Unchanged state: the same picture, not a new raster.
        let serial = o.serial;
        t.on_pointer(x, y, true, now);
        assert_eq!(t.overlay().map(|o| o.serial), Some(serial));
    }

    /// Writes the bar, with each button hovered in turn and Power off armed,
    /// as PNGs into `$ENTANGLED_TOOLBAR_PNG_DIR` — for looking at it.
    #[test]
    #[ignore]
    fn dump_toolbar_pngs() {
        let Some(dir) = std::env::var_os("ENTANGLED_TOOLBAR_PNG_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let now = Instant::now();
        for scale in [1.0, 2.0] {
            for button in Button::ALL {
                let mut t = Toolbar::new((1920, 1080), scale);
                t.on_pointer(960.0, 0.0, true, now);
                let (x, y) = centre(t.layout(), button);
                t.on_pointer(x, y, true, now);
                if button.needs_confirm() {
                    t.on_click(now);
                }
                let o = t.overlay().expect("visible").clone();
                // Composite over a mid-grey "guest" so translucency shows.
                let mut rgba = Vec::with_capacity(o.pixels.len());
                for p in o.pixels.chunks_exact(4) {
                    let a = f32::from(p[3]) / 255.0;
                    let over = |c: u8| (f32::from(c) + 96.0 * (1.0 - a)).round() as u8;
                    rgba.extend_from_slice(&[over(p[2]), over(p[1]), over(p[0]), 255]);
                }
                let path = dir.join(format!("toolbar-{scale}x-{button:?}.png"));
                let file = std::fs::File::create(&path).expect("create png");
                let mut encoder = png::Encoder::new(file, o.width, o.height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().expect("png header");
                writer.write_image_data(&rgba).expect("png data");
            }
        }
    }

    #[test]
    fn every_label_character_has_a_glyph() {
        for b in Button::ALL {
            for text in [b.label(), b.armed_label()] {
                for c in text.chars().filter(|c| *c != ' ') {
                    assert_ne!(glyph(c), [0; 7], "{c:?} in {text:?}");
                }
            }
        }
    }
}
