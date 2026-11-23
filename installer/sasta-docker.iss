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
; a command name other than `docker` (e.g. sdocker): its own folder, which is the one put on PATH, so a real Docker's `docker` keeps working
Source: "{#Bin}\docker.exe"; DestDir: "{app}\cmd"; DestName: "{code:CmdName}.exe"; Flags: ignoreversion; Check: UseAlias
Source: "{#Bin}\{#Engine}"; DestDir: "{app}\cmd"; Flags: ignoreversion; Check: UseAlias

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

var CmdPage: TInputOptionWizardPage;
    DirPage: TInputDirWizardPage;

// the command the user types: `docker` (replaces Docker's) or `sdocker` (lives next to a real Docker); nothing else
function CmdName(Param: string): string;
begin
  if CmdPage <> nil then begin
    if CmdPage.Values[1] then Result := 'sdocker' else Result := 'docker';
  end else if CompareText(Trim(ExpandConstant('{param:CMDNAME|docker}')), 'sdocker') = 0 then Result := 'sdocker'
  else Result := 'docker';
end;

function UseAlias: Boolean;
begin
  Result := CompareText(CmdName(''), 'docker') <> 0;
end;

function PathDir: string;
begin
  if UseAlias then Result := ExpandConstant('{app}\cmd') else Result := ExpandConstant('{app}');
end;

// where the images and the Linux VM disk live (they grow to many GB): ask, with /DATADIR=... for silent installs
function DataDir(Param: string): string;
begin
  if DirPage <> nil then Result := DirPage.Values[0]
  else Result := ExpandConstant('{param:DATADIR|{localappdata}\rust-docker}');
end;

procedure InitializeWizard;
begin
  DirPage := CreateInputDirPage(wpSelectDir, 'Where to keep the data',
    'Where should images, containers and the Linux VM disk be stored?',
    'They can grow to many gigabytes. Pick a drive with enough free space, then click Next.', False, '');
  DirPage.Add('');
  DirPage.Values[0] := ExpandConstant('{param:DATADIR|{localappdata}\rust-docker}');
  CmdPage := CreateInputOptionPage(DirPage.ID, 'Command name', 'Which command do you want to type in the terminal?',
    'The desktop app (docker-gui) is installed either way.', True, False);
  CmdPage.Add('docker  - replaces the docker command (also the one of a Docker Desktop on your PATH)');
  CmdPage.Add('sdocker  - use Sasta Docker next to a real Docker');
  CmdPage.Values[0] := CompareText(Trim(ExpandConstant('{param:CMDNAME|docker}')), 'sdocker') <> 0;
  CmdPage.Values[1] := not CmdPage.Values[0];
end;

procedure DropFromPath(Dir: string);
var P: string;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', P) then Exit;
  P := ';' + P + ';';
  StringChangeEx(P, ';' + Dir + ';', ';', True);
  if (P <> '') and (P[1] = ';') then Delete(P, 1, 1);
  if (P <> '') and (P[Length(P)] = ';') then Delete(P, Length(P), 1);
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', P);
end;

// put the chosen folder on the user's PATH so the command works in any new terminal
procedure CurStepChanged(CurStep: TSetupStep);
var P: string;
begin
  if CurStep = ssPostInstall then begin
    DropFromPath(ExpandConstant('{app}'));
    DropFromPath(ExpandConstant('{app}\cmd'));
    if not RegQueryStringValue(HKCU, EnvKey, 'Path', P) then P := '';
    if (P <> '') and (P[Length(P)] <> ';') then P := P + ';';
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', P + PathDir);
    // every new terminal and the desktop app then use the chosen data folder
    ForceDirectories(DataDir(''));
    RegWriteExpandStringValue(HKCU, EnvKey, 'RUST_DOCKER_HOME', DataDir(''));
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then begin
    RegDeleteValue(HKCU, EnvKey, 'RUST_DOCKER_HOME'); // the data folder itself stays: it holds your images and volumes
    DropFromPath(ExpandConstant('{app}'));
    DropFromPath(ExpandConstant('{app}\cmd'));
  end;
end;
