; Inno Setup script for the Lumacap installer.
; Built by CI: ISCC /DAppVersion=x.y.z installer\lumacap.iss  (expects target\release\lumacap.exe)
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6F0D8C52-3B7A-4E41-9C1E-5B2A7D43A9E1}
AppName=Lumacap
AppVersion={#AppVersion}
AppVerName=Lumacap {#AppVersion}
AppPublisher=Lumacap contributors
AppPublisherURL=https://github.com/TheBuildShelf/lumacap
AppSupportURL=https://github.com/TheBuildShelf/lumacap/issues
AppUpdatesURL=https://github.com/TheBuildShelf/lumacap/releases
DefaultDirName={autopf}\Lumacap
DefaultGroupName=Lumacap
DisableProgramGroupPage=yes
; installs just for you without an admin prompt; the dialog lets you pick "all users" instead
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\dist
OutputBaseFilename=Lumacap-Setup
SetupIconFile=..\icon.ico
UninstallDisplayIcon={app}\lumacap.exe
UninstallDisplayName=Lumacap
LicenseFile=..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "..\target\release\lumacap.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Lumacap"; Filename: "{app}\lumacap.exe"
Name: "{autodesktop}\Lumacap"; Filename: "{app}\lumacap.exe"; Tasks: desktopicon

[Registry]
; remove the "Start with Windows" entry Lumacap may have created when it is uninstalled
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "Lumacap"; ValueType: none; Flags: dontcreatekey uninsdeletevalue

[Run]
Filename: "{app}\lumacap.exe"; Description: "Start Lumacap"; Flags: nowait postinstall skipifsilent
