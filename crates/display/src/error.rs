//! Typed errors for the host presentation stack.

/// Everything that can go wrong in the display crate.
///
/// Init-time failures (event loop, adapter, device) are fatal for the window;
/// scanout-update failures are *not* — they reject one guest request and leave
/// the window running, mirroring the "malformed guest input fails the request"
/// rule from the workspace hard rules.
#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// winit refused to create or run the event loop.
    #[error("event loop error: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),

    /// winit refused to create the window.
    #[error("window creation failed: {0}")]
    Window(#[from] winit::error::OsError),

    /// wgpu could not create a surface for the window.
    #[error("surface creation failed: {0}")]
    CreateSurface(#[from] wgpu::CreateSurfaceError),

    /// No GPU adapter (not even a software fallback) can present to the window.
    #[error("no usable wgpu adapter: {0}")]
    NoAdapter(#[from] wgpu::RequestAdapterError),

    /// The adapter exists but refused a device with the limits we need.
    #[error("wgpu device request failed: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),

    /// The adapter cannot present to this surface at all (no supported format).
    #[error("adapter cannot present to this window surface")]
    UnsupportedSurface,

    /// Presenting failed in a way reconfiguring the surface cannot fix.
    #[error("surface presentation failed: {0}")]
    Surface(#[from] wgpu::SurfaceError),

    /// Requested scanout dimensions are zero or beyond [`crate::MAX_SCANOUT_PIXELS`].
    #[error("invalid scanout resolution {width}x{height}")]
    InvalidResolution {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
    },

    /// A partial update rect does not fit inside the current scanout.
    #[error(
        "update rect {x},{y} {width}x{height} does not fit the {scanout_width}x{scanout_height} scanout"
    )]
    RectOutOfBounds {
        /// Rect origin x.
        x: u32,
        /// Rect origin y.
        y: u32,
        /// Rect width.
        width: u32,
        /// Rect height.
        height: u32,
        /// Current scanout width.
        scanout_width: u32,
        /// Current scanout height.
        scanout_height: u32,
    },

    /// The caller passed fewer pixel bytes than the rect describes.
    #[error("update rect needs {expected} bytes of pixel data, got {actual}")]
    ShortPixelData {
        /// Bytes required for the rect (`width * height * 4`).
        expected: usize,
        /// Bytes actually supplied.
        actual: usize,
    },

    /// Invalid `[display]` configuration.
    #[error("invalid display configuration: {0}")]
    Config(&'static str),

    /// PNG encoding of a screenshot failed.
    #[error("screenshot encoding failed: {0}")]
    Png(#[from] png::EncodingError),

    /// Writing a screenshot to disk failed.
    #[error("screenshot io failed: {0}")]
    Io(#[from] std::io::Error),
}
