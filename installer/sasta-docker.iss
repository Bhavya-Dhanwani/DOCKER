; Windows installer: the `docker` command, the `docker-gui` app and the Linux engine for the VM.
; Build:  iscc /DArch=arm64 /DVersion=0.1.0 /DBin=..\target\release installer\sasta-docker.iss   (Arch: x64 or arm64)
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef Version
  #define Version "0.1.0"
#endif
#ifndef Bin
  #define Bin "..\target\release"
#endif
#if Arch == "arm64"
  #define Engine "docker-linux-aarch64"
#else
  #define Engine "docker-linux-x86_64"
#endif

[Setup]
AppId={{6D0C4B52-5A57-4A0F-9B65-5A57A0D0C4B5}
AppName=Sasta Docker
AppVersion={#Version}
AppPublisher=Bhavya Dhanawani
DefaultDirName={localappdata}\Programs\Sasta Docker
DefaultGroupName=Sasta Docker
PrivilegesRequired=lowest
ArchitecturesAllowed={#Arch}
ArchitecturesInstallIn64BitMode={#Arch}
OutputBaseFilename=SastaDocker-Setup-{#Arch}
OutputDir=..\dist
Compression=lzma2
SolidCompression=yes
ChangesEnvironment=yes
UninstallDisplayName=Sasta Docker
DisableProgramGroupPage=yes
WizardStyle=modern
LicenseFile=..\LICENSE
SetupIconFile=..\assets\logo.ico
UninstallDisplayIcon={app}\docker-gui.exe

[Files]
Source: "{#Bin}\docker.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Bin}\docker-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Bin}\{#Engine}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Sasta Docker"; Filename: "{app}\docker-gui.exe"
Name: "{autodesktop}\Sasta Docker"; Filename: "{app}\docker-gui.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
Name: "setupvm"; Description: "Set up the Linux engine now (needs WSL2; takes a minute or two)"

[Run]
; the data folder is passed explicitly: the environment variable written below is not visible to this process yet
Filename: "{cmd}"; Parameters: "/C set ""RUST_DOCKER_HOME={code:DataDir}"" && ""{app}\docker.exe"" vm init"; StatusMsg: "Setting up the Linux engine..."; Tasks: setupvm; Flags: waituntilterminated
Filename: "{app}\docker-gui.exe"; Description: "Open Sasta Docker"; Flags: postinstall nowait skipifsilent

[Code]
const EnvKey = 'Environment';

var DataPage: TInputDirWizardPage;

// where the images and the Linux VM disk live (they grow to many GB): ask, with /DATADIR=... for silent installs
function DataDir(Param: string): string;
begin
  if DataPage <> nil then Result := DataPage.Values[0]
  else Result := ExpandConstant('{param:DATADIR|{localappdata}\rust-docker}');
end;

procedure InitializeWizard;
begin
  DataPage := CreateInputDirPage(wpSelectDir, 'Where to keep the data',
    'Where should images, containers and the Linux VM disk be stored?',
    'They can grow to many gigabytes. Pick a drive with enough free space, then click Next.', False, '');
  DataPage.Add('');
  DataPage.Values[0] := ExpandConstant('{param:DATADIR|{localappdata}\rust-docker}');
end;

// add {app} to the user's PATH so `docker` works in any new terminal
procedure CurStepChanged(CurStep: TSetupStep);
var P: string;
begin
  if CurStep = ssPostInstall then begin
    if not RegQueryStringValue(HKCU, EnvKey, 'Path', P) then P := '';
    if Pos(';' + Lowercase(ExpandConstant('{app}')) + ';', ';' + Lowercase(P) + ';') = 0 then begin
      if (P <> '') and (P[Length(P)] <> ';') then P := P + ';';
      RegWriteExpandStringValue(HKCU, EnvKey, 'Path', P + ExpandConstant('{app}'));
    end;
    // every new terminal and the desktop app then use the chosen data folder
    ForceDirectories(DataDir(''));
    RegWriteExpandStringValue(HKCU, EnvKey, 'RUST_DOCKER_HOME', DataDir(''));
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var P, App: string; I: Integer;
begin
  if CurUninstallStep = usPostUninstall then begin
    RegDeleteValue(HKCU, EnvKey, 'RUST_DOCKER_HOME'); // the data folder itself stays: it holds your images and volumes
    if RegQueryStringValue(HKCU, EnvKey, 'Path', P) then begin
      App := ExpandConstant('{app}');
      I := Pos(';' + Lowercase(App), ';' + Lowercase(P));
      if I > 0 then begin
        Delete(P, I, Length(App) + 1);
        RegWriteExpandStringValue(HKCU, EnvKey, 'Path', P);
      end;
    end;
  end;
end;
