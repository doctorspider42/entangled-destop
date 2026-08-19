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

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
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
LicenseFile=..\LICENSE
; TODO(code signing): once a certificate exists, add SignTool= here and sign
; both the installer and the binaries; until then SmartScreen will warn.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\entangled-manager.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\entangled.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "entangled.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Entangled Desktop"; Filename: "{app}\entangled-manager.exe"; IconFilename: "{app}\entangled.ico"
Name: "{group}\Uninstall Entangled Desktop"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Entangled Desktop"; Filename: "{app}\entangled-manager.exe"; IconFilename: "{app}\entangled.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\entangled-manager.exe"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
