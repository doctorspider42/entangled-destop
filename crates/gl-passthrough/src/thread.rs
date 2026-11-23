//! The thread the host's OpenGL lives on.
//!
//! OpenGL contexts belong to threads, and the guest's buffers arrive on
//! whichever vCPU thread its driver rang the doorbell from. So one thread
//! owns the contexts, and a vCPU thread hands its buffer over and waits for
//! the answer — the guest's driver is waiting for it anyway.

use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::exec::Executor;
use crate::wgl::Wgl;
use crate::Outcome;

enum Job {
    Run(Vec<u32>, mpsc::Sender<Outcome>),
    Reset,
    Stop,
}

/// The host has no usable OpenGL.
#[derive(Debug, thiserror::Error)]
#[error("host OpenGL unavailable: {0}")]
pub struct StartError(String);

/// The OpenGL thread; dropping it destroys every context and joins it.
pub struct GlThread {
    jobs: mpsc::Sender<Job>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for GlThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlThread").finish_non_exhaustive()
    }
}

impl GlThread {
    /// Starts the thread and its OpenGL; fails if the host has none that
    /// can do what a guest context needs (framebuffer objects, above all).
    pub fn start() -> Result<Self, StartError> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (ready, started) = mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("gl-passthrough".into())
            .spawn(move || {
                let mut executor = match Wgl::new() {
                    Ok(wgl) => {
                        let _ = ready.send(Ok(()));
                        Executor::new(wgl)
                    }
                    Err(why) => {
                        let _ = ready.send(Err(why));
                        return;
                    }
                };
                for job in inbox {
                    match job {
                        Job::Run(words, answer) => {
                            let _ = answer.send(executor.run(&words));
                        }
                        Job::Reset => executor.reset(),
                        Job::Stop => break,
                    }
                }
                executor.reset();
            })
            .map_err(|e| StartError(e.to_string()))?;
        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs,
                thread: Some(thread),
            }),
            Ok(Err(why)) => Err(StartError(why)),
            Err(_) => Err(StartError("the OpenGL thread died".into())),
        }
    }

    /// Runs one buffer and waits for what it produced. A dead OpenGL thread
    /// produces nothing.
    pub fn run(&self, words: Vec<u32>) -> Outcome {
        let (answer, outcome) = mpsc::channel();
        if self.jobs.send(Job::Run(words, answer)).is_err() {
            return Outcome::default();
        }
        outcome.recv().unwrap_or_default()
    }

    /// Destroys every context: the guest was reset.
    pub fn reset(&self) {
        let _ = self.jobs.send(Job::Reset);
    }
}

impl Drop for GlThread {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
