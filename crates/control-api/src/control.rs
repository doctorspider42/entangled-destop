//! The lifecycle control channel's vocabulary: the words a program writes to
//! `entangled run --control-stdin`, and the lines the VM writes back
//! ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md),
//! [ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! # Why this lives here rather than in the CLI
//!
//! There are two programs at the ends of that pipe — `entangled run`, which
//! reads the commands and prints the replies, and `entangled-manager`, which
//! writes the commands and reads the replies out of the child's log. Until this
//! module existed the manager could only have matched the prefix as a literal
//! string of its own, and a rename on the engine side would have turned every
//! reply into ordinary log noise: the Suspend button would spin until the child
//! exited and then report the wrong thing. The vocabulary is a wire format
//! between two crates, so it is written down once, in the crate both already
//! depend on.
//!
//! Replies are **not** a general-purpose protocol: the channel is one-way
//! commands plus a human-readable acknowledgement, and [`Reply`] exists so the
//! one machine-read answer — did the suspend produce a file, or an error? — can
//! be lifted out of a log line without guessing.

/// Every reply line the VM prints starts with this.
///
/// Chosen to be unmistakable in a log that also carries the guest's serial
/// console: a guest can of course print the same bytes, which is why a reply is
/// only ever *read* for something the reader itself asked for.
pub const PREFIX: &str = "entangled-control:";

/// Freeze the VM. Idempotent.
pub const CMD_PAUSE: &str = "pause";
/// Let a frozen VM continue. Idempotent.
pub const CMD_RESUME: &str = "resume";
/// Reboot the VM in place.
pub const CMD_RESET: &str = "reset";
/// Suspend to a snapshot file and exit; takes an optional path.
pub const CMD_SAVE: &str = "save";
/// Print the current run state.
pub const CMD_STATUS: &str = "status";
/// Type text and Enter on the guest's serial console.
pub const CMD_TYPE: &str = "type";
/// A named serial-console key, without an appended Enter.
pub const CMD_SERIAL_KEY: &str = "serial-key";

pub fn serial_key_bytes(key: &str) -> Option<&'static [u8]> {
    Some(match key {
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "left" => b"\x1b[D",
        "right" => b"\x1b[C",
        "escape" => b"\x1b",
        "tab" => b"\t",
        "enter" => b"\r",
        _ => return None,
    })
}
/// Press the guest's ACPI power button: ask it to shut down cleanly. The VM
/// ends when the guest powers off — or not, if its policy says otherwise; a
/// caller that must have it stopped follows up with its own deadline.
pub const CMD_SHUTDOWN: &str = "shutdown";
/// Ask the guest for another desktop resolution, `display WIDTHxHEIGHT`: what
/// resizing the window does, for a headless VM or a test. Requires a virtio
/// display adapter. `ok display` acknowledges the request; the guest driver
/// and desktop decide whether to apply the mode.
pub const CMD_DISPLAY: &str = "display";
/// Write a small PNG of the guest's screen, `thumbnail [path]`; without a path
/// it goes to [`thumbnail_path`] of the VM's snapshot location. Silent on
/// success — the manager asks every few seconds, and an `ok` per ask would
/// fill the run log.
pub const CMD_THUMBNAIL: &str = "thumbnail";
/// Type text on the guest's keyboard, `paste <text>`, US layout, `\n` for
/// Enter; answered with how many characters were typed, skipped (no key) and
/// truncated (past `virtio_input::typing::MAX_TYPED_CHARS`).
pub const CMD_PASTE: &str = "paste";
/// Press keys together on the guest's keyboard, `chord super r`: modifiers
/// and named keys by name, other keys as one character
/// (`virtio_input::typing::parse_chord`).
pub const CMD_CHORD: &str = "chord";
/// Write one full-resolution PNG of the guest's screen, `screenshot <path>`
/// (cursor included), atomically: answered `screenshot <path> <W>x<H>` once
/// the file is complete, or `error screenshot <detail>`.
pub const CMD_SCREENSHOT: &str = "screenshot";
/// Run a command line in a Windows guest through its agent,
/// `exec [--timeout SECS] <command line>` (`cmd.exe /d /s /c`, no window).
/// Off unless the profile says `guest_exec = true`. Answered at once with
/// `exec <id> sent`, then — whenever the guest answers — zero or more
/// `exec <id> stdout <line>` / `exec <id> stderr <line>` lines followed by
/// exactly one of `exec <id> exit <code> [stdout-truncated]
/// [stderr-truncated]`, `exec <id> timeout`, `exec <id> error <detail>`.
pub const CMD_EXEC: &str = "exec";
/// How long an `exec` may run when the line does not say.
pub const EXEC_DEFAULT_TIMEOUT_SECS: u32 = 60;
/// The longest `--timeout` an `exec` accepts.
pub const EXEC_MAX_TIMEOUT_SECS: u32 = 600;

/// Splits one control line into its command word and the rest, after
/// trimming — and after dropping a leading UTF-8 byte-order mark, which
/// .NET's `Process.StandardInput` writes before the first line and which
/// would otherwise turn that line's command into `"\u{feff}chord"`.
pub fn split_command(line: &str) -> (&str, &str) {
    let line = line.trim_start_matches('\u{feff}').trim();
    match line.split_once(char::is_whitespace) {
        Some((command, rest)) => (command, rest),
        None => (line, ""),
    }
}

/// The argument of `exec`: an optional `--timeout SECS`, then the command
/// line. `Err` carries what to tell the caller.
pub fn parse_exec(argument: &str) -> Result<(u32, &str), String> {
    let argument = argument.trim();
    let (timeout, command) = match argument.strip_prefix("--timeout") {
        Some(rest) if rest.starts_with(char::is_whitespace) => {
            let rest = rest.trim_start();
            let (secs, command) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let secs = secs
                .parse::<u32>()
                .ok()
                .filter(|s| (1..=EXEC_MAX_TIMEOUT_SECS).contains(s))
                .ok_or_else(|| {
                    format!("--timeout takes 1..={EXEC_MAX_TIMEOUT_SECS} seconds, not {secs:?}")
                })?;
            (secs, command.trim())
        }
        _ => (EXEC_DEFAULT_TIMEOUT_SECS, argument),
    };
    if command.is_empty() {
        return Err("needs a command line".into());
    }
    Ok((timeout, command))
}

/// Where a machine's card picture lives: `.thumbnails/<name>.png` beside its
/// profile. The engine derives it from the default snapshot path (which sits
/// beside the profile as `<name>.esnap`) and the manager from the profile, so
/// neither has to tell the other a path — which matters when the engine runs
/// in WSL and sees the directory under another name.
pub fn thumbnail_path(profile_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    profile_dir.join(".thumbnails").join(format!("{name}.png"))
}

/// Where a machine's window geometry is kept between runs:
/// `.window/<name>.toml` beside its profile (`display::placement`).
pub fn window_path(profile_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    profile_dir.join(".window").join(format!("{name}.toml"))
}

/// One reply line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `ok <command>` — the command was accepted (not that it has finished).
    Ok(String),
    /// `saved <path> <summary>` — a suspend completed and the file is on disk.
    /// Carried whole: the path may contain spaces, and the summary is meant for
    /// a person either way.
    Saved(String),
    /// `error <detail>` — the command failed. For `save` this is the one place
    /// the reason ever appears, because the VM then exits regardless.
    Error(String),
    /// `state=<RunState>` — the answer to `status`.
    State(String),
    /// A reply this build does not know. Never an error: a newer engine may
    /// say more than an older reader understands.
    Other(String),
}

/// Parses one line of a VM's output, or `None` when it is not a reply at all.
///
/// Tolerant by construction — the line arrives interleaved with the guest's
/// own console output and with `tracing` output that may have been through an
/// ANSI filter — so anything carrying the prefix is classified rather than
/// rejected.
pub fn parse_reply(line: &str) -> Option<Reply> {
    let rest = line.split_once(PREFIX)?.1.trim();
    if rest.is_empty() {
        return None;
    }
    let (head, tail) = match rest.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (rest, ""),
    };
    Some(match head {
        "ok" => Reply::Ok(tail.to_string()),
        "saved" => Reply::Saved(tail.to_string()),
        "error" => Reply::Error(tail.to_string()),
        _ if rest.starts_with("state=") => Reply::State(rest["state=".len()..].to_string()),
        _ => Reply::Other(rest.to_string()),
    })
}

/// The suspend outcome carried by a reply, if it carries one.
///
/// `Ok(detail)` is a written file, `Err(detail)` a suspend that failed — and
/// after either the VM is on its way out, so this is the last thing its
/// operator will hear from it.
pub fn save_outcome(reply: &Reply) -> Option<Result<String, String>> {
    match reply {
        Reply::Saved(detail) => Some(Ok(detail.clone())),
        // `save` is the only command whose failure is reported this way, and
        // the engine prefixes the detail with the command name.
        Reply::Error(detail) if detail.starts_with(CMD_SAVE) => {
            Some(Err(detail[CMD_SAVE.len()..].trim().to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_menu_keys_do_not_append_enter_or_accept_commands() {
        assert_eq!(serial_key_bytes("up"), Some(&b"\x1b[A"[..]));
        assert_eq!(serial_key_bytes("escape"), Some(&b"\x1b"[..]));
        assert_eq!(serial_key_bytes("enter"), Some(&b"\r"[..]));
        assert_eq!(serial_key_bytes("up\nreset"), None);
        assert_eq!(serial_key_bytes("unknown"), None);
    }

    /// .NET's `Process.StandardInput` starts the stream with a BOM.
    #[test]
    fn a_leading_byte_order_mark_is_not_part_of_the_command() {
        assert_eq!(split_command("\u{feff}chord alt f4"), ("chord", "alt f4"));
        assert_eq!(split_command("  status \r"), ("status", ""));
        assert_eq!(split_command("\u{feff}"), ("", ""));
        assert_eq!(split_command("paste a b"), ("paste", "a b"));
    }

    #[test]
    fn exec_takes_an_optional_timeout_and_a_command_line() {
        assert_eq!(parse_exec("dir Z:\\"), Ok((60, "dir Z:\\")));
        assert_eq!(parse_exec("--timeout 5 ver"), Ok((5, "ver")));
        assert_eq!(
            parse_exec("--timeout 5 echo a --timeout b"),
            Ok((5, "echo a --timeout b"))
        );
        assert!(parse_exec("").is_err());
        assert!(parse_exec("--timeout 5").is_err());
        assert!(parse_exec("--timeout 0 ver").is_err());
        assert!(parse_exec("--timeout 601 ver").is_err());
        assert!(parse_exec("--timeout x ver").is_err());
        // Not the flag: a command that happens to start with the word.
        assert_eq!(parse_exec("--timeoutx"), Ok((60, "--timeoutx")));
    }

    #[test]
    fn a_line_without_the_prefix_is_not_a_reply() {
        assert_eq!(parse_reply("[    0.412] guest booted"), None);
        assert_eq!(parse_reply(""), None);
        assert_eq!(parse_reply("entangled-control:"), None);
    }

    #[test]
    fn the_four_shapes_parse() {
        assert_eq!(
            parse_reply("entangled-control: ok pause"),
            Some(Reply::Ok("pause".into()))
        );
        assert_eq!(
            parse_reply("entangled-control: state=Running"),
            Some(Reply::State("Running".into()))
        );
        assert_eq!(
            parse_reply("entangled-control: unknown command \"fly\""),
            Some(Reply::Other("unknown command \"fly\"".into()))
        );
    }

    /// The path is kept whole: it can contain spaces, and every consumer shows
    /// it to a person rather than opening it.
    #[test]
    fn a_saved_reply_keeps_its_path_and_summary_together() {
        let reply = parse_reply(
            "entangled-control: saved /home/s/my vms/demo.esnap 514.0 MiB of guest RAM in \
             812 runs written in 2.76s",
        )
        .expect("reply");
        let detail = save_outcome(&reply).expect("outcome").expect("success");
        assert!(detail.starts_with("/home/s/my vms/demo.esnap"), "{detail}");
        assert!(detail.ends_with("2.76s"), "{detail}");
    }

    /// A failed suspend is the one error whose detail a reader has to act on:
    /// the VM exits either way, so this line is the whole explanation.
    #[test]
    fn a_failed_save_is_an_error_outcome_without_the_command_name() {
        let reply = parse_reply(
            "entangled-control: error save cannot write /vms/demo.esnap.part: No space left \
             on device",
        )
        .expect("reply");
        let detail = save_outcome(&reply).expect("outcome").unwrap_err();
        assert!(detail.starts_with("cannot write"), "{detail}");

        // Another command's failure is not a suspend outcome.
        let other = parse_reply("entangled-control: error pause the seam refused").expect("reply");
        assert_eq!(save_outcome(&other), None);
    }

    /// A reply that arrives on the same line as other output still parses: the
    /// log carries the guest's console, and a missing newline must not lose the
    /// one line a Suspend button is waiting for.
    #[test]
    fn a_reply_is_found_mid_line() {
        let reply = parse_reply("root@guest:~# entangled-control: saved /vms/a.esnap 1 MiB");
        assert_eq!(reply, Some(Reply::Saved("/vms/a.esnap 1 MiB".into())));
    }
}
