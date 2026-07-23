; Inno Setup script for dbTool — per-user install, no admin required.
; Installs to %LOCALAPPDATA%\Programs\dbTool, creates a Start-menu entry,
; and adds the install dir to the user PATH (removed again on uninstall).

#define AppName "dbTool"
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{com.rinoceronte.dbtool}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Rinoceronte
DefaultDirName={localappdata}\Programs\dbTool
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=dbTool-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
ChangesEnvironment=yes
UninstallDisplayIcon={app}\dbTool.exe

[Files]
Source: "..\..\target\release\dbtool.exe"; DestDir: "{app}"; DestName: "dbTool.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\dbTool.exe"; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}'))

[Code]
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Lowercase(Param) + ';', ';' + Lowercase(OrigPath) + ';') = 0;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, App: string;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    App := ExpandConstant('{app}');
    if RegQueryStringValue(HKCU, 'Environment', 'Path', Path) then
    begin
      if (StringChangeEx(Path, ';' + App, '', True) > 0) or
         (StringChangeEx(Path, App + ';', '', True) > 0) or
         (StringChangeEx(Path, App, '', True) > 0) then
        RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Path);
    end;
  end;
end;
