//! Host-independent choices for launching a configured VM.
use std::path::PathBuf;
use std::time::Duration;

/// Request a debug scanout capture after a delay, optionally repeated.
#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    pub after: Duration,
    pub path: PathBuf,
    pub burst: Option<Duration>,
}

/// Everything about *how* to run a VM that is not the VM itself.
///
/// A struct rather than five more parameters: the run path has grown a
/// lifecycle control channel, a debug screenshot and now a snapshot on either
/// end of it, and a call site that reads `run_with(cfg, false, None, None,
/// false, None, None)` tells the reader nothing.
#[derive(Debug, Default)]
pub struct RunOptions {
    pub headless: bool,
    pub control_stdin: bool,
    pub screenshot: Option<ScreenshotRequest>,
    pub snapshot: Option<PathBuf>,
    pub restore: Option<PathBuf>,
}
