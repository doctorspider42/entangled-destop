//! Isolate CPU scanout uploads and native window presentation from a guest GPU.
//! Run `cargo run -p display --example memory_soak -- 300` for a bounded soak.
use display::{ControlEvent, DisplayConfig, DisplayHost};
use std::io::BufRead;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let seconds = std::env::args()
        .nth(1)
        .map(|arg| arg.parse::<u64>())
        .transpose()?
        .unwrap_or(300);
    if !(1..=3600).contains(&seconds) {
        return Err("duration must be 1..=3600 seconds".into());
    }
    let host = DisplayHost::new(DisplayConfig {
        width: 2048,
        height: 1536,
        scale: 1.0,
    })?
    .with_title("Entangled display memory soak (no VM)")
    .with_follow_window(false);
    let display = host.handle();
    let control = host.control_queue();
    let phase = match std::env::args().nth(2).as_deref() {
        None => None,
        Some("saving") => Some(display::LifecyclePhase::Saving),
        Some("restoring") => Some(display::LifecyclePhase::Restoring),
        Some(_) => return Err("preview phase must be saving or restoring".into()),
    };
    display.set_lifecycle_progress(phase);
    let running = Arc::new(AtomicBool::new(true));
    let stop_display = display.clone();
    let stop_running = running.clone();
    // A process-memory monitor can stop this disposable probe cleanly.
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if matches!(line.as_deref(), Ok("stop")) {
                stop_running.store(false, Ordering::Release);
                stop_display.shutdown();
                break;
            }
        }
    });
    let worker_running = running.clone();
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let mut pixels = vec![0x40; 2048 * 1536 * 4];
        let mut frames = 0u64;
        while worker_running.load(Ordering::Acquire)
            && started.elapsed() < Duration::from_secs(seconds)
        {
            if control.drain().iter().any(|event| {
                matches!(
                    event,
                    ControlEvent::QuitRequested | ControlEvent::WindowCloseRequested
                )
            }) {
                break;
            }
            let frame_start = Instant::now();
            // Calm image, but each frame uploads the entire 12 MiB plane.
            pixels[0] = frames as u8;
            if let Err(error) = display.update_scanout(0, 0, 2048, 1536, &pixels) {
                tracing::error!(%error, "display soak upload failed");
                display.shutdown();
                return Err(error);
            }
            display.request_redraw();
            frames += 1;
            std::thread::sleep(Duration::from_millis(16).saturating_sub(frame_start.elapsed()));
        }
        tracing::info!(frames, elapsed = ?started.elapsed(), "display memory soak finished");
        display.shutdown();
        Ok::<(), display::DisplayError>(())
    });
    let result = host.run();
    running.store(false, Ordering::Release);
    worker
        .join()
        .map_err(|_| std::io::Error::other("display soak worker panicked"))??;
    result?;
    Ok(())
}
