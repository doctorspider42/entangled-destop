; Inno Setup script for Entangled Desktop (the Windows build).
;
; Compiled by .github/workflows/release.yml on every push to main:
;
;   ISCC.exe /DAppVersion=<MAJOR.MINOR.PATCH> /DBinDir=<cargo release dir> installer\entangled.iss
;
; Local dry run (after `cargo build --release -p entangled -p entangled-manager`,
; and with a CLOUDHV.fd in artifacts\firmware\ or named by /DFirmwareDir):
;
;   ISCC.exe /DAppVersion=0.2.0 /DBinDir=..\target\release installer\entangled.iss
;
; Written for Inno Setup 7, which is what release.yml pins (INNO_SETUP_URL) and
; what the ISPP file primitives below were tested against. No code signing yet —
; there is no certificate; SignTool/SignedUninstaller are the TODO markers for
; when one exists.
;
; THE FIRMWARE IS NOT OPTIONAL. Every UEFI machine — which is every Ubuntu and
; every Fedora — boots through artifacts\firmware\CLOUDHV.fd, and the only build
; of it is a Linux EDK2 build. A Windows user has no way to produce one, so a
; setup that shipped without it would install a program that cannot create the
; machines its own README advertises. That is exactly the bug this file was
; changed to fix, so the entry below has NO `skipifsourcedoesntexist`: a
; compile with no firmware to hand must fail here rather than four screens into
; someone's create-machine wizard. Point /DFirmwareDir= at a directory holding
; CLOUDHV.fd, or let the default find a checkout that ran
; guest/firmware/build-cloudhv.sh.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif
#ifndef FirmwareDir
  #define FirmwareDir "..\artifacts\firmware"
#endif

; The licence page must show what this installer redistributes, not only what
; we wrote — and it redistributes EDK2 (BSD-2-Clause-Patent) as the firmware.
; Inno takes exactly one LicenseFile, so one is built here at compile time from
; the two files that already exist, rather than keeping a third copy of either
; in the tree for them to drift apart from.
;
; Done with the preprocessor's own file primitives rather than `Exec("cmd.exe",
; "/c copy …")`: Exec does not reliably wait for a console process, and the
; version of this that shelled out failed its own existence check on a clean
; tree and passed on the second run — the worst way for a build step to be
; wrong. FileOpen/FileRead/SaveStringToFile are synchronous. They also
; normalise the two files' line endings on the way through, which is what the
; licence memo wants.
#define CombinedLicense "LICENSE-and-notices.txt"
#define CombinedPath AddBackslash(SourcePath) + CombinedLicense
#define LicenseHandle
#sub AppendLicenseLine
  #expr SaveStringToFile(CombinedPath, FileRead(LicenseHandle) + NewLine, True)
#endsub
#expr SaveStringToFile(CombinedPath, "", False)
#for {LicenseHandle = FileOpen(AddBackslash(SourcePath) + "..\LICENSE"); \
      LicenseHandle && !FileEof(LicenseHandle); ""} AppendLicenseLine
#expr FileClose(LicenseHandle)
#for {LicenseHandle = FileOpen(AddBackslash(SourcePath) + "..\THIRD-PARTY-NOTICES.txt"); \
      LicenseHandle && !FileEof(LicenseHandle); ""} AppendLicenseLine
#expr FileClose(LicenseHandle)
#if FileSize(CombinedPath) < 1024
  #error Could not build the licence page from ..\LICENSE and ..\THIRD-PARTY-NOTICES.txt
#endif

#define AppName "Entangled Desktop"
#define Publisher "doctorspider42"
#define RepoUrl "https://github.com/doctorspider42/entangled-destop"

[Setup]
; The AppId is the upgrade identity: it was generated once and must NEVER
; change, or upgrades become side-by-side installs. Doubled first brace is
; Inno's escape for a literal '{'.
AppId={{EF1CC6F3-68F0-43EE-BABF-12296005FFDE}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} v{#AppVersion}
AppPublisher={#Publisher}
AppPublisherURL={#RepoUrl}
AppSupportURL={#RepoUrl}/issues
AppUpdatesURL={#RepoUrl}/releases
VersionInfoVersion={#AppVersion}

; 64-bit only, matching the x86-64 VMM. {autopf} resolves to Program Files
; for an administrative install (the default here).
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={autopf}\Entangled Desktop
DefaultGroupName=Entangled Desktop
DisableProgramGroupPage=yes
PrivilegesRequired=admin

; Upgrade-in-place: reinstalling over an older version replaces files in the
; same {app}. CloseApplications asks a running entangled-manager politely to
; close (WM_CLOSE first) instead of failing on locked files;
; RestartApplications relaunches it afterwards.
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=yes

OutputDir=Output
OutputBaseFilename=entangled-desktop-{#AppVersion}-setup
SetupIconFile=entangled.ico
UninstallDisplayIcon={app}\entangled-manager.exe
UninstallDisplayName={#AppName}
WizardStyle=modern
SolidCompression=yes
Compression=lzma2
; Our Apache-2.0 licence followed by THIRD-PARTY-NOTICES.txt (EDK2), built just
; above. A user agreeing on this page has been shown both.
LicenseFile={#CombinedLicense}
; TODO(code signing): once a certificate exists, add SignTool= here and sign
; both the installer and the binaries; until then SmartScreen will warn.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
; Unticked on purpose. It downloads ~20 MiB and talks to WSL, and a person who
; wanted a Windows-only setup must be able to walk past it without reading a
; paragraph. Everything it can do wrong is a sentence on the finished page and
; never a failed installation — see SetupWslEngine below.
Name: "wslengine"; Description: "Also set up the Linux engine in WSL (enables the KVM backend and 3D acceleration)"; GroupDescription: "Linux engine:"; Flags: unchecked

[Files]
Source: "{#BinDir}\entangled-manager.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\entangled.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "entangled.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

; The UEFI firmware. `artifacts\firmware\` next to the executables is not a new
; layout invented for the installer — it is the relative path `entangled` has
; always searched and every generated profile has always named, so a machine
; created here works whether it is started from the manager, from this
; directory, or from anywhere at all (apps/entangled/src/firmware.rs resolves it
; relative to the executable). Per-machine and read-only, like the binaries.
Source: "{#FirmwareDir}\CLOUDHV.fd"; DestDir: "{app}\artifacts\firmware"; Flags: ignoreversion
; What was built, from which upstream tag, and its SHA-256 — so the 4 MiB opaque
; blob above is auditable on the machine it lands on. Optional only because a
; firmware handed over by /DFirmwareDir may not carry one; the release pipeline
; always does.
Source: "{#FirmwareDir}\CLOUDHV.fd.provenance"; DestDir: "{app}\artifacts\firmware"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\Entangled Desktop"; Filename: "{app}\entangled-manager.exe"; IconFilename: "{app}\entangled.ico"
Name: "{group}\Uninstall Entangled Desktop"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Entangled Desktop"; Filename: "{app}\entangled-manager.exe"; IconFilename: "{app}\entangled.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\entangled-manager.exe"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

; ---------------------------------------------------------------------------
; The optional Linux engine in WSL
; ---------------------------------------------------------------------------
;
; WHY THIS IS HERE. The manager can run a machine two ways: entangled.exe
; natively on WHP, or the *Linux* build of entangled inside WSL, where there is
; a real /dev/kvm — which is where the KVM backend and 3D acceleration live.
; This installer ships no Linux binary (it cannot; it is a Windows setup), so
; that backend's very first machine used to die with
;
;   <3>WSL (13) ERROR: CreateProcessEntryCommon:505: execvpe entangled failed 2
;
; The manager has been able to fix that from Settings for a while. The task
; above is the same fix, offered at the moment the user is already answering
; questions about their installation, so a person who wants KVM never meets
; that message at all.
;
; WHY IT RUNS A CLI AND NOT A SCRIPT OF ITS OWN. The download is verified
; against a SHA-256 the release pipeline compiles into the binaries — nobody
; types it, nothing fetches it. An .iss cannot read a constant inside an .exe,
; so it does the only thing it sensibly can: run the program that holds it.
; `entangled wsl install-engine` is exactly the code path the manager's button
; takes (control_api::wsl_engine), and its exit codes are its interface. They
; are mirrored in the constants below and pinned by a Rust test.
;
; THE ELEVATION TRAP, AND WHICH WAY THIS GOES. PrivilegesRequired=admin, so
; Setup always runs elevated. **WSL distributions are registered per Windows
; user.** If a standard user starts Setup and UAC elevates to a *different*
; administrator account, an ordinary post-install step would ask that
; administrator's WSL — which usually has no distributions at all — and the
; engine would land in the wrong profile or nowhere, with a message about the
; wrong machine.
;
; Two honest ways out: run the step as the originating user, or record the
; intent and let the manager (which always runs as the user) do it on first
; launch. This takes the first, with ExecAsOriginalUser:
;
;   * the work happens while the user is watching a progress bar that already
;     says what it is doing, rather than as a surprise on some later launch;
;   * it needs no new persistent state — no "pending setup" flag to write, to
;     read, to expire, or to get stuck on;
;   * an engine in ~/.local/bin is per-user anyway, so "the user who ran Setup"
;     is exactly the right account to install it for, and the same one whose
;     `wsl --list` the CLI is about to read.
;
; The residual case is Setup launched from an already-elevated shell, where
; Windows has no other user to hand back to: then the "original user" is that
; administrator, and the engine is installed for them. That is not silent —
; the finished page names the distribution and what happened — and the manager
; run by anyone else will simply offer the install again for their own profile.
;
; NOTHING HERE MAY FAIL THE INSTALLATION. The exit code is only ever used to
; choose a sentence. A user who wanted a Windows-only setup, or whose WSL is
; not in the mood, gets a complete, working Windows installation either way.

[Code]
const
  { The exit codes of `entangled wsl install-engine`
    (apps/entangled/src/wsl_engine.rs). This script cannot be compiled against
    the Rust enum, so the Rust side has a test that pins every one of these
    values; change one there and this table must change with it. }
  WslOk           = 0;
  WslFailed       = 1;
  WslNoWsl        = 2;
  WslNoDistro     = 3;
  WslUnusable     = 4;
  WslNoDownload   = 5;
  WslBadDigest    = 6;
  WslNoPin        = 7;
  WslTimedOut     = 8;
  WslNotWindows   = 9;
  { Not one of its codes: the program never started. }
  WslNotStarted   = -1;

  { Long enough for a distribution that has never been started to boot and for
    a ~20 MiB download on a slow line; short enough that a wedged WSL is a
    sentence rather than an installation that never ends. The CLI enforces it
    itself, so this is a value passed, not a wait implemented here. }
  WslTimeoutSecs  = 600;

var
  WslEngineAttempted: Boolean;
  WslEngineResult: Integer;

{ One sentence per outcome, each naming the way out. The manager is the way out
  in most of them because it is the surface that can retry, show progress, and
  let the user pick a different distribution. }
function WslEngineSentence(): String;
var
  Failure: String;
begin
  if WslEngineResult = WslOk then
  begin
    Result := 'The Linux engine for WSL is set up. New machines can run on the ' +
      'WSL (KVM) backend, with 3D acceleration.';
    exit;
  end;

  Failure := 'The Linux engine for WSL was not set up. Everything else installed ' +
    'normally. ';
  case WslEngineResult of
    WslNoWsl:
      Result := Failure + 'This machine has no usable WSL: run "wsl --install" in ' +
        'a terminal (it needs a restart), then finish from the manager — ' +
        'Settings, Install the Linux engine.';
    WslNoDistro:
      Result := Failure + 'WSL has no distribution called "Ubuntu": install one ' +
        'with "wsl --install -d Ubuntu", then finish from the manager, which also ' +
        'lets you choose a different distribution.';
    WslUnusable:
      Result := Failure + 'The distribution already holds an engine that will not ' +
        'run. The manager says exactly why — Settings, Linux engine.';
    WslNoDownload:
      Result := Failure + 'The download did not succeed — no network, or this ' +
        'release published no Linux engine. Try again from the manager: Settings, ' +
        'Install the Linux engine.';
    WslBadDigest:
      Result := Failure + 'What was downloaded did not match the digest built into ' +
        'this program, so it was deleted. Nothing unverified was installed.';
    WslNoPin:
      Result := Failure + 'This build carries no verified digest for the Linux ' +
        'engine, so nothing was downloaded — normal for a build made outside the ' +
        'release pipeline. Build the Linux engine yourself and name it in the ' +
        'manager: Settings, Linux engine.';
    WslTimedOut:
      Result := Failure + 'It did not finish within ' + IntToStr(WslTimeoutSecs) +
        ' seconds; a WSL distribution that has never been started can take minutes ' +
        'to boot. Nothing half-written was left behind — try again from the ' +
        'manager: Settings, Install the Linux engine.';
    WslNotWindows:
      Result := Failure + 'The engine reported that it is not running on Windows, ' +
        'which should not be possible here. Please report it.';
    WslNotStarted:
      Result := Failure + 'entangled.exe could not be started as your own user ' +
        'account, which is the account whose WSL this would go into. Finish from ' +
        'the manager: Settings, Install the Linux engine.';
  else
    Result := Failure + 'Finish it from the manager: Settings, Install the Linux ' +
      'engine.';
  end;
  Result := Result + #13#10#13#10 +
    'Details: %LOCALAPPDATA%\entangled\engine\install-engine.log';
end;

{ Runs the CLI as the user who started Setup — see the long note above. }
procedure SetupWslEngine();
var
  ResultCode: Integer;
begin
  WslEngineAttempted := True;
  WslEngineResult := WslNotStarted;
  if WizardForm <> nil then
  begin
    WizardForm.StatusLabel.Caption :=
      'Setting up the Linux engine in WSL (a distribution that has not been ' +
      'started yet has to boot first)...';
    WizardForm.StatusLabel.Refresh();
  end;
  if ExecAsOriginalUser(ExpandConstant('{app}\entangled.exe'),
       'wsl install-engine --quiet --timeout ' + IntToStr(WslTimeoutSecs),
       ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    WslEngineResult := ResultCode;
  { Into the /LOG file too, which is all a silent install has. }
  Log('WSL engine setup: exit code ' + IntToStr(WslEngineResult) + ' — ' +
    WslEngineSentence());
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('wslengine') then
    SetupWslEngine();
end;

procedure CurPageChanged(CurPageID: Integer);
var
  Below: Integer;
begin
  { The finished page is where the user finds out. Appended rather than
    replacing, so "Setup has finished installing" still reads first: the
    installation succeeded whatever the engine did.

    AdjustHeight and the nudge below it are not decoration. Inno sizes
    FinishedLabel to the text it put there, so a longer caption is simply
    clipped — which is how the first version of this silently showed nothing
    at all. Grow the label to its new text, then move the "Launch" checkbox
    out from under it if it would now overlap. }
  if (CurPageID = wpFinished) and WslEngineAttempted and (WizardForm <> nil) then
  begin
    WizardForm.FinishedLabel.Caption :=
      WizardForm.FinishedLabel.Caption + #13#10#13#10 + WslEngineSentence();
    WizardForm.FinishedLabel.AdjustHeight();
    Below := WizardForm.FinishedLabel.Top + WizardForm.FinishedLabel.Height + ScaleY(12);
    if WizardForm.RunList.Top < Below then
      WizardForm.RunList.Top := Below;
  end;
end;
