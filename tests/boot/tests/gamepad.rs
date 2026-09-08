//! The gamepad acceptance on a real guest (backlog GAME-2104).
//!
//! What this proves that a unit test cannot: that a **Linux kernel** accepts
//! the descriptor. The device's config space is exhaustively unit-tested in
//! `virtio-input`, and the malicious-guest tests in `tests/gamepad_queue.rs`
//! cover the ring — but neither of them can tell you whether the input core
//! registered the thing as a joystick, whether `joydev` bound it, or whether
//! the `ABS_INFO` ranges survived the round trip through
//! `virtinput_cfg_abs()`. Those are the questions that decide whether a game
//! in the guest can use the pad at all, and only a guest kernel can answer
//! them.
//!
//! The pad attached here has **no host capture**: every event comes from this
//! test. A machine with a controller plugged in and a machine without must
//! produce identical results, or the acceptance is measuring the developer's
//! desk.
//!
//! Self-skips without `/dev/kvm` or the guest artifacts, like every other test
//! in this crate.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use boot_tests::{boot_artifacts, boot_once_driven, kvm_available, BootSpec, VmHandle};
use control_api::VirtioTransport;
use virtio_input::{abs, btn, ev, InputEvent};

/// How long to wait for the guest to open its event node and say so.
const READY: Duration = Duration::from_secs(45);

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

fn spec() -> Option<BootSpec> {
    if !kvm_available() {
        return None;
    }
    let (kernel, initramfs) = boot_artifacts()?;
    // Seven real events plus four SYN_REPORTs is what the guest waits for.
    Some(
        BootSpec::new(kernel, initramfs)
            .with_gamepad_probe(11)
            // pci, not the harness's mmio default: the Debian netboot kernel
            // the tests run builds virtio-mmio as a module, so a
            // `virtio_mmio.device=` clause reaches nothing. The transport is
            // not what is under test here — the descriptor is — and the guest
            // finds the pad by name either way.
            .with_transport(VirtioTransport::Pci),
    )
}

/// Pushes the sequence once the guest says its event node is open.
fn drive(vm: VmHandle) {
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
    // The probe prints its own result line; the harness's `await_marker` ends
    // the boot on it, and this thread just waits for the deadline.
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
    let outcome = boot_once_driven(&spec, Some(Box::new(drive))).expect("the VM boots");
    assert!(
        outcome.reached_ready(),
        "guest never became ready:\n{}",
        outcome.serial
    );

    // ---- what the kernel made of the descriptor --------------------------
    let info = outcome
        .probe("padinfo")
        .unwrap_or_else(|| panic!("no padinfo line:\n{}", outcome.serial));
    let field = |key: &str| {
        info.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("padinfo has no {key}:\n{}", outcome.serial))
    };

    assert_eq!(field("name"), "Entangled_Gamepad");
    // The ids the guest reads back out of `VIRTIO_INPUT_CFG_ID_DEVIDS`.
    assert_eq!(field("bus"), "0006", "BUS_VIRTUAL");
    assert_eq!(field("vendor"), "564d");
    assert_eq!(field("product"), "0003");

    // The whole point of the layout: `joydev` bound it, so the pad is a
    // joystick to userspace and not merely an input device.
    assert_eq!(
        field("js"),
        "1",
        "joydev did not claim the pad, so there is no /dev/input/js*; handlers={}",
        field("handlers")
    );

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

    // ---- and the round trip ---------------------------------------------
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
    let expected = expected_echo();
    assert_eq!(
        value("seq"),
        expected.join(","),
        "the guest echoed a different sequence than the host injected:\n{}",
        outcome.serial
    );
    assert_eq!(value("events"), expected.len().to_string());
    // Four reports in, four SYN_REPORTs out: the grouping survived, so the
    // guest sees four discrete pad states rather than one smeared one.
    assert_eq!(value("syn"), injected().len().to_string());
}
