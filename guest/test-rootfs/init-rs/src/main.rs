//! PID 1 of the test initramfs: prints the boot marker the integration
//! tests wait for, then powers the machine off.

fn main() {
    // The marker must match linux_boot::GUEST_READY_MARKER.
    println!("VMHOST_GUEST_READY");

    // Restart, not power-off: the MVP machine has no ACPI, so power-off just
    // halts the vCPU forever. With `reboot=k` the kernel's restart chain ends
    // in a triple fault, which reaches the host as KVM_EXIT_SHUTDOWN and
    // terminates the VM cleanly.
    // SAFETY: plain syscalls; as PID 1 we hold CAP_SYS_BOOT.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_RESTART);
    }
    // Unreachable unless the kernel refuses; never return from PID 1 (that
    // would panic the kernel with a confusing message).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
