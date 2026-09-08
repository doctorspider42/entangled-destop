//! The gamepad acceptance on a real guest (backlog GAME-2104).
//!
//! What these prove that a unit test cannot: that a **Linux kernel** accepts
//! the descriptor. The device's config space is exhaustively unit-tested in
//! `virtio-input` and the ring is covered by `crates/virtio-input/tests/
//! input_queue.rs` — but neither of them can tell you whether the input core
//! registered the thing as a joystick, whether `joydev` bound it, or whether
//! the `ABS_INFO` ranges survived the round trip through `virtinput_cfg_abs()`.
//! Those are the questions that decide whether a game in the guest can use the
//! pad at all, and only a guest kernel can answer them.
//!
//! Two boots, because the pad has two halves:
//!
//! 1. **The descriptor**, driven by events the test writes straight into the
//!    device's [`virtio_input::InputHandle`]. Exact sequence in, exact
//!    sequence asserted out.
//! 2. **Hotplug**, driven by a scripted [`virtio_input::GamepadSource`] behind
//!    the production [`virtio_input::GamepadCapture`] pump — the half a direct
//!    push skips, and the only place "a controller went away" exists.
//!
//! Neither boot ever touches a real controller, on purpose: a machine with a
//! pad plugged into it and a machine without must produce identical results,
//! or the acceptance is measuring the developer's desk.
//!
//! Self-skips without `/dev/kvm`, without the guest artifacts, or without the
//! project's own bootstrap kernel (see [`boot_tests::bootstrap_kernel`] — the
//! Debian-installer fallback has no built-in `joydev`).

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use boot_tests::{
    boot_once_driven, bootstrap_kernel, kvm_available, test_initramfs, BootSpec, VmHandle,
};
use control_api::VirtioTransport;
use virtio_input::{abs, btn, ev, GamepadSource, InputEvent, PadId, PadState, Poll, SourceFactory};

/// How long to wait for the guest to open its event node and say so.
const READY: Duration = Duration::from_secs(45);

/// Kernel, initramfs and the pci transport every boot here shares.
///
/// pci rather than the harness's mmio default for one practical reason: it is
/// what a real `entangled run` uses for anything that boots through UEFI, and
/// the descriptor is transport-independent, so testing it on the bus a player
/// will actually have costs nothing.
fn spec() -> Option<BootSpec> {
    if !kvm_available() {
        return None;
    }
    let (Some(kernel), Some(initramfs)) = (bootstrap_kernel(), test_initramfs()) else {
        eprintln!(
            "skipping: this test needs the project's own bootstrap kernel (CONFIG_INPUT_JOYDEV) \
             — run guest/bootstrap-kernel/build.sh and scripts/build-test-initramfs.sh"
        );
        return None;
    };
    Some(BootSpec::new(kernel, initramfs).with_transport(VirtioTransport::Pci))
}

/// Every `padinfo` assertion, shared by both boots: this is the descriptor,
/// read back out of the guest kernel that consumed it.
fn assert_the_kernel_read_our_descriptor(info: &[(String, String)], serial: &str) {
    let field = |key: &str| {
        info.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("padinfo has no {key}:\n{serial}"))
    };

    assert_eq!(field("name"), "Entangled_Gamepad");
    // The ids the guest reads back out of `VIRTIO_INPUT_CFG_ID_DEVIDS`.
    assert_eq!(field("bus"), "0006", "BUS_VIRTUAL");
    assert_eq!(field("vendor"), "564d");
    assert_eq!(field("product"), "0003");

    // The whole point of the layout: `joydev` bound it, so the pad is a
    // joystick to userspace and not merely an input device — and the node it
    // promised really opens.
    //
    // Guarded by `joydev=`, which says whether the guest kernel has the
    // handler at all. A checkout whose `artifacts/bootstrap/vmlinuz` predates
    // `CONFIG_INPUT_JOYDEV` would otherwise fail here and blame the
    // descriptor, which is the one conclusion the evidence does not support.
    if field("joydev") == "1" {
        assert_eq!(
            field("js"),
            "1",
            "joydev did not claim the pad, so there is no /dev/input/js*; handlers={}",
            field("handlers")
        );
        assert!(
            field("jsnode").starts_with("js"),
            "joydev claimed the pad but named no js node: {}",
            field("jsnode")
        );
        assert_eq!(
            field("jsopen"),
            "1",
            "/dev/input/{} exists in the handler list but does not open",
            field("jsnode")
        );
    } else {
        eprintln!(
            "NOT CHECKED: this guest kernel has no joydev handler, so /dev/input/js* cannot \
             exist for any device — rebuild artifacts/bootstrap/vmlinuz with \
             guest/bootstrap-kernel/build.sh, which now asserts CONFIG_INPUT_JOYDEV=y"
        );
    }

    // Every button and axis the device advertised, registered by the input
    // core — not one more (a stray code would change what SDL guesses) and not
    // one fewer (a dropped one is a button no game can bind).
    assert_eq!(field("keys"), btn::GAMEPAD.len().to_string());
    assert_eq!(field("axes"), "8");

    // The three axis shapes, read back through `EVIOCGABS`: this is our
    // `ABS_INFO` having survived the trip, negative minimum and all.
    assert_eq!(field("absx"), "-32768:32767:16:128", "stick");
    assert_eq!(field("absz"), "0:255:0:0", "trigger");
    assert_eq!(field("abshat"), "-1:1:0:0", "hat");
}

/// Echoes the guest's own two lines to stdout, so `--nocapture` prints the
/// evidence rather than only the verdict. What the kernel concluded about a
/// descriptor is worth reading even when the assertions pass.
fn quote_the_guest(outcome: &boot_tests::BootOutcome) {
    for line in outcome
        .serial
        .lines()
        .filter(|line| line.contains("VMHOST_TEST_OK pad"))
    {
        println!("guest: {}", line.trim());
    }
}

/// The `seq=`, `events=` and `syn=` fields of a `padprobe` line.
fn echoed(outcome: &boot_tests::BootOutcome) -> (Vec<String>, usize, usize) {
    let probe = outcome
        .probe("padprobe")
        .unwrap_or_else(|| panic!("no padprobe line:\n{}", outcome.serial));
    let value = |key: &str| {
        probe
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let seq = match value("seq").as_str() {
        "" | "none" => Vec::new(),
        list => list.split(',').map(str::to_string).collect(),
    };
    let count = |key: &str| value(key).parse::<usize>().unwrap_or(usize::MAX);
    (seq, count("events"), count("syn"))
}

// ---------------------------------------------------------------- the descriptor

/// The sequence the host injects, as four separate reports — a press, a stick
/// sweep, a trigger and hat, and the release. Four rather than one because a
/// game reads reports, not events: what has to survive the trip is the
/// *grouping*, and a single batch could not show that.
fn injected() -> Vec<Vec<InputEvent>> {
    let key = |code, value| InputEvent {
        event_type: ev::KEY,
        code,
        value,
    };
    let axis = |code, value: i32| InputEvent {
        event_type: ev::ABS,
        code,
        value: value as u32,
    };
    vec![
        vec![key(btn::SOUTH, 1), InputEvent::SYN_REPORT],
        // Both extremes of a signed axis in one report: the values a kernel
        // that read our `ABS_INFO` as unsigned would clamp or drop.
        vec![
            axis(abs::X, -32768),
            axis(abs::Y, 32767),
            InputEvent::SYN_REPORT,
        ],
        vec![
            axis(abs::Z, 255),
            axis(abs::HAT0X, 1),
            axis(abs::HAT0Y, -1),
            InputEvent::SYN_REPORT,
        ],
        vec![key(btn::SOUTH, 0), InputEvent::SYN_REPORT],
    ]
}

/// Events the guest should echo, excluding the `SYN_REPORT`s it counts
/// separately, in the probe's own notation.
fn expected_echo() -> Vec<String> {
    vec![
        format!("k{:x}=1", btn::SOUTH),
        format!("a{:x}=-32768", abs::X),
        format!("a{:x}=32767", abs::Y),
        format!("a{:x}=255", abs::Z),
        format!("a{:x}=1", abs::HAT0X),
        format!("a{:x}=-1", abs::HAT0Y),
        format!("k{:x}=0", btn::SOUTH),
    ]
}

/// Pushes the sequence once the guest says its event node is open.
fn drive_injection(vm: VmHandle) {
    let Some(pad) = vm.gamepad.clone() else {
        vm.finish();
        return;
    };
    // evdev only buffers for clients that already exist, so injecting before
    // the guest has opened the node would lose the events with no trace. The
    // probe prints `padinfo` *after* the open precisely so this is a handshake
    // and not a sleep.
    if vm.wait_for("VMHOST_TEST_OK padinfo ", 1, READY) == 0 {
        vm.finish();
        return;
    }
    for report in injected() {
        if let Err(error) = pad.push(&report) {
            eprintln!("harness: gamepad push failed: {error}");
            break;
        }
        // A real controller reports at ~125 Hz; keeping the reports apart is
        // what makes "the grouping survived" a meaningful assertion.
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for_the_probe(&vm);
}

/// Waits for the guest's own result line, then releases the boot.
fn wait_for_the_probe(vm: &VmHandle) {
    let deadline = Instant::now() + READY;
    while Instant::now() < deadline && vm.count("VMHOST_TEST_OK padprobe ") == 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    vm.finish();
}

/// One boot, three questions: did the kernel register the pad as a joystick,
/// did it read the descriptor we published, and does an event injected on the
/// host reach a guest reading `/dev/input/event*`?
#[test]
fn a_guest_kernel_registers_the_pad_as_a_joystick_and_events_round_trip() {
    let Some(spec) = spec() else { return };
    // A ceiling well above the eleven that are coming: the guest stops on
    // silence, so an extra event the device invented would land in `seq=`
    // instead of going unnoticed.
    let spec = spec.with_gamepad_probe(32);
    let outcome = boot_once_driven(&spec, Some(Box::new(drive_injection))).expect("the VM boots");
    assert!(
        outcome.reached_ready(),
        "guest never became ready:\n{}",
        outcome.serial
    );

    quote_the_guest(&outcome);
    let info = outcome
        .probe("padinfo")
        .unwrap_or_else(|| panic!("no padinfo line:\n{}", outcome.serial));
    assert_the_kernel_read_our_descriptor(&info, &outcome.serial);

    // ---- and the round trip ---------------------------------------------
    let (seq, events, syn) = echoed(&outcome);
    let expected = expected_echo();
    assert_eq!(
        seq, expected,
        "the guest echoed a different sequence than the host injected:\n{}",
        outcome.serial
    );
    assert_eq!(events, expected.len());
    // Four reports in, four SYN_REPORTs out: the grouping survived, so the
    // guest sees four discrete pad states rather than one smeared one.
    assert_eq!(syn, injected().len());
}

// -------------------------------------------------------------------- hotplug

/// A controller that is not there, then is, then is not, then is a *different*
/// one — the four states GAME-2104's hotplug claim is made of, scripted.
///
/// It stays disconnected until `armed`, because the guest has to have opened
/// its event node first: evdev buffers only for clients that already exist, so
/// a plug-in the guest was not yet listening for would vanish without trace.
struct ScriptedPad {
    armed: Arc<AtomicBool>,
    step: Arc<AtomicUsize>,
    /// Polls served in the current step, so each state is held long enough for
    /// the pump to have certainly seen it.
    held: usize,
}

/// Polls one script step is held for. The pump polls at
/// `virtio_input::gamepad::POLL_INTERVAL` (8 ms), so this is ~200 ms a step —
/// far longer than one report, and the guest's own quiet window is 1.5 s.
const POLLS_PER_STEP: usize = 25;

impl ScriptedPad {
    fn new(armed: Arc<AtomicBool>, step: Arc<AtomicUsize>) -> Self {
        Self {
            armed,
            step,
            held: 0,
        }
    }

    /// Pad A: the south button held and the D-pad pushed right.
    fn pad_a() -> PadState {
        let mut state = PadState::NEUTRAL;
        assert!(state.set_button_by_code(btn::SOUTH, true));
        state.hat = (1, 0);
        state
    }

    /// Pad B, plugged in after A was pulled out: a different button, so the
    /// guest could not mistake it for A still being there.
    fn pad_b() -> PadState {
        let mut state = PadState::NEUTRAL;
        assert!(state.set_button_by_code(btn::START, true));
        state
    }
}

impl GamepadSource for ScriptedPad {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn poll(&mut self, timeout: Duration) -> Poll {
        // Every source paces the pump; a scripted one is no exception, or the
        // capture thread spins a core for the length of the test.
        std::thread::sleep(timeout);
        if !self.armed.load(Ordering::Acquire) {
            return Poll::Disconnected;
        }
        let step = self.step.load(Ordering::Acquire);
        self.held += 1;
        if self.held >= POLLS_PER_STEP {
            self.held = 0;
            self.step.store(step + 1, Ordering::Release);
        }
        let id = |label: &str, slot| PadId {
            label: label.to_owned(),
            slot,
        };
        match step {
            // Nothing plugged in yet, with the guest already listening: this
            // step is what makes "an absent pad produces no events at all" an
            // observation rather than an assumption.
            0 => Poll::Disconnected,
            1 => Poll::Connected {
                id: id("scripted pad A", 0),
                state: Self::pad_a(),
            },
            // Unplugged. The pump owes the guest a release of everything A was
            // holding, and then silence.
            2 => Poll::Disconnected,
            3 => Poll::Connected {
                id: id("scripted pad B", 1),
                state: Self::pad_b(),
            },
            // …and B stays plugged in, reporting the same state for ever. A
            // source that repeats itself must produce nothing: that is the
            // "does not spam the guest" half of the claim.
            _ => Poll::Connected {
                id: id("scripted pad B", 1),
                state: Self::pad_b(),
            },
        }
    }
}

/// What the guest must see: A's press, A's release on the unplug, then B's.
fn expected_hotplug_echo() -> Vec<String> {
    vec![
        format!("k{:x}=1", btn::SOUTH),
        format!("a{:x}=1", abs::HAT0X),
        format!("k{:x}=0", btn::SOUTH),
        format!("a{:x}=0", abs::HAT0X),
        format!("k{:x}=1", btn::START),
    ]
}

/// A controller that appears and disappears while the guest is running must
/// leave the guest in a sane state and then be quiet.
///
/// This is the only test that runs the real [`virtio_input::GamepadCapture`]
/// pump against a guest: the state diff, the `PadState::NEUTRAL` an unplug
/// turns into, and the pause gate are all in the path. What it asserts is the
/// two failure modes a naive event-forwarding design has — a button left stuck
/// down when its controller is yanked, and a stream of no-op reports once
/// there is nothing to report.
#[test]
fn a_controller_can_come_and_go_without_wedging_the_pad_or_spamming_the_guest() {
    let Some(spec) = spec() else { return };
    let armed = Arc::new(AtomicBool::new(false));
    let step = Arc::new(AtomicUsize::new(0));
    let factory: SourceFactory = {
        let (armed, step) = (Arc::clone(&armed), Arc::clone(&step));
        Arc::new(move || Box::new(ScriptedPad::new(Arc::clone(&armed), Arc::clone(&step))))
    };
    // Sixteen is roughly twice what the script produces, so the events that
    // must *not* exist have room to show up.
    let spec = spec.with_gamepad_capture(factory, 16);

    let drive_armed = Arc::clone(&armed);
    let outcome = boot_once_driven(
        &spec,
        Some(Box::new(move |vm: VmHandle| {
            if vm.wait_for("VMHOST_TEST_OK padinfo ", 1, READY) == 0 {
                vm.finish();
                return;
            }
            // The guest is listening; let the script run.
            drive_armed.store(true, Ordering::Release);
            wait_for_the_probe(&vm);
        })),
    )
    .expect("the VM boots");
    assert!(
        outcome.reached_ready(),
        "guest never became ready:\n{}",
        outcome.serial
    );

    quote_the_guest(&outcome);
    let info = outcome
        .probe("padinfo")
        .unwrap_or_else(|| panic!("no padinfo line:\n{}", outcome.serial));
    assert_the_kernel_read_our_descriptor(&info, &outcome.serial);

    let (seq, events, syn) = echoed(&outcome);
    let expected = expected_hotplug_echo();
    assert_eq!(
        seq, expected,
        "a plug-in, an unplug and a swap did not reach the guest as three clean reports:\n{}",
        outcome.serial
    );
    assert_eq!(events, expected.len());
    // Exactly three reports: A arriving, A leaving, B arriving. Not one per
    // poll of a pad that is not there, and not one per poll of a pad whose
    // state has not changed.
    assert_eq!(
        syn, 3,
        "the pad reported {syn} times for three state changes:\n{}",
        outcome.serial
    );
    // The script ran past step 3 for as long as the guest kept listening, so
    // "B is still plugged in and still holding START" was polled many times
    // over. None of that may have reached the guest.
    assert!(
        step.load(Ordering::Acquire) >= 4,
        "the script never got past the swap, so the quiet half was not tested"
    );
}
