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
