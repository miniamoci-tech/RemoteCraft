; Inno Setup script for the RemoteCraft Windows installer.
; Produces dist\RemoteCraft-Setup.exe (see .github/workflows/release.yml).
; Per-user install (no admin required); adds the install dir to the user PATH.

#define AppName "RemoteCraft"
#define AppVersion "0.1.6"
#define AppPublisher "miniamoci-tech"
#define AppURL "https://github.com/miniamoci-tech/RemoteCraft"

[Setup]
AppId=RemoteCraft.miniamoci-tech
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
DefaultDirName={localappdata}\Programs\RemoteCraft
DefaultGroupName=RemoteCraft
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\..\dist
OutputBaseFilename=RemoteCraft-Setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop icon for RemoteCraft"; Flags: checkedonce
Name: "addtopath"; Description: "Add RemoteCraft to my PATH (run it from any terminal)"; Flags: checkedonce

[Files]
Source: "..\..\target\release\remotecraft.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\remotecraft-relay.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Double-click to open the graphical launcher (no command line needed).
Name: "{group}\RemoteCraft"; Filename: "{app}\remotecraft.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\RemoteCraft"; Filename: "{app}\remotecraft.exe"; WorkingDir: "{app}"; Tasks: desktopicon
; A ready-to-type command prompt opened in the install directory, for the CLI.
Name: "{group}\RemoteCraft Prompt"; Filename: "{cmd}"; Parameters: "/k echo RemoteCraft. Try:  remotecraft view --relay HOST:21118 --id ID --password PW & cd /d ""{app}"""; WorkingDir: "{app}"
Name: "{group}\README"; Filename: "{app}\README.md"
Name: "{group}\Uninstall RemoteCraft"; Filename: "{uninstallexe}"

[Registry]
; Append {app} to the user PATH when the task is selected.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}'))

[Code]
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  // Avoid duplicating the entry if it is already present.
  Result := Pos(';' + Lowercase(Param) + ';', ';' + Lowercase(OrigPath) + ';') = 0;
end;
