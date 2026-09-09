//! Is there a Linux engine inside WSL, and if not, which of the three things
//! went wrong.
//!
//! On a Windows host the manager can run a machine two ways: `entangled.exe`
//! natively on WHP, or the **Linux** build of `entangled` inside WSL, where
//! there is a real `/dev/kvm`. The second one has a hole in it that no amount
//! of path handling can close: the Windows installer ships no Linux binary, so
//! `wsl.exe -d Ubuntu -e entangled …` on a fresh install runs a program that is
//! not there. WSL reports that as
//!
//! ```text
//! <3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2
//! ```
//!
//! which is `ENOENT` wearing a hat, and which used to reach the user *after*
//! they had filled in a whole wizard. This module is the answer: ask the three
//! questions first — does WSL run, does the distribution exist, does the engine
//! inside it start — and turn every "no" into one sentence with a fix in it.
//!
//! It lives in `control-api` rather than in the manager because `entangled
//! doctor` owes the same answer (it reports which engine *each* backend would
//! use), and two copies of this classification would drift within a release.
//!
//! # Everything interesting is pure
//!
//! [`probe_with`] takes the process runner as an argument, so the whole
//! three-step flow — including every failure classification — is unit-tested on
//! both hosts with no `wsl.exe` anywhere. [`probe`] is the thin `std::process`
//! binding for callers that want the default one; the manager passes its own so
//! that no console window flashes over the GUI.
//!
//! # A detail that decides the whole design
//!
//! `wsl.exe -e <program>` does **not** run a login shell, so `$PATH` is the one
//! WSL's init built (`/etc/environment` plus the interop additions) — not the
//! one `~/.profile` would have made. `~/.local/bin` is on the second and
//! usually not on the first. An engine installed there is therefore perfectly
//! present and still not runnable as `-e entangled`, which is why
//! [`install_script`] reports the **absolute** path it wrote and the caller
//! stores that, and why [`PATH_PROBE`] asks `sh -c` rather than `sh -lc`: the
//! question is what the launch will see, not what a terminal would.

use std::collections::BTreeMap;

/// The distribution the manager talks to unless told otherwise — `wsl -d
/// <name>`, the name shown by `wsl --list`.
pub const DEFAULT_DISTRO: &str = "Ubuntu";

/// What an unset engine path means: whatever `entangled` resolves to on the
/// distribution's `PATH`.
pub const PATH_LOOKUP: &str = "entangled";

/// Where the manager installs a downloaded Linux engine, relative to `$HOME`.
/// The conventional per-user location, and one that needs no `sudo`.
pub const HOME_BIN: &str = ".local/bin";

/// `$HOME`-relative path of an engine this project installed.
pub const HOME_ENGINE: &str = ".local/bin/entangled";

/// The program every command here belongs to.
pub const WSL_PROGRAM: &str = "wsl.exe";

/// Which of the three pre-flight questions answered "no".
///
/// The distinction is not decoration: each one has a different fix, and the
/// manager only offers to *install* an engine for [`Fault::NoEngine`] — there
/// is nowhere to install it when the distribution itself is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `wsl.exe` could not be started, or WSL has no distributions at all.
    NoWsl,
    /// WSL runs, but not under the configured name.
    NoDistro,
    /// The distribution runs; the engine is not there (or not on the `PATH`
    /// that `wsl -e` uses — see the module note).
    NoEngine,
    /// The engine is there and would not run: not executable, wrong
    /// architecture, missing loader.
    EngineFailed,
}

impl Fault {
    /// Whether "download and install the Linux engine" is a sensible offer for
    /// this fault.
    pub const fn installable(self) -> bool {
        matches!(self, Fault::NoEngine)
    }
}

/// A refused pre-flight: what is wrong, and what to do about it.
///
/// Two fields rather than one string because the UI shows them in two places —
/// `what` inline under the control, `fix` where there is room — and because a
/// message that only says what is wrong is the failure this module exists to
/// replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFault {
    pub fault: Fault,
    /// One clause, no trailing full stop: "WSL has no distribution called
    /// 'Ubuntu'".
    pub what: String,
    /// One sentence the user can act on.
    pub fix: String,
}

impl EngineFault {
    fn new(fault: Fault, what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            fault,
            what: what.into(),
            fix: fix.into(),
        }
    }

    /// Both halves, as the one line a toast or a CLI prints.
    pub fn sentence(&self) -> String {
        format!("{} — {}", self.what, self.fix)
    }
}

impl std::fmt::Display for EngineFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.sentence())
    }
}

/// A working engine inside a distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFound {
    pub distro: String,
    /// What was executed — the configured path, or the bare name that was
    /// looked up on the `PATH`.
    pub command: String,
    /// Where that resolved to inside the distribution, when it could be
    /// resolved. `None` only means the resolution step was unavailable, never
    /// that the engine is missing.
    pub path: Option<String>,
    /// `entangled 0.2.137` → `0.2.137`.
    pub version: Option<String>,
}

impl EngineFound {
    /// The one-line status: `Ubuntu: /home/spider/.local/bin/entangled
    /// (0.2.137)`.
    pub fn summary(&self) -> String {
        let where_ = self.path.as_deref().unwrap_or(&self.command);
        match &self.version {
            Some(version) => format!("{}: {where_} ({version})", self.distro),
            None => format!("{}: {where_}", self.distro),
        }
    }
}

// ---------------------------------------------------------------------------
// Running things
// ---------------------------------------------------------------------------

/// What one `wsl.exe` invocation produced.
#[derive(Debug, Clone, Default)]
pub struct Ran {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Both streams, decoded — `wsl.exe`'s own errors go to stderr in UTF-16
    /// while the guest program's go to stdout in UTF-8, and a classification
    /// that read only one of them would miss half the failures.
    pub fn text(&self) -> String {
        let mut text = decode(&self.stdout);
        let err = decode(&self.stderr);
        if !err.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&err);
        }
        text
    }
}

/// How [`probe_with`] runs a `wsl.exe` command line. An `Err` means the process
/// could not be started at all, which is a different fault from one that ran
/// and failed.
pub type Runner<'a> = &'a dyn Fn(&[String]) -> std::io::Result<Ran>;

/// `std::process` with nothing added. The manager passes its own runner instead
/// (same closure shape) so the spawn carries `CREATE_NO_WINDOW`.
pub fn run_wsl(args: &[String]) -> std::io::Result<Ran> {
    let out = std::process::Command::new(WSL_PROGRAM)
        .args(args)
        .output()?;
    Ok(Ran {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

// ---------------------------------------------------------------------------
// Decoding what wsl.exe says
// ---------------------------------------------------------------------------

/// `wsl.exe` writes its own messages as UTF-16LE (`wsl --list` most visibly),
/// while a Linux program's output arrives as bytes. Guessing is unavoidable, so
/// guess on the evidence: a BOM, or an even length with NULs where UTF-16LE
/// ASCII puts them.
pub fn decode(bytes: &[u8]) -> String {
    if looks_utf16le(bytes) {
        let start = usize::from(bytes.starts_with(&[0xFF, 0xFE])) * 2;
        let units: Vec<u16> = bytes[start..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn looks_utf16le(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return true;
    }
    if bytes.len() < 4 || bytes.len() % 2 != 0 {
        return false;
    }
    // Every second byte NUL over a sample is what ASCII-in-UTF-16LE looks like
    // and what nothing else does.
    let sample = bytes.len().min(64);
    let odd_nuls = bytes[..sample]
        .chunks_exact(2)
        .filter(|pair| pair[1] == 0)
        .count();
    odd_nuls * 2 >= sample / 2 && odd_nuls > 0
}

/// The distribution names in a `wsl --list --quiet` answer.
///
/// `--quiet` prints one name per line and no header, but the encoding is
/// UTF-16LE and older builds still decorate the default with `(Default)`, so
/// both are handled here rather than at three call sites.
pub fn parse_distros(bytes: &[u8]) -> Vec<String> {
    decode(bytes)
        .lines()
        .map(|line| {
            line.trim_matches(|c: char| c.is_whitespace() || c == '\u{0}')
                .split(" (")
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Distribution names are matched case-insensitively: `wsl -d ubuntu` starts
/// `Ubuntu`, so a settings file that spells it in lower case must not be told
/// its distribution does not exist.
pub fn has_distro(list: &[String], wanted: &str) -> bool {
    list.iter().any(|name| name.eq_ignore_ascii_case(wanted))
}

/// `entangled 0.2.17` → `0.2.17`; anything else → `None`, because half a
/// version in a status line is worse than none.
pub fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| !line.trim().is_empty())?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
        .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// Translating the raw noise
// ---------------------------------------------------------------------------

/// Turns a line of raw WSL failure output into a sentence, or `None` when it is
/// not one of the shapes we know.
///
/// This is the second half of the bug this module fixes: a pre-flight stops the
/// common case, but a run can still die this way — a distribution shut down
/// between the check and the launch, an engine deleted, a `wsl --shutdown` from
/// another window. When it does, `execvpe entangled failed 2` must not be what
/// the user reads.
pub fn translate(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("execvpe") && lower.contains("failed") {
        let program = line
            .split_whitespace()
            .skip_while(|word| !word.eq_ignore_ascii_case("execvpe"))
            .nth(1)
            .unwrap_or(PATH_LOOKUP);
        // "failed 2" is ENOENT, "failed 13" is EACCES. Everything else is rare
        // enough that the generic sentence is the honest one.
        let detail = match line.split_whitespace().last().and_then(|n| n.parse().ok()) {
            Some(2) => "there is no such program in that distribution",
            Some(13) => "that file is in the distribution but is not executable",
            Some(8) => "that file is not a Linux program for this processor",
            _ => "WSL could not start it",
        };
        return Some(format!(
            "WSL could not run '{program}' inside the distribution: {detail}. The Windows \
             installer ships no Linux build of the engine — install it from Settings, or \
             point Settings ▸ Linux engine at your own build."
        ));
    }
    if lower.contains("createprocessentrycommon") {
        // The line after the execvpe one. On its own it says nothing a user can
        // act on, and it must not be mistaken for a second, different fault.
        return None;
    }
    if lower.contains("no distribution with the supplied name") {
        return Some(
            "WSL has no distribution with that name. Check Settings ▸ WSL distro against \
             `wsl --list`, or install one with `wsl --install -d Ubuntu`."
                .to_string(),
        );
    }
    if lower.contains("has no installed distributions")
        || (lower.contains("wsl") && lower.contains("is not recognized"))
    {
        return Some(
            "Windows has no usable WSL installation. Run `wsl --install` in a terminal (it \
             needs a restart), or run this machine on the Windows engine instead."
                .to_string(),
        );
    }
    None
}

/// The same, over the tail of a log (oldest → newest); newest match wins.
pub fn translate_log(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|line| translate(line))
}

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

/// The shell fragment that answers "would `wsl -e <engine>` find it, and where
/// is it really".
///
/// Run through `sh -c`, deliberately **not** `sh -lc`: a login shell sources
/// `~/.profile`, which is exactly the `PATH` the real launch does *not* get.
pub const PATH_PROBE: &str = concat!(
    r#"p="$(command -v "$0" 2>/dev/null || true)"; "#,
    r#"[ -n "$p" ] && printf 'PATH=%s\n' "$p"; "#,
    r#"[ -x "$HOME/.local/bin/entangled" ] && printf 'HOME_ENGINE=%s\n' "$HOME/.local/bin/entangled"; "#,
    "exit 0"
);

/// Asks the three questions, in the order in which their answers stop being
/// interesting: is there a WSL, is there this distribution, does the engine in
/// it run.
///
/// `engine` is the configured Linux path; `None` means "whatever `entangled`
/// resolves to", which is what an empty setting means everywhere else.
pub fn probe_with(
    distro: &str,
    engine: Option<&str>,
    run: Runner<'_>,
) -> Result<EngineFound, EngineFault> {
    let distro = distro.trim();
    let distro = if distro.is_empty() {
        DEFAULT_DISTRO
    } else {
        distro
    };
    let command = engine
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or(PATH_LOOKUP);

    // 1. Is there a WSL, and does it have this distribution?
    let listed = run(&["--list".into(), "--quiet".into()]).map_err(|e| {
        EngineFault::new(
            Fault::NoWsl,
            format!("Windows could not start wsl.exe ({e})"),
            "Install the Windows Subsystem for Linux with `wsl --install` (it needs a \
             restart), or run this machine on the Windows engine instead.",
        )
    })?;
    if !listed.ok() {
        let text = listed.text();
        return Err(EngineFault::new(
            Fault::NoWsl,
            "WSL is installed but has no usable distribution".to_string(),
            translate(&text).unwrap_or_else(|| {
                "Install one with `wsl --install -d Ubuntu`, then check it starts with \
                 `wsl -d Ubuntu -e true`."
                    .to_string()
            }),
        ));
    }
    let distros = parse_distros(&listed.stdout);
    if !distros.is_empty() && !has_distro(&distros, distro) {
        return Err(EngineFault::new(
            Fault::NoDistro,
            format!(
                "WSL has no distribution called '{distro}' (it has: {})",
                distros.join(", ")
            ),
            format!(
                "Choose one of those under Settings ▸ WSL distro, or install it with \
                 `wsl --install -d {distro}`."
            ),
        ));
    }

    // 2. Does the engine run, exactly the way a launch would run it?
    let attempt = run(&[
        "-d".into(),
        distro.to_string(),
        "-e".into(),
        command.to_string(),
        "--version".into(),
    ])
    .map_err(|e| {
        EngineFault::new(
            Fault::NoWsl,
            format!("Windows could not start wsl.exe ({e})"),
            "Install the Windows Subsystem for Linux with `wsl --install`, or run this \
             machine on the Windows engine instead.",
        )
    })?;

    if attempt.ok() {
        let text = attempt.text();
        return Ok(EngineFound {
            distro: distro.to_string(),
            command: command.to_string(),
            path: resolve_path(distro, command, run),
            version: parse_version(&text),
        });
    }

    // 3. It did not. Find out *why* before writing a sentence about it: a file
    //    that exists at the conventional place but is not on the launch PATH is
    //    a completely different message from one that is not there at all.
    Err(classify(
        distro,
        command,
        &attempt,
        resolve(distro, command, run),
    ))
}

/// [`probe_with`] against the plain `std::process` runner.
pub fn probe(distro: &str, engine: Option<&str>) -> Result<EngineFound, EngineFault> {
    probe_with(distro, engine, &run_wsl)
}

/// What the distribution says about where the engine is. Best effort by design:
/// a failure here loses a path in a status line, never a verdict.
fn resolve(distro: &str, command: &str, run: Runner<'_>) -> BTreeMap<String, String> {
    let args = vec![
        "-d".to_string(),
        distro.to_string(),
        "-e".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        PATH_PROBE.to_string(),
        command.to_string(),
    ];
    match run(&args) {
        Ok(out) => parse_kv(&out.text()),
        Err(_) => BTreeMap::new(),
    }
}

fn resolve_path(distro: &str, command: &str, run: Runner<'_>) -> Option<String> {
    if command.starts_with('/') {
        return Some(command.to_string());
    }
    resolve(distro, command, run).remove("PATH")
}

/// `KEY=value` lines into a map. Anything else in the output — a shell warning,
/// a motd fragment — is ignored rather than parsed.
pub fn parse_kv(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.trim().split_once('='))
        .filter(|(key, _)| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect()
}

/// Turns a failed `--version` attempt into the fault it actually is.
fn classify(
    distro: &str,
    command: &str,
    attempt: &Ran,
    facts: BTreeMap<String, String>,
) -> EngineFault {
    let text = attempt.text();
    let lower = text.to_ascii_lowercase();

    if lower.contains("no distribution with the supplied name") {
        return EngineFault::new(
            Fault::NoDistro,
            format!("WSL has no distribution called '{distro}'"),
            format!(
                "Check Settings ▸ WSL distro against `wsl --list`, or install it with \
                 `wsl --install -d {distro}`."
            ),
        );
    }
    if lower.contains("exec format error") || lower.contains("failed 8") {
        return EngineFault::new(
            Fault::EngineFailed,
            format!("'{command}' in {distro} is not a Linux program for this processor"),
            "Install the published Linux engine instead of the file that is there, or point \
             Settings ▸ Linux engine at an x86-64 Linux build."
                .to_string(),
        );
    }
    if lower.contains("permission denied") || lower.contains("failed 13") {
        return EngineFault::new(
            Fault::EngineFailed,
            format!("'{command}' in {distro} is not executable"),
            format!("Run `wsl -d {distro} -e chmod +x {command}`, or install the engine again."),
        );
    }

    // A dynamic-loader complaint. It says "not found" and means the exact
    // opposite of the missing-engine case below — the file is there and the
    // distribution is older than the machine that built it — so it has to be
    // caught first, or the manager would offer to install the very binary that
    // just refused to load.
    if lower.contains("glibc") || lower.contains("cannot open shared object file") {
        return EngineFault::new(
            Fault::EngineFailed,
            format!(
                "the engine in {distro} needs newer system libraries than {distro} has ({})",
                first_line(&text)
            ),
            "Upgrade the distribution, or build the engine inside it and point Settings ▸ \
             Linux engine at your build."
                .to_string(),
        );
    }

    // The headline case, and the one with the best fix: the file is there, at
    // the place this manager installs to, and the launch cannot see it because
    // `wsl -e` does not read ~/.profile.
    if let Some(found) = facts.get("HOME_ENGINE") {
        if command == PATH_LOOKUP {
            return EngineFault::new(
                Fault::NoEngine,
                format!(
                    "{distro} has an engine at {found}, but it is not on the PATH that \
                         `wsl -e` uses"
                ),
                format!(
                    "Put {found} in Settings ▸ Linux engine — a launch runs no login shell, \
                     so ~/.profile's PATH does not apply to it."
                ),
            );
        }
    }

    let missing = lower.contains("failed 2")
        || lower.contains("not found")
        || lower.contains("no such file")
        || attempt.code == Some(127);
    if missing || text.trim().is_empty() {
        return EngineFault::new(
            Fault::NoEngine,
            format!("{distro} has no Entangled engine ('{command}' is not there)"),
            // Venue-neutral on purpose: this sentence is printed by `doctor` in
            // a terminal and shown by the manager under a button, and "install
            // it here" was a lie in one of those two places.
            "The Windows installer ships no Linux build: let the manager install one \
             (Settings ▸ Linux engine), or point that setting at your own build."
                .to_string(),
        );
    }

    EngineFault::new(
        Fault::EngineFailed,
        format!(
            "'{command}' in {distro} would not run ({})",
            first_line(&text)
        ),
        "Try it in a terminal with `wsl -d {distro} -e {command} --version`, or install the \
         published Linux engine from Settings."
            .replace("{distro}", distro)
            .replace("{command}", command),
    )
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .chars()
        .take(160)
        .collect()
}

// ---------------------------------------------------------------------------
// Installing an engine into a distribution
// ---------------------------------------------------------------------------

/// A finished install, as read back out of the distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Absolute Linux path of the engine — the value the caller stores as the
    /// Linux engine setting, because it is the one a launch can always reach.
    pub path: String,
    pub version: Option<String>,
    /// SHA-256 of the file as it now sits inside the distribution, when the
    /// distribution had `sha256sum` to compute it with.
    pub sha256: Option<String>,
    /// Whether a bare `entangled` also resolves on the launch `PATH`. Not a
    /// requirement — the absolute path is what gets used — but worth saying.
    pub on_path: bool,
}

/// The shell program that copies a verified engine into `~/.local/bin` and
/// reports what it did.
///
/// `source` is the file as the *distribution* sees it (a `/mnt/...` path for a
/// download sitting in the Windows cache). Copy to a temporary name and rename
/// over the target so an engine that is currently running is replaced rather
/// than written into ("text file busy"), and so a half-copied file is never
/// left behind under the real name.
pub fn install_script(source: &str) -> String {
    format!(
        r#"set -e
dir="$HOME/{HOME_BIN}"
mkdir -p "$dir"
cp {src} "$dir/entangled.part"
chmod 755 "$dir/entangled.part"
mv -f "$dir/entangled.part" "$dir/entangled"
printf 'INSTALLED=%s\n' "$dir/entangled"
printf 'VERSION=%s\n' "$("$dir/entangled" --version 2>&1 | head -n 1)"
if command -v sha256sum >/dev/null 2>&1; then
  printf 'SHA256=%s\n' "$(sha256sum "$dir/entangled" | cut -d' ' -f1)"
fi
if command -v entangled >/dev/null 2>&1; then printf 'ONPATH=yes\n'; else printf 'ONPATH=no\n'; fi
"#,
        src = sh_quote(source)
    )
}

/// The `wsl.exe` argument vector that runs [`install_script`].
pub fn install_args(distro: &str, source: &str) -> Vec<String> {
    vec![
        "-d".to_string(),
        distro.to_string(),
        "-e".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        install_script(source),
    ]
}

/// Reads the script's `KEY=value` report back.
pub fn parse_installed(text: &str) -> Option<Installed> {
    let facts = parse_kv(text);
    let path = facts.get("INSTALLED")?.clone();
    if path.is_empty() {
        return None;
    }
    Some(Installed {
        path,
        version: facts.get("VERSION").and_then(|line| parse_version(line)),
        sha256: facts
            .get("SHA256")
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        on_path: facts.get("ONPATH").is_some_and(|v| v == "yes"),
    })
}

/// A string as one POSIX single-quoted shell word. The only quoting rule that
/// has no exceptions: inside `'…'` nothing is special, and an embedded quote is
/// spelled `'\''`.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scripted `wsl.exe`: each call pops the next answer and records the
    /// arguments it was given.
    struct Fake {
        answers: RefCell<Vec<std::io::Result<Ran>>>,
        seen: RefCell<Vec<Vec<String>>>,
    }

    impl Fake {
        fn new(answers: Vec<std::io::Result<Ran>>) -> Self {
            Self {
                answers: RefCell::new(answers.into_iter().rev().collect()),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn run(&self, args: &[String]) -> std::io::Result<Ran> {
            self.seen.borrow_mut().push(args.to_vec());
            self.answers
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Ok(Ran::default()))
        }
    }

    fn ok(stdout: &str) -> std::io::Result<Ran> {
        Ok(Ran {
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    fn failed(code: i32, stderr: &str) -> std::io::Result<Ran> {
        Ok(Ran {
            code: Some(code),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    fn utf16(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    /// `wsl --list --quiet` answers in UTF-16LE, which is why a naive
    /// `from_utf8_lossy` reader saw one distribution called "U\0b\0u\0…".
    #[test]
    fn the_distribution_list_is_utf16() {
        let bytes = utf16("Ubuntu\r\ndocker-desktop\r\n");
        let list = parse_distros(&bytes);
        assert_eq!(list, vec!["Ubuntu", "docker-desktop"]);
        assert!(has_distro(&list, "ubuntu"), "matching is case-insensitive");
        assert!(!has_distro(&list, "Debian"));

        // A UTF-8 answer (an older build, or a test fixture) still parses, and
        // the `(Default)` decoration is not part of a name.
        assert_eq!(
            parse_distros(b"Ubuntu (Default)\nDebian\n"),
            vec!["Ubuntu", "Debian"]
        );
        assert!(parse_distros(b"").is_empty());
    }

    /// The exact message from the bug report must become a sentence about the
    /// engine, and the follow-on line must not become a second fault.
    #[test]
    fn the_execvpe_noise_is_translated() {
        let message = translate(
            "<3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2",
        )
        .expect("translated");
        assert!(message.contains("no such program"), "{message}");
        assert!(message.contains("Settings"), "{message}");
        assert!(!message.contains("execvpe"), "{message}");

        assert_eq!(
            translate(
                "<3>WSL (13) ERROR: CreateProcessEntryCommon:508: Create process not expected \
                 to return"
            ),
            None,
            "the follow-on line has no fix of its own and must stay quiet"
        );

        // The other two errnos WSL reports this way.
        assert!(translate("execvpe /home/x/entangled failed 13")
            .expect("translated")
            .contains("not executable"));
        assert!(translate("execvpe entangled failed 8")
            .expect("translated")
            .contains("not a Linux program"));

        assert_eq!(translate("INFO vm: VM running"), None);
    }

    #[test]
    fn the_newest_recognisable_line_of_a_log_wins() {
        let log = [
            "INFO starting".to_string(),
            "<3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2"
                .to_string(),
            "<3>WSL (13) ERROR: CreateProcessEntryCommon:508: Create process not expected to \
             return"
                .to_string(),
        ];
        assert!(translate_log(&log)
            .expect("hint")
            .contains("no such program"));
        assert_eq!(translate_log(&[]), None);
    }

    #[test]
    fn a_working_engine_is_found_with_its_path_and_version() {
        let fake = Fake::new(vec![
            Ok(Ran {
                code: Some(0),
                stdout: utf16("Ubuntu\r\n"),
                stderr: Vec::new(),
            }),
            ok("entangled 0.2.137\n"),
            ok("PATH=/home/spider/.local/bin/entangled\n"),
        ]);
        let found = probe_with("Ubuntu", None, &|args| fake.run(args)).expect("found");
        assert_eq!(found.version.as_deref(), Some("0.2.137"));
        assert_eq!(
            found.path.as_deref(),
            Some("/home/spider/.local/bin/entangled")
        );
        assert!(
            found.summary().contains("Ubuntu: /home/spider"),
            "{}",
            found.summary()
        );

        // The version probe must be spelled exactly like a real launch, or it
        // proves nothing about one.
        let seen = fake.seen.borrow();
        assert_eq!(
            seen[1],
            vec!["-d", "Ubuntu", "-e", "entangled", "--version"]
        );
    }

    /// An explicit path needs no resolution step — and must not spend a process
    /// spawn on one.
    #[test]
    fn an_explicit_engine_path_is_reported_as_given() {
        let fake = Fake::new(vec![
            Ok(Ran {
                code: Some(0),
                stdout: utf16("Ubuntu\r\n"),
                stderr: Vec::new(),
            }),
            ok("entangled 0.2.137\n"),
        ]);
        let found =
            probe_with("Ubuntu", Some("/opt/entangled"), &|args| fake.run(args)).expect("found");
        assert_eq!(found.path.as_deref(), Some("/opt/entangled"));
        assert_eq!(
            fake.seen.borrow().len(),
            2,
            "no third spawn for `command -v`"
        );
    }

    #[test]
    fn a_missing_distribution_names_the_ones_that_exist() {
        let fake = Fake::new(vec![Ok(Ran {
            code: Some(0),
            stdout: utf16("Ubuntu\r\ndocker-desktop\r\n"),
            stderr: Vec::new(),
        })]);
        let fault = probe_with("Fedora", None, &|args| fake.run(args)).expect_err("no distro");
        assert_eq!(fault.fault, Fault::NoDistro);
        assert!(
            fault.what.contains("Ubuntu, docker-desktop"),
            "{}",
            fault.what
        );
        assert!(
            fault.fix.contains("wsl --install -d Fedora"),
            "{}",
            fault.fix
        );
        assert!(!fault.fault.installable(), "there is nowhere to install it");
    }

    #[test]
    fn no_wsl_at_all_is_its_own_fault() {
        let fake = Fake::new(vec![Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "program not found",
        ))]);
        let fault = probe_with("Ubuntu", None, &|args| fake.run(args)).expect_err("no wsl");
        assert_eq!(fault.fault, Fault::NoWsl);
        assert!(fault.fix.contains("wsl --install"), "{}", fault.fix);
    }

    /// The bug, end to end: a distribution that runs and has no engine. The
    /// fault must be the installable one, and the sentence must not contain the
    /// word execvpe.
    #[test]
    fn a_distribution_without_an_engine_offers_the_install() {
        let fake = Fake::new(vec![
            Ok(Ran {
                code: Some(0),
                stdout: utf16("Ubuntu\r\n"),
                stderr: Vec::new(),
            }),
            failed(
                1,
                "<3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2\n",
            ),
            ok(""),
        ]);
        let fault = probe_with("Ubuntu", None, &|args| fake.run(args)).expect_err("no engine");
        assert_eq!(fault.fault, Fault::NoEngine);
        assert!(fault.fault.installable());
        assert!(fault.what.contains("no Entangled engine"), "{}", fault.what);
        assert!(
            !fault.sentence().contains("execvpe"),
            "{}",
            fault.sentence()
        );
    }

    /// The subtle one: the engine is installed where the manager puts it, and
    /// `wsl -e` still cannot see it because it runs no login shell. The fix is
    /// the absolute path, and the message has to say so.
    #[test]
    fn an_engine_off_the_launch_path_is_diagnosed_precisely() {
        let fake = Fake::new(vec![
            Ok(Ran {
                code: Some(0),
                stdout: utf16("Ubuntu\r\n"),
                stderr: Vec::new(),
            }),
            failed(1, "execvpe entangled failed 2\n"),
            ok("HOME_ENGINE=/home/spider/.local/bin/entangled\n"),
        ]);
        let fault = probe_with("Ubuntu", None, &|args| fake.run(args)).expect_err("off PATH");
        assert_eq!(fault.fault, Fault::NoEngine);
        assert!(fault.what.contains("not on the PATH"), "{}", fault.what);
        assert!(
            fault.fix.contains("/home/spider/.local/bin/entangled"),
            "{}",
            fault.fix
        );
    }

    #[test]
    fn a_present_but_unrunnable_engine_is_not_a_missing_one() {
        for (stderr, needle) in [
            ("sh: 1: /opt/e: Permission denied", "not executable"),
            ("/opt/e: 1: Exec format error", "not a Linux program"),
            // The loader's complaint contains the words "not found" and means
            // the file is *there*. Classifying it as missing would have the
            // manager offer to install the binary that just refused to load.
            (
                "/opt/e: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found",
                "newer system libraries",
            ),
        ] {
            let fake = Fake::new(vec![
                Ok(Ran {
                    code: Some(0),
                    stdout: utf16("Ubuntu\r\n"),
                    stderr: Vec::new(),
                }),
                failed(126, stderr),
                ok(""),
            ]);
            let fault =
                probe_with("Ubuntu", Some("/opt/e"), &|args| fake.run(args)).expect_err("broken");
            assert_eq!(fault.fault, Fault::EngineFailed);
            assert!(fault.what.contains(needle), "{}", fault.what);
            assert!(!fault.fault.installable());
        }
    }

    #[test]
    fn the_install_script_quotes_its_source_and_reports_what_it_did() {
        let script = install_script("/mnt/c/Users/O'Brien/cache/entangled-linux-x86_64");
        assert!(
            script.contains(r"'/mnt/c/Users/O'\''Brien/cache/entangled-linux-x86_64'"),
            "{script}"
        );
        // Copy-then-rename, or replacing a running engine fails with ETXTBSY.
        assert!(script.contains("entangled.part"), "{script}");
        assert!(script.contains(r#"mv -f "$dir/entangled.part" "$dir/entangled""#));

        let args = install_args("Ubuntu", "/mnt/c/x");
        assert_eq!(&args[..5], &["-d", "Ubuntu", "-e", "sh", "-c"]);

        let installed = parse_installed(
            "INSTALLED=/home/spider/.local/bin/entangled\n\
             VERSION=entangled 0.2.137\n\
             SHA256=4A999C1CCC92F9987EE4FC337F84030F0F4388B41589CD18054091E0ADD83BD3\n\
             ONPATH=no\n",
        )
        .expect("parsed");
        assert_eq!(installed.path, "/home/spider/.local/bin/entangled");
        assert_eq!(installed.version.as_deref(), Some("0.2.137"));
        assert_eq!(
            installed.sha256.as_deref(),
            Some("4a999c1ccc92f9987ee4fc337f84030f0f4388b41589cd18054091e0add83bd3")
        );
        assert!(!installed.on_path);

        // Noise around the report is ignored, and a report without the one
        // required key is not an install.
        assert!(parse_installed("cp: cannot stat '/mnt/c/x'\n").is_none());
    }

    /// The install script against a **real** distribution.
    ///
    /// Everything above it is fake-driven, which cannot catch the things a
    /// shell gets wrong: a quoting mistake, a `cp` that needs a directory that
    /// is not there, a `mv` onto a running binary. So this one runs the real
    /// script through real `wsl.exe` — into a throwaway `HOME`, so it never
    /// touches the developer's own `~/.local/bin`.
    ///
    /// Self-skipping, like the WHP and KVM tests: it needs `ENTANGLED_WSL_E2E`
    /// to name a Linux `entangled` binary as the distribution sees it (a
    /// `/mnt/...` path or one inside the distribution), and
    /// `ENTANGLED_WSL_E2E_DISTRO` to override the distribution.
    #[test]
    fn the_install_script_works_against_a_real_distribution() {
        let Some(source) = std::env::var("ENTANGLED_WSL_E2E")
            .ok()
            .filter(|v| !v.trim().is_empty())
        else {
            eprintln!("skipped: set ENTANGLED_WSL_E2E to a Linux entangled binary to run this");
            return;
        };
        let distro = std::env::var("ENTANGLED_WSL_E2E_DISTRO")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_DISTRO.to_string());
        let home = format!("/tmp/entangled-wsl-e2e-{}", std::process::id());

        // The real script, with only `$HOME` redirected — `install_args` is
        // otherwise reproduced verbatim.
        let args = vec![
            "-d".to_string(),
            distro.clone(),
            "-e".to_string(),
            "env".to_string(),
            format!("HOME={home}"),
            "sh".to_string(),
            "-c".to_string(),
            install_script(&source),
        ];
        let out = run_wsl(&args).expect("wsl.exe");
        let text = out.text();
        assert!(out.ok(), "install failed: {text}");

        let installed = parse_installed(&text).expect("the script reported what it did");
        assert_eq!(installed.path, format!("{home}/.local/bin/entangled"));
        assert!(installed.version.is_some(), "no version: {text}");
        assert!(
            !installed.on_path,
            "a throwaway HOME must not be on the launch PATH"
        );

        // The copy is byte-identical to the source: the digest the manager
        // verified on the Windows side survives the trip into the distribution.
        let sum = run_wsl(&[
            "-d".into(),
            distro.clone(),
            "-e".into(),
            "sh".into(),
            "-c".into(),
            format!("sha256sum {} | cut -d' ' -f1", sh_quote(&source)),
        ])
        .expect("sha256sum");
        assert_eq!(
            installed.sha256.as_deref(),
            Some(sum.text().trim()),
            "the installed copy differs from its source"
        );

        // And the engine it wrote really answers the probe a launch performs.
        let found = probe(&distro, Some(&installed.path)).expect("the installed engine runs");
        assert_eq!(found.version, installed.version);

        let _ = run_wsl(&[
            "-d".into(),
            distro,
            "-e".into(),
            "rm".into(),
            "-rf".into(),
            home,
        ]);
    }

    #[test]
    fn versions_parse_or_are_dropped() {
        assert_eq!(parse_version("entangled 0.2.137\n"), Some("0.2.137".into()));
        assert_eq!(parse_version("sh: entangled: not found"), None);
        assert_eq!(parse_version(""), None);
    }
}
