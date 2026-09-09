; Inno Setup script for Entangled Desktop (the Windows build).
;
; Compiled by .github/workflows/release.yml on every push to main:
;
;   ISCC.exe /DAppVersion=<MAJOR.MINOR.PATCH> /DBinDir=<cargo release dir> installer\entangled.iss
;
; Local dry run (after `cargo build --release -p entangled -p entangled-manager`):
;
;   ISCC.exe /DAppVersion=0.2.0 /DBinDir=..\target\release installer\entangled.iss
;
; Written for Inno Setup 6 (the current major). No code signing yet — there is
; no certificate; SignTool/SignedUninstaller are the TODO markers for when one
; exists.
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
