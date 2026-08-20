//! Windows console plumbing for a GUI-subsystem binary.
//!
//! `main.rs` marks the release build as `windows_subsystem = "windows"`, so the
//! manager no longer drags a black console window behind its own window when it
//! is started from Explorer, the Start menu or the installer's shortcut. The
//! cost is that a GUI-subsystem process starts with no standard handles at all,
//! which would silently swallow `--version`, `--help`, clap's usage errors and
//! every `tracing` line when someone *does* run it from a terminal.
//!
//! [`attach_parent`] buys that back: if the process was launched from a console
//! it borrows that console and points the standard handles at it, so terminal
//! use behaves exactly like the console-subsystem build did. If there is no
//! parent console (the Explorer case) it reports so, and the caller logs to a
//! file instead.

/// Whether this process can write to a terminal, and how that came to be.
///
/// The type is platform-neutral so `main.rs` needs no `cfg`, but only the
/// Windows implementation can produce `Borrowed` or `None` — every other
/// platform hands a process its standard handles no matter who launched it.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// The standard handles were already usable — a console-subsystem build, or
    /// a GUI build whose caller redirected output (`manager.exe > log.txt`).
    Inherited,
    /// Borrowed the parent's console; output goes to the terminal that started
    /// the manager.
    Borrowed,
    /// No terminal is reachable: launched from a GUI shell.
    None,
}

impl Attach {
    /// True when writing to stdout/stderr reaches a human.
    pub fn has_console(self) -> bool {
        !matches!(self, Attach::None)
    }
}

#[cfg(not(windows))]
pub fn attach_parent() -> Attach {
    // Every other platform's process starts with real standard handles.
    Attach::Inherited
}

#[cfg(windows)]
pub fn attach_parent() -> Attach {
    use windows::core::w;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    fn usable(handle: std::result::Result<HANDLE, windows::core::Error>) -> bool {
        matches!(handle, Ok(h) if !h.is_invalid() && h != INVALID_HANDLE_VALUE)
    }

    // SAFETY: GetStdHandle takes a documented constant and returns a handle we
    // only inspect; it borrows nothing and can be called at any time.
    let already = unsafe { usable(GetStdHandle(STD_OUTPUT_HANDLE)) };
    if already {
        return Attach::Inherited;
    }

    // SAFETY: AttachConsole with ATTACH_PARENT_PROCESS is the documented way to
    // adopt the caller's console. It fails harmlessly (ERROR_INVALID_HANDLE)
    // when the parent has none, which is the Explorer case.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
        return Attach::None;
    }

    // Attaching a console does not necessarily repoint handles that started out
    // null, so open the console device explicitly. CONOUT$/CONIN$ always name
    // the attached console, whatever the parent's own redirections were.
    let open = |name, access, share| {
        // SAFETY: `name` is a static wide literal, the flags are documented
        // constants, and both optional pointer arguments are None. The returned
        // handle is stored in this process's standard-handle slots and lives
        // until process exit, so it is never closed while in use.
        unsafe {
            CreateFileW(
                name,
                access,
                share,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
    };

    let out = open(
        w!("CONOUT$"),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    );
    let input = open(
        w!("CONIN$"),
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    );

    let Ok(out) = out else {
        // Attached but unusable: better to behave like the windowed case than
        // to write into a handle we could not open.
        return Attach::None;
    };

    // SAFETY: SetStdHandle stores a handle this process owns in a documented
    // slot; the handle outlives every write because it is never closed.
    unsafe {
        let _ = SetStdHandle(STD_OUTPUT_HANDLE, out);
        let _ = SetStdHandle(STD_ERROR_HANDLE, out);
        if let Ok(input) = input {
            let _ = SetStdHandle(STD_INPUT_HANDLE, input);
        }
    }

    Attach::Borrowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_windowed_case_lacks_a_console() {
        assert!(Attach::Inherited.has_console());
        assert!(Attach::Borrowed.has_console());
        assert!(!Attach::None.has_console());
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_always_have_their_standard_handles() {
        assert_eq!(attach_parent(), Attach::Inherited);
    }
}
