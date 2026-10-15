//! Host-independent choices for launching a configured VM.
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    pub after: Duration,
    pub path: PathBuf,
    pub burst: Option<Duration>,
}

#[derive(Debug, Default)]
pub struct RunOptions {
    pub headless: bool,
    pub control_stdin: bool,
    pub screenshot: Option<ScreenshotRequest>,
    pub snapshot: Option<PathBuf>,
    pub restore: Option<PathBuf>,
}
