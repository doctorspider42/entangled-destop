//! Turning CLI failure output into one actionable sentence for the UI.
//!
//! The manager cannot see inside the VMM, only the child's log — but the
//! handful of failures a user actually hits are all recognisable from it.

/// Scans the tail of a task log (oldest → newest) and returns an explanation
/// for the first known failure signature found, newest line first.
pub fn explain(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|line| explain_line(line))
}

fn explain_line(line: &str) -> Option<String> {
    // WSL's own failures come first: they are the only ones in this file that
    // are not the engine's output at all, and `execvpe entangled failed 2` is
    // the least readable line the product can produce. The translation lives in
    // `control_api::wsl` because `entangled doctor` says the same thing.
    if let Some(message) = control_api::wsl::translate(line) {
        return Some(message);
    }
    let lower = line.to_ascii_lowercase();
    let mentions_tap = lower.contains("tap");

    if mentions_tap && (lower.contains("busy") || lower.contains("in use")) {
        return Some(
            "the TAP interface is already held by another VM — only one VM at a time can \
             use it, so stop the other one first"
                .to_string(),
        );
    }
    if mentions_tap && (lower.contains("no such device") || lower.contains("not found")) {
        return Some(
            "the TAP interface does not exist on the host — create it with \
             scripts/setup-tap.sh"
                .to_string(),
        );
    }
    if mentions_tap
        && (lower.contains("permission denied") || lower.contains("operation not permitted"))
    {
        return Some(
            "no permission to open the TAP interface — /dev/net/tun needs access rights \
             (see scripts/setup-tap.sh)"
                .to_string(),
        );
    }
    if lower.contains("/dev/kvm") || lower.contains("cannot open kvm") {
        return Some(
            "/dev/kvm is not usable — run `entangled doctor` to check host virtualisation"
                .to_string(),
        );
    }
    if lower.contains("bootstrap kernel")
        || lower.contains("vmlinuz") && lower.contains("not found")
    {
        return Some(
            "the bootstrap kernel was not found relative to the working directory — set it \
             in Settings"
                .to_string(),
        );
    }
    if lower.contains("cannot attach disk")
        || lower.contains("no such file or directory") && lower.contains(".raw")
    {
        return Some("the disk image could not be opened".to_string());
    }
    if lower.contains("does not look installed") {
        return Some(
            "the installer exited before the system was installed — check the log for the \
             last d-i step"
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recognises_a_busy_tap() {
        let log = lines(&[
            "INFO vm{id=debian-demo}: attaching virtio-blk device",
            "error: cannot open TAP 'entangled0': Device or resource busy (os error 16)",
        ]);
        let hint = explain(&log).expect("hint");
        assert!(hint.contains("only one VM at a time"), "{hint}");
    }

    #[test]
    fn recognises_a_missing_tap_and_permissions() {
        let missing = lines(&["error: cannot open TAP 'entangled9': No such device"]);
        assert!(explain(&missing).expect("hint").contains("setup-tap.sh"));

        let denied = lines(&["error: cannot open TAP 'entangled0': Permission denied"]);
        assert!(explain(&denied).expect("hint").contains("permission"));
    }

    #[test]
    fn recognises_kvm_and_installer_failures() {
        assert!(
            explain(&lines(&["error: cannot open /dev/kvm: No such file"]))
                .expect("hint")
                .contains("doctor")
        );
        assert!(explain(&lines(&[
            "error: the installer exited but debian.raw does not look installed: no ext4"
        ]))
        .expect("hint")
        .contains("installer exited"));
    }

    /// The failure this whole feature exists for, arriving the one way a
    /// pre-flight cannot stop: mid-run. It must reach the user as a sentence.
    #[test]
    fn recognises_wsls_own_noise_about_a_missing_engine() {
        let log = lines(&[
            "<3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2",
            "<3>WSL (13) ERROR: CreateProcessEntryCommon:508: Create process not expected to \
             return",
        ]);
        let hint = explain(&log).expect("hint");
        assert!(hint.contains("no such program"), "{hint}");
        assert!(!hint.contains("execvpe"), "{hint}");
    }

    #[test]
    fn stays_silent_on_unknown_output() {
        assert!(explain(&lines(&["INFO vm: VM running", "guest login:"])).is_none());
        assert!(explain(&[]).is_none());
    }

    #[test]
    fn prefers_the_newest_recognisable_line() {
        let log = lines(&[
            "error: cannot open /dev/kvm: nope",
            "error: cannot open TAP 'entangled0': Device or resource busy",
        ]);
        assert!(explain(&log).expect("hint").contains("TAP interface"));
    }
}
