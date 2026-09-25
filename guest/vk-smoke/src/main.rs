//! vk-smoke: a guest-side Vulkan smoke test.
//!
//! Runs nine numbered checks, in dependency order, and prints one line each:
//!
//! ```text
//! SMOKE <n> <name> PASS|FAIL|SKIP <detail>
//! SMOKE DONE pass=<p> fail=<f> skip=<s>
//! ```
//!
//! Lines starting with `#` are extra diagnostics. The exit status is 0 when
//! nothing failed, 1 when something did, 2 when the watchdog ended a hung run.
//! A failing check never stops a later one that does not depend on it; one
//! that does is reported as SKIP with the reason. See README.md.

mod checks;
mod gpu;
mod raster;

use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use checks::Out;
use gpu::Opts;

const CHECKS: [(u32, &str); 9] = [
    (1, "instance"),
    (2, "device"),
    (3, "host-memory"),
    (4, "transfer"),
    (5, "compute"),
    (6, "graphics"),
    (7, "dynamic-rendering"),
    (8, "timeline-sync"),
    (9, "many-submits"),
];

fn name(id: u32) -> &'static str {
    CHECKS.iter().find(|c| c.0 == id).map_or("?", |c| c.1)
}

static PASS: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);
static SKIP: AtomicU32 = AtomicU32::new(0);
/// The check in progress (0: none) and when it started, for the watchdog.
static CURRENT: AtomicU32 = AtomicU32::new(0);
static STARTED_MS: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pass,
    Fail,
    Skip,
}

fn report(id: u32, status: Status, detail: &str) {
    let word = match status {
        Status::Pass => {
            PASS.fetch_add(1, Ordering::SeqCst);
            "PASS"
        }
        Status::Fail => {
            FAIL.fetch_add(1, Ordering::SeqCst);
            "FAIL"
        }
        Status::Skip => {
            SKIP.fetch_add(1, Ordering::SeqCst);
            "SKIP"
        }
    };
    // One line per check, whatever the detail contains.
    let detail = detail.replace(['\n', '\r'], " ");
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "SMOKE {id} {} {word} {detail}", name(id));
    let _ = out.flush();
}

fn done() -> i32 {
    let (p, f, s) = (
        PASS.load(Ordering::SeqCst),
        FAIL.load(Ordering::SeqCst),
        SKIP.load(Ordering::SeqCst),
    );
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "SMOKE DONE pass={p} fail={f} skip={s}");
    let _ = out.flush();
    i32::from(f > 0)
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic)".into()
    }
}

/// Runs one check under the watchdog and catch_unwind; returns the value a
/// passing check hands to its dependents.
fn run<T>(id: u32, f: impl FnOnce() -> Result<(Out, Option<T>), String>) -> Option<T> {
    STARTED_MS.store(now_ms(), Ordering::SeqCst);
    CURRENT.store(id, Ordering::SeqCst);
    let t0 = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(f));
    CURRENT.store(0, Ordering::SeqCst);
    let took = format!("[{:.0} ms]", t0.elapsed().as_secs_f64() * 1000.0);
    match result {
        Ok(Ok((Out::Pass(detail), value))) => {
            report(id, Status::Pass, &format!("{detail} {took}"));
            value
        }
        Ok(Ok((Out::Skip(reason), _))) => {
            report(id, Status::Skip, &reason);
            None
        }
        Ok(Err(diagnosis)) => {
            report(id, Status::Fail, &format!("{diagnosis} {took}"));
            None
        }
        Err(payload) => {
            report(
                id,
                Status::Fail,
                &format!("panicked: {} {took}", panic_text(payload)),
            );
            None
        }
    }
}

/// A hung driver call (vkQueueWaitIdle has no timeout, and a broken renderer
/// can block any call) must still end in a complete, parseable report.
fn start_watchdog(limit: Duration) {
    let limit_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
    let spawned = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            let id = CURRENT.load(Ordering::SeqCst);
            if id == 0 {
                continue;
            }
            let elapsed = now_ms().saturating_sub(STARTED_MS.load(Ordering::SeqCst));
            if elapsed > limit_ms && CURRENT.load(Ordering::SeqCst) == id {
                report(
                    id,
                    Status::Fail,
                    &format!(
                        "watchdog: no result after {} s, the run is ended here",
                        elapsed / 1000
                    ),
                );
                for (later, _) in CHECKS.iter().filter(|c| c.0 > id) {
                    report(
                        *later,
                        Status::Skip,
                        &format!("watchdog ended the run during check {id}"),
                    );
                }
                done();
                std::process::exit(2);
            }
        });
    if let Err(e) = spawned {
        println!("# cannot start the watchdog thread ({e}); a hang will not be reported");
    }
}

const USAGE: &str =
    "usage: vk-smoke [--device-index N] [--allow-cpu] [--checks 3,5,6] [--timeout-secs N]
                [--api-cap 1.2] [--repeat N]

  --device-index N   test physical device N (as listed in the '# device[N]' lines),
                     CPU devices included                 env VK_SMOKE_DEVICE_INDEX
  --allow-cpu        let automatic selection pick a CPU device (lavapipe/llvmpipe)
                     when nothing else is present        env VK_SMOKE_ALLOW_CPU=1
  --checks LIST      run only these of checks 3..9 (1 and 2 always run)
                                                          env VK_SMOKE_CHECKS
  --timeout-secs N   per-wait GPU timeout, default 10; the watchdog ends a check
                     that makes no progress for 2N+10 s   env VK_SMOKE_TIMEOUT_SECS
  --api-cap M.N      request at most Vulkan M.N (default 1.3), to take the 1.2 or 1.1
                     code paths (KHR extensions) on a newer device
                                                          env VK_SMOKE_API_CAP
  --repeat N         checks 4-7 submit their work N times (default 10) and then N
                     empty command buffers; each is timed by wall clock and by
                     GPU timestamps, the first apart from the warm rest
                                                          env VK_SMOKE_REPEAT";

fn parse_opts() -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut device_index = env("VK_SMOKE_DEVICE_INDEX");
    let mut allow_cpu = env("VK_SMOKE_ALLOW_CPU").is_some_and(|v| v != "0");
    let mut checks = env("VK_SMOKE_CHECKS");
    let mut timeout = env("VK_SMOKE_TIMEOUT_SECS");
    let mut api_cap = env("VK_SMOKE_API_CAP");
    let mut repeat = env("VK_SMOKE_REPEAT");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "--device-index" => device_index = Some(value("--device-index")?),
            "--allow-cpu" => allow_cpu = true,
            "--checks" => checks = Some(value("--checks")?),
            "--timeout-secs" => timeout = Some(value("--timeout-secs")?),
            "--api-cap" => api_cap = Some(value("--api-cap")?),
            "--repeat" => repeat = Some(value("--repeat")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let device_index = device_index
        .map(|v| {
            v.trim()
                .parse::<usize>()
                .map_err(|e| format!("bad device index {v:?}: {e}"))
        })
        .transpose()?;
    let checks = checks
        .map(|list| {
            list.split(',')
                .map(|n| {
                    n.trim()
                        .parse::<u32>()
                        .map_err(|e| format!("bad check number {n:?}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let secs = timeout
        .map(|v| {
            v.trim()
                .parse::<u64>()
                .map_err(|e| format!("bad timeout {v:?}: {e}"))
        })
        .transpose()?
        .unwrap_or(10);
    let api_cap = api_cap
        .map(|v| {
            let parsed = v
                .trim()
                .split_once('.')
                .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)));
            parsed.ok_or_else(|| format!("bad --api-cap {v:?}, want MAJOR.MINOR like 1.2"))
        })
        .transpose()?;
    let repeat = repeat
        .map(|v| {
            v.trim()
                .parse::<u32>()
                .map_err(|e| format!("bad repeat count {v:?}: {e}"))
        })
        .transpose()?
        .unwrap_or(10)
        .max(1);
    Ok(Opts {
        api_cap,
        repeat,
        device_index,
        allow_cpu,
        timeout: Duration::from_secs(secs.max(1)),
        checks,
    })
}

fn main() {
    EPOCH.get_or_init(Instant::now);
    let opts = match parse_opts() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vk-smoke: {e}\n{USAGE}");
            std::process::exit(64);
        }
    };
    println!(
        "# vk-smoke {} ({} {}), timeout {} s",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        opts.timeout.as_secs()
    );
    start_watchdog(opts.timeout * 2 + Duration::from_secs(10));

    let picked = run(1, || {
        gpu::check_instance(&opts).map(|(d, p)| (Out::Pass(d), Some(p)))
    });
    let gpu = match &picked {
        Some(p) => run(2, || {
            gpu::check_device(p, &opts).map(|(d, g)| (Out::Pass(d), Some(g)))
        }),
        None => {
            report(
                2,
                Status::Skip,
                "needs check 1 (no instance or no usable physical device)",
            );
            None
        }
    };

    type Check = fn(&gpu::Gpu) -> Result<Out, String>;
    let later: [(u32, Check); 7] = [
        (3, checks::host_memory),
        (4, checks::transfer),
        (5, checks::compute),
        (6, checks::graphics),
        (7, checks::dynamic_rendering),
        (8, checks::sync),
        (9, checks::many_submits),
    ];
    for (id, check) in later {
        if opts.checks.as_ref().is_some_and(|list| !list.contains(&id)) {
            report(id, Status::Skip, "not selected (--checks)");
            continue;
        }
        match &gpu {
            None => report(id, Status::Skip, "needs check 2 (no device)"),
            Some(g) => {
                run::<()>(id, || check(g).map(|out| (out, None)));
            }
        }
    }

    // Tear down: the device (and its pool) first, then the instance — unless
    // a wait timed out, in which case objects the GPU may still use are left
    // for process exit to reclaim.
    let hung = gpu.as_ref().is_some_and(|g| g.hung.get());
    drop(gpu);
    if let Some(p) = picked {
        if !hung {
            // SAFETY: the device, the only child of the instance, is gone.
            unsafe { p.instance.destroy_instance(None) };
        }
        // Never unload the loader: after a hang, driver threads may still be
        // running inside it, and at exit there is nothing to gain.
        std::mem::forget(p.entry);
    }
    std::process::exit(done());
}
