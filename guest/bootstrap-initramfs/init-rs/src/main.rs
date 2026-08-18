//! Bootstrap initramfs /init (backlog MVP-1103..1106).
//!
//! The host boots the project's bootstrap kernel with this initramfs; our
//! job is to find the installed system's root filesystem, mount it and
//! `switch_root` into `/sbin/init`. Everything needed is built into the
//! kernel — this binary loads no modules.
//!
//! Supported `root=` forms on the kernel command line:
//!   root=/dev/vdaN         — explicit block device
//!   root=UUID=<uuid>       — ext4 filesystem UUID, resolved by scanning vd*
//! Optional: `rootfstype=` (default ext4), `init=` (default /sbin/init),
//! `rw` (default is read-only mount; Debian remounts rw itself).

use std::ffi::CString;
use std::path::Path;
use std::time::{Duration, Instant};

const ROOT_WAIT: Duration = Duration::from_secs(10);

fn main() {
    println!("vmhost-bootstrap: init starting");
    mount_pseudo();

    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let root = param(&cmdline, "root=").unwrap_or_else(|| {
        die("no root= on the kernel command line");
    });
    let fstype = param(&cmdline, "rootfstype=").unwrap_or_else(|| "ext4".into());
    let init = param(&cmdline, "init=").unwrap_or_else(|| "/sbin/init".into());
    let writable = cmdline.split_ascii_whitespace().any(|w| w == "rw");

    let device = resolve_root(&root).unwrap_or_else(|| {
        eprintln!("vmhost-bootstrap: cannot resolve root '{root}'");
        list_block_devices();
        die("root filesystem not found");
    });

    println!("vmhost-bootstrap: mounting {device} as {fstype} (rw={writable})");
    let flags = if writable { 0 } else { libc::MS_RDONLY };
    if mount(&device, "/newroot", &fstype, flags).is_err() {
        eprintln!("vmhost-bootstrap: mounting {device} failed: {}", errno());
        list_block_devices();
        die("cannot mount root filesystem");
    }

    if !Path::new("/newroot").join(init.trim_start_matches('/')).exists() {
        die(&format!("{init} does not exist on {device}"));
    }

    println!("vmhost-bootstrap: switching root to {device}, init {init}");
    switch_root("/newroot", &init);
}

/// Mounts /dev (devtmpfs), /proc and /sys inside the initramfs.
fn mount_pseudo() {
    for dir in ["/dev", "/proc", "/sys", "/newroot"] {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = mount("devtmpfs", "/dev", "devtmpfs", 0);
    let _ = mount("proc", "/proc", "proc", 0);
    let _ = mount("sysfs", "/sys", "sysfs", 0);
}

fn param(cmdline: &str, key: &str) -> Option<String> {
    cmdline
        .split_ascii_whitespace()
        .find_map(|w| w.strip_prefix(key))
        .map(str::to_string)
}

/// Resolves root= to a device path, waiting for it to appear (virtio probe
/// order is asynchronous).
fn resolve_root(root: &str) -> Option<String> {
    let deadline = Instant::now() + ROOT_WAIT;
    loop {
        let found = if let Some(uuid) = root.strip_prefix("UUID=") {
            find_by_ext4_uuid(uuid)
        } else if Path::new(root).exists() {
            Some(root.to_string())
        } else {
            None
        };
        if found.is_some() {
            return found;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Scans /dev/vd* partitions for an ext4 superblock with the given UUID
/// (MVP-1105). The ext4 superblock sits at byte 1024; s_uuid at offset 0x68.
fn find_by_ext4_uuid(want: &str) -> Option<String> {
    let want = want.to_ascii_lowercase();
    let entries = std::fs::read_dir("/dev").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("vd") {
            continue;
        }
        let path = format!("/dev/{name}");
        if let Some(uuid) = read_ext4_uuid(&path) {
            if uuid == want {
                return Some(path);
            }
        }
    }
    None
}

fn read_ext4_uuid(dev: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(dev).ok()?;
    let mut sb = [0u8; 1024];
    f.seek(SeekFrom::Start(1024)).ok()?;
    f.read_exact(&mut sb).ok()?;
    // Magic 0xef53 little-endian at superblock offset 0x38.
    if sb[0x38] != 0x53 || sb[0x39] != 0xef {
        return None;
    }
    let u = &sb[0x68..0x78];
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    ))
}

fn list_block_devices() {
    eprintln!("vmhost-bootstrap: available block devices:");
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("vd") {
                let uuid = read_ext4_uuid(&format!("/dev/{name}"))
                    .map(|u| format!(" (ext4 UUID {u})"))
                    .unwrap_or_default();
                eprintln!("  /dev/{name}{uuid}");
            }
        }
    }
}

fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong) -> Result<(), ()> {
    let src = CString::new(src).map_err(|_| ())?;
    let target = CString::new(target).map_err(|_| ())?;
    let fstype = CString::new(fstype).map_err(|_| ())?;
    // SAFETY: all pointers come from live CStrings; data is NULL (no options).
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(())
    }
}

/// Classic initramfs switch_root: move the mount over /, chroot, exec init.
fn switch_root(newroot: &str, init: &str) -> ! {
    let newroot_c = CString::new(newroot).unwrap_or_else(|_| die("bad newroot path"));
    let root_c = CString::new("/").unwrap_or_else(|_| die("unreachable"));
    let dot_c = CString::new(".").unwrap_or_else(|_| die("unreachable"));
    let init_c = CString::new(init).unwrap_or_else(|_| die("bad init path"));

    // SAFETY: straight syscall sequence with valid CString pointers; we are
    // PID 1 in the initramfs and the target directory is a mount point.
    unsafe {
        if libc::chdir(newroot_c.as_ptr()) != 0
            || libc::mount(
                newroot_c.as_ptr(),
                root_c.as_ptr(),
                std::ptr::null(),
                libc::MS_MOVE,
                std::ptr::null(),
            ) != 0
            || libc::chroot(dot_c.as_ptr()) != 0
            || libc::chdir(root_c.as_ptr()) != 0
        {
            die(&format!("switch_root sequence failed: {}", errno()));
        }
        let argv = [init_c.as_ptr(), std::ptr::null()];
        libc::execv(init_c.as_ptr(), argv.as_ptr());
    }
    die(&format!("execv {init} failed: {}", errno()));
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Prints the diagnostic and terminates the VM: restart ends in a triple
/// fault the host reports as a clean shutdown (this machine has no ACPI
/// power-off). MVP-1106: the error must be readable on the serial console.
fn die(message: &str) -> ! {
    eprintln!("vmhost-bootstrap: FATAL: {message}");
    // Drain the serial console before rebooting: the lean bootstrap kernel
    // triple-faults almost instantly, and interrupt-driven UART TX still in
    // flight would be lost (observed: diagnostics truncated at reboot).
    // SAFETY: plain syscalls; PID 1 holds CAP_SYS_BOOT.
    unsafe {
        libc::tcdrain(1);
        libc::tcdrain(2);
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_RESTART);
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
