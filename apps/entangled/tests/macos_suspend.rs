//! Opt-in native end-to-end suspend acceptance, without external guest images.
//! Sign the engine with tools/macos-probe/entitlements.plist, then set
//! ENTANGLED_NATIVE_ACCEPTANCE_ENGINE and run this test with --ignored.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

struct Driver {
    child: Child,
    lines: Receiver<String>,
}
impl Driver {
    fn start(args: &[&str]) -> Self {
        let engine = std::env::var_os("ENTANGLED_NATIVE_ACCEPTANCE_ENGINE")
            .expect("set ENTANGLED_NATIVE_ACCEPTANCE_ENGINE to a signed native engine");
        let mut child = Command::new(engine)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.unwrap();
                eprintln!("guest: {line}");
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }
    fn send(&mut self, command: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
    }
    fn success(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "engine did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn save(&mut self, path: &Path, first: Option<u8>, refuse_first: bool) -> u8 {
        let mut last = None;
        let mut refused = !refuse_first;
        let mut requested = false;
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect("engine stopped or stalled");
            if line.starts_with("entangled-control: error save ") {
                assert!(refuse_first);
                assert!(line.contains("session kept Running"));
                refused = true;
                requested = false;
            } else if line.starts_with("entangled-control: saved ") {
                self.success();
                return last.unwrap() + 1;
            } else if line.len() == 1 && line.as_bytes()[0].is_ascii_uppercase() {
                let value = line.as_bytes()[0];
                if let Some(last) = last {
                    assert_eq!(value, last + 1);
                } else if let Some(first) = first {
                    assert_eq!(value, first, "guest restarted instead of resuming");
                }
                last = Some(value);
                if !requested {
                    if refused {
                        self.send(&format!("save {}", path.display()));
                    } else {
                        self.send(&format!(
                            "save {}/absent/session.esnap",
                            path.parent().unwrap().display()
                        ));
                    }
                    requested = true;
                }
            }
        }
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        // Only our disposable headless child; always reap it on assertion failure.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires a signed native HVF engine; see module instructions"]
fn refused_suspend_and_two_restores_continue_exact_heartbeat_successors() {
    acceptance(false);
}

#[test]
#[ignore = "requires a signed native HVF engine; see module instructions"]
fn active_smp_survives_two_suspend_restore_cycles() {
    acceptance(true);
}

fn acceptance(active: bool) {
    let dir = std::env::temp_dir().join(format!(
        "entangled-arm-suspend-{}-{active}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("probe.Image");
    // ARM64 Image header, then: print an incrementing character via PL011;
    // retain its successor in RAM; wait 2.4M virtual-counter ticks; repeat;
    // PSCI SYSTEM_OFF after T. Secondary CPUs remain powered off, but must
    // still participate in both snapshot gates and be restored as off.
    let words: &[u32] = &[
        0x14000010, 0, 0, 0, 0x1000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x644d5241, 0, 0xd2800833,
        0xd2a12014, 0x39000293, 0x52800141, 0x39000281, 0x91000673, 0xd2a80015, 0xf90202b3,
        0xd53be056, 0xd293e017, 0xf2a00497, 0x8b1702d6, 0xd53be058, 0xeb16031f, 0x54ffffc3,
        0xf101567f, 0x54fffe43, 0xd2800100, 0xf2b08000, 0xd4000002,
    ];
    let words = if active {
        &[
            0x14000010, 0x0, 0x0, 0x0, 0x1000, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
            0x644d5241, 0x0, 0xd2800021, 0x10000382, 0xaa0103e3, 0xd2800060, 0xf2b88000,
            0xd4000002, 0x91000421, 0xf100103f, 0x54ffff23, 0xd2800833, 0xd2a12014, 0x39000293,
            0x52800141, 0x39000281, 0x91000673, 0xd2a80015, 0xf90202b3, 0xd53be056, 0xd293e017,
            0xf2a00497, 0x8b1702d6, 0xd53be058, 0xeb16031f, 0x54ffffc3, 0xf101567f, 0x54fffe43,
            0xd2800100, 0xf2b08000, 0xd4000002, 0xd378dc13, 0xd2a80014, 0x91400a94, 0x8b000e94,
            0x91000673, 0xf9000293, 0x17fffffe,
        ][..]
    } else {
        words
    };
    std::fs::write(
        &image,
        words
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let profile = dir.join("probe.toml");
    std::fs::write(
        &profile,
        format!(
            r#"
name = "native-suspend-probe"
architecture = "aarch64"
memory_mib = 128
vcpus = 4
transport = "mmio"
[boot]
mode = "direct-linux"
kernel = {:?}
cmdline = "console=ttyAMA0"
[display]
width = 640
height = 480
"#,
            image
        ),
    )
    .unwrap();
    let snapshot = dir.join("session with spaces.esnap");
    let path = snapshot.to_str().unwrap();
    let mut cold = Driver::start(&[
        "run",
        profile.to_str().unwrap(),
        "--headless",
        "--control-stdin",
        "--snapshot",
        path,
    ]);
    let first = cold.save(&snapshot, Some(b'A'), true);
    assert!(!dir.join("session with spaces.esnap.part").exists());
    let info = vm_snapshot::inspect(&snapshot).unwrap();
    assert_eq!(info.host, vm_snapshot::HostKind::HvfMacos);
    assert!(info.restorable_here);
    let mut saved =
        vm_snapshot::SnapshotReader::open(std::fs::File::open(&snapshot).unwrap()).unwrap();
    assert_eq!(
        saved.instances(vm_snapshot::SectionKind::Cpu),
        vec![0, 1, 2, 3]
    );
    if active {
        for index in 1..4 {
            let bytes = saved
                .read_section(vm_snapshot::SectionKind::Cpu, index)
                .unwrap();
            let cpu = vm_snapshot::arm64::decode_cpu(&bytes).unwrap();
            assert!(
                cpu.registers[19] > 256 * u64::from(index),
                "secondary CPU never ran"
            );
        }
    }
    // Resume must use saved RAM rather than re-read boot images.
    std::fs::remove_file(image).unwrap();
    let mut resumed = Driver::start(&["resume", path, "--headless", "--control-stdin"]);
    let second = resumed.save(&snapshot, Some(first), false);
    assert!(second > first);
    let mut last = Driver::start(&["resume", path, "--headless", "--control-stdin"]);
    let mut expected = second;
    while let Ok(line) = last.lines.recv_timeout(Duration::from_secs(30)) {
        if line.len() == 1 && line.as_bytes()[0].is_ascii_uppercase() {
            assert_eq!(line.as_bytes()[0], expected);
            expected += 1;
        }
    }
    assert_eq!(expected, b'U');
    last.success();
    std::fs::remove_dir_all(dir).unwrap();
}
