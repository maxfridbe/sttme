; Inno Setup Script for sttme (Instant Push-to-Talk Speech-to-Text)
; Produces a standalone, reproducible Windows installer (sttme-windows-x86_64-setup.exe)
; Built via Docker using amake/innosetup without needing Windows or PowerShell.

#ifndef AppVersion
  #define AppVersion "26.1029.01"
#endif

#ifndef SourceExe
  #define SourceExe "dist\sttme-windows-x86_64\sttme.exe"
#endif

#ifndef OutputDir
  #define OutputDir "dist"
#endif

#ifndef OutputBaseFilename
  #define OutputBaseFilename "sttme-windows-x86_64-setup"
#endif

[Setup]
AppId={{C6B8D149-163D-4A59-86B5-B5E3C71A9140}
AppName=sttme
AppVersion={#AppVersion}
AppVerName=sttme v{#AppVersion}
AppPublisher=maxfridbe
AppPublisherURL=https://github.com/maxfridbe/sttme
AppSupportURL=https://github.com/maxfridbe/sttme/issues
AppUpdatesURL=https://github.com/maxfridbe/sttme/releases
DefaultDirName={autopf}\sttme
DefaultGroupName=sttme
DisableProgramGroupPage=yes
LicenseFile=LICENSE
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseFilename}
SourceDir=..\..
SetupIconFile=packaging\windows\sttme.ico
UninstallDisplayIcon={app}\sttme.exe
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startup"; Description: "&Start sttme automatically when Windows signs in"; GroupDescription: "Startup options:"
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked
Name: "addtopath"; Description: "Add sttme to User &PATH (recommended for CLI usage)"; GroupDescription: "Environment:"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "sttme.exe"; Flags: ignoreversion
Source: "README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "packaging\windows\sttme.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\sttme"; Filename: "{app}\sttme.exe"; IconFilename: "{app}\sttme.ico"; Comment: "Instant Hold-to-Talk Speech-to-Text"
Name: "{group}\Uninstall sttme"; Filename: "{uninstallexe}"
Name: "{autodesktop}\sttme"; Filename: "{app}\sttme.exe"; IconFilename: "{app}\sttme.ico"; Comment: "Instant Hold-to-Talk Speech-to-Text"; Tasks: desktopicon

[Registry]
; Launch at Windows Sign-In (optional task)
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "sttme"; ValueData: """{app}\sttme.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\sttme.exe"; Description: "&Launch sttme now"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvironmentKey = 'Environment';

function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  AppDir: string;
  OrigPath: string;
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('addtopath') then
    begin
      AppDir := ExpandConstant('{app}');
      if NeedsAddPath(AppDir) then
      begin
        if RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath) then
        begin
          if (Length(OrigPath) > 0) and (OrigPath[Length(OrigPath)] <> ';') then
            OrigPath := OrigPath + ';';
          OrigPath := OrigPath + AppDir;
        end
        else
          OrigPath := AppDir;
        RegWriteStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath);
      end;
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AppDir: string;
  OrigPath: string;
  P: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    AppDir := ExpandConstant('{app}');
    if RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath) then
    begin
      P := Pos(';' + Uppercase(AppDir) + ';', ';' + Uppercase(OrigPath) + ';');
      if P > 0 then
      begin
        if P = 1 then
          Delete(OrigPath, 1, Length(AppDir) + 1)
        else
          Delete(OrigPath, P - 1, Length(AppDir) + 1);
        RegWriteStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath);
      end;
    end;
  end;
end;
