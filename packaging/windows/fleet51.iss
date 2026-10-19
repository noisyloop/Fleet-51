; Fleet-51 for Windows: a per-user installer (no administrator rights needed).
; Built by GitHub Actions: iscc /DVersion=x.y.z /DSourceDir=<dir with the .exe files> fleet51.iss

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\release"
#endif

[Setup]
AppId={{6F1E5A51-A1E5-4F1E-B51A-F1EE751A1E51}
AppName=Fleet-51
AppVersion={#Version}
AppPublisher=noisyloop
AppPublisherURL=https://github.com/noisyloop/Fleet-51
DefaultDirName={localappdata}\Programs\Fleet-51
DefaultGroupName=Fleet-51
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=Fleet-51-windows-setup
SetupIconFile=fleet51.ico
UninstallDisplayIcon={app}\fleet51-app.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=force

[Files]
Source: "{#SourceDir}\fleet51.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\fleet51-app.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Fleet-51"; Filename: "{app}\fleet51-app.exe"
Name: "{userdesktop}\Fleet-51"; Filename: "{app}\fleet51-app.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
; An update replaces the running service with the new version, keeping bots and chats.
Filename: "{app}\fleet51-app.exe"; Description: "Open Fleet-51"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop the bots and the model engine before removing the programs. Chats and bot folders
; (%USERPROFILE%\.fleet51 and %USERPROFILE%\Fleet-51) are kept.
Filename: "{app}\fleet51.exe"; Parameters: "down"; Flags: runhidden; RunOnceId: "StopFleet51"
