; Inno Setup script for Arca.
;
; Produces a single setup.exe: wizard, license, folder choice, uninstaller in
; "Installed apps", and detection of a previous install so it upgrades instead
; of piling up. Handwriting that in Rust would mean reimplementing, badly, what
; this already does correctly.
;
; The install is per user: LocalAppData, HKCU only, no administrator. That also
; matches what the shell extension needs, since its registration lives in HKCU.
;
; Build it with:
;   ISCC.exe /DVersion=0.2.0 /DBinDir=..\target\x86_64-pc-windows-msvc\release ^
;            /DShellDir=arca-shell\target\x86_64-pc-windows-msvc\release windows\arca.iss

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\x86_64-pc-windows-msvc\release"
#endif
#ifndef ShellDir
  #define ShellDir "arca-shell\target\x86_64-pc-windows-msvc\release"
#endif

#define AppName "Arca"
#define Publisher "Arca Project"
#define Url "https://github.com/THIONG/arca"
; Must match CLSID_ARCA_CLASSIC in arca-shell/src/lib.rs.
#define ClassicClsid "{{B528A7F3-C889-4C98-B052-5D7F7F778E14}"

[Setup]
AppId={{7C4E0E4A-6C0D-4C21-9E0B-2B5D0F1A9C77}
AppName={#AppName}
AppVersion={#Version}
AppPublisher={#Publisher}
AppPublisherURL={#Url}
AppSupportURL={#Url}
DefaultDirName={localappdata}\Programs\Arca
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=arca-setup-{#Version}-x86_64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Per user: no UAC prompt, and the shell extension registers in HKCU anyway.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName={#AppName} {#Version}
UninstallDisplayIcon={app}\arca-gui.exe
SetupIconFile=..\brand\arca.ico
ChangesEnvironment=yes
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"

[Tasks]
Name: "shellmenu"; Description: "{cm:TaskShellMenu}"; GroupDescription: "{cm:GroupIntegration}"
Name: "fileassoc"; Description: "{cm:TaskFileAssoc}"; GroupDescription: "{cm:GroupIntegration}"
Name: "addtopath"; Description: "{cm:TaskAddToPath}"; GroupDescription: "{cm:GroupIntegration}"

[CustomMessages]
english.TaskShellMenu=Add Arca to the Explorer context menu
english.TaskFileAssoc=Let Arca open .zip, .tar, .gz and .tgz files
english.TaskAddToPath=Add the arca command to PATH
english.GroupIntegration=System integration:
english.RestartExplorer=The Explorer needs to restart for the context menu to appear. Restart it now?
spanish.TaskShellMenu=Anadir Arca al menu contextual del Explorador
spanish.TaskFileAssoc=Permitir que Arca abra ficheros .zip, .tar, .gz y .tgz
spanish.TaskAddToPath=Anadir el comando arca al PATH
spanish.GroupIntegration=Integracion con el sistema:
spanish.RestartExplorer=El Explorador tiene que reiniciarse para que aparezca el menu contextual. Reiniciarlo ahora?

[Files]
Source: "{#BinDir}\arca.exe";            DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\arca-gui.exe";        DestDir: "{app}"; Flags: ignoreversion
; The Explorer loads this DLL and, by design, never unloads it: DllCanUnloadNow
; returns S_FALSE on purpose. So it stays locked and neither an upgrade nor an
; uninstall can touch it while the Explorer is running. RestartExplorer below
; handles the normal case; these flags are the fallback if that is not enough.
Source: "{#ShellDir}\arca_shell.dll";    DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "AppxManifest.xml";              DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE";                    DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md";                  DestDir: "{app}"; Flags: ignoreversion
Source: "assets\*";                      DestDir: "{app}\Assets"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\arca-gui.exe"

[Registry]
; uninsdeletekey on a child only removes that child, so the parent survives as
; an empty shell. These three entries own the whole subtree instead, which is
; what actually leaves the registry as it was found.
Root: HKCU; Subkey: "Software\Classes\CLSID\{#ClassicClsid}"; ValueType: none; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe"; ValueType: none; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Arca"; ValueType: none; Flags: uninsdeletekey; Tasks: fileassoc

; The classic context menu, the one behind "Show more options" and the only one
; on Windows 10. The modern menu comes from the sparse MSIX package instead.
Root: HKCU; Subkey: "Software\Classes\CLSID\{#ClassicClsid}\InprocServer32"; ValueType: string; ValueName: ""; ValueData: "{app}\arca_shell.dll"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\CLSID\{#ClassicClsid}\InprocServer32"; ValueType: string; ValueName: "ThreadingModel"; ValueData: "Apartment"; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\*\shellex\ContextMenuHandlers\Arca"; ValueType: string; ValueName: ""; ValueData: "{#ClassicClsid}"; Flags: uninsdeletekey; Tasks: shellmenu
Root: HKCU; Subkey: "Software\Classes\Directory\shellex\ContextMenuHandlers\Arca"; ValueType: string; ValueName: ""; ValueData: "{#ClassicClsid}"; Flags: uninsdeletekey; Tasks: shellmenu

; File associations. Three registrations are needed and one alone is not
; enough: the ProgID says how to open it, OpenWithProgids fills "Open with",
; and Capabilities plus RegisteredApplications is what puts Arca in
; Settings > Default apps.
Root: HKCU; Subkey: "Software\Classes\Arca.zip"; ValueType: string; ValueName: ""; ValueData: "ZIP archive"; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.zip\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\arca-gui.exe,0"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.zip\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\arca-gui.exe"" ""%1"""; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.tar"; ValueType: string; ValueName: ""; ValueData: "TAR archive"; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.tar\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\arca-gui.exe,0"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.tar\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\arca-gui.exe"" ""%1"""; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.targz"; ValueType: string; ValueName: ""; ValueData: "Compressed TAR archive"; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.targz\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\arca-gui.exe,0"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.targz\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\arca-gui.exe"" ""%1"""; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.tgz"; ValueType: string; ValueName: ""; ValueData: "Compressed TAR archive"; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.tgz\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\arca-gui.exe,0"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Arca.tgz\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\arca-gui.exe"" ""%1"""; Tasks: fileassoc

Root: HKCU; Subkey: "Software\Classes\.zip\OpenWithProgids"; ValueType: string; ValueName: "Arca.zip"; ValueData: ""; Flags: uninsdeletevalue; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\.tar\OpenWithProgids"; ValueType: string; ValueName: "Arca.tar"; ValueData: ""; Flags: uninsdeletevalue; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\.gz\OpenWithProgids"; ValueType: string; ValueName: "Arca.targz"; ValueData: ""; Flags: uninsdeletevalue; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\.tgz\OpenWithProgids"; ValueType: string; ValueName: "Arca.tgz"; ValueData: ""; Flags: uninsdeletevalue; Tasks: fileassoc

; Windows shows FriendlyAppName in "Open with" and in Settings. Without it the
; list falls back to the executable name, which reads like something the user
; did not install on purpose.
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Arca"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\arca-gui.exe"" ""%1"""; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe\SupportedTypes"; ValueType: string; ValueName: ".zip"; ValueData: ""; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe\SupportedTypes"; ValueType: string; ValueName: ".tar"; ValueData: ""; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe\SupportedTypes"; ValueType: string; ValueName: ".gz"; ValueData: ""; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\Applications\arca-gui.exe\SupportedTypes"; ValueType: string; ValueName: ".tgz"; ValueData: ""; Tasks: fileassoc

Root: HKCU; Subkey: "Software\Arca\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Arca"; Flags: uninsdeletekey; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Arca\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Fast, safe archiver"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Arca\Capabilities\FileAssociations"; ValueType: string; ValueName: ".zip"; ValueData: "Arca.zip"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Arca\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tar"; ValueData: "Arca.tar"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Arca\Capabilities\FileAssociations"; ValueType: string; ValueName: ".gz"; ValueData: "Arca.targz"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Arca\Capabilities\FileAssociations"; ValueType: string; ValueName: ".tgz"; ValueData: "Arca.tgz"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "Arca"; ValueData: "Software\Arca\Capabilities"; Flags: uninsdeletevalue; Tasks: fileassoc

[Code]
const
  ModernPackage = 'Arca.Archivador';

function RunHidden(const Command: string): Integer;
var
  Code: Integer;
begin
  Result := -1;
  if Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
          '-NoProfile -NonInteractive -InputFormat None -ExecutionPolicy Bypass -WindowStyle Hidden -Command "' + Command + '"',
          '', SW_HIDE, ewWaitUntilTerminated, Code) then
    Result := Code;
end;

// The modern Windows 11 menu cannot be declared in the registry: it comes from
// a sparse MSIX package, which has to be registered through Add-AppxPackage.
// It needs Developer Mode, so a failure here is not fatal; the classic menu
// still works.
// Windows takes the package version from the manifest and shows it in
// Installed apps, so a fixed number there means every release looks like the
// same old one. Stamped from the version this installer was built with, right
// before registering.
procedure StampManifestVersion();
var
  Path, Text: string;
  Bytes: AnsiString;
begin
  Path := ExpandConstant('{app}\AppxManifest.xml');
  // LoadStringFromFile hands back the raw bytes and SaveStringToFile writes
  // them straight back out; the two conversions in between go through the
  // ANSI code page. That is lossless only while the manifest stays ASCII,
  // which is why it says so at the top and why the CI checks it.
  if LoadStringFromFile(Path, Bytes) then
  begin
    Text := Bytes;
    StringChangeEx(Text, 'Version="0.0.0.0"', 'Version="{#Version}.0"', True);
    Bytes := Text;
    SaveStringToFile(Path, Bytes, False);
  end;
end;

procedure RegisterModernMenu();
begin
  StampManifestVersion();
  RunHidden('Add-AppxPackage -Register ''' + ExpandConstant('{app}\AppxManifest.xml') +
            ''' -ExternalLocation ''' + ExpandConstant('{app}') + ''' -ErrorAction SilentlyContinue');
end;

procedure UnregisterModernMenu();
begin
  RunHidden('Get-AppxPackage ' + ModernPackage + ' | Remove-AppxPackage -ErrorAction SilentlyContinue');
end;

procedure AddToPath();
var
  Current: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', Current) then
    Current := '';
  if Pos(LowerCase(ExpandConstant('{app}')), LowerCase(Current)) > 0 then
    exit;
  if (Current <> '') and (Current[Length(Current)] <> ';') then
    Current := Current + ';';
  RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Current + ExpandConstant('{app}'));
end;

procedure RemoveFromPath();
var
  Current: string;
  Target: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', Current) then
    exit;
  Target := ExpandConstant('{app}');
  P := Pos(LowerCase(Target), LowerCase(Current));
  if P = 0 then
    exit;
  Delete(Current, P, Length(Target));
  StringChangeEx(Current, ';;', ';', True);
  if (Current <> '') and (Current[Length(Current)] = ';') then
    Delete(Current, Length(Current), 1);
  RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Current);
end;

// Killing the Explorer is the only way to make it release arca_shell.dll.
// Windows brings it straight back, and the context menu only picks up changes
// after this anyway.
procedure RestartExplorer();
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM explorer.exe', '', SW_HIDE,
       ewWaitUntilTerminated, Code);
  Sleep(1500);
  if not CheckForMutexes('') then
    Exec(ExpandConstant('{win}\explorer.exe'), '', '', SW_SHOW, ewNoWait, Code);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  // Before copying: on an upgrade the DLL is already loaded and locked.
  if (CurStep = ssInstall) and FileExists(ExpandConstant('{app}\arca_shell.dll')) then
    RestartExplorer();

  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('shellmenu') then
      RegisterModernMenu();
    if WizardIsTaskSelected('addtopath') then
      AddToPath();
    // No manual SHChangeNotify here. ChangesAssociations=yes already makes
    // Setup notify the shell, and an earlier attempt to call it through
    // PowerShell left an unbalanced quote in -Command: powershell then waits
    // on stdin forever and the installer hangs after copying the files.
  end;
end;

// Whatever the user picked in Settings > Default apps points at one of our
// ProgIDs, and the uninstall is about to delete those. Leaving UserChoice
// behind pins the extension to a handler that no longer exists: the file then
// opens with nothing and Settings shows a broken entry that is awkward to undo.
//
// Writing a UserChoice is blocked by Windows, on purpose, because it is how an
// application would steal a default. Deleting one is allowed, and it puts the
// extension back to whatever Windows decides on its own.
procedure ForgetDefaults();
var
  Exts: array[0..3] of string;
  Key, ProgId: string;
  I: Integer;
begin
  Exts[0] := '.zip';
  Exts[1] := '.tar';
  Exts[2] := '.gz';
  Exts[3] := '.tgz';
  for I := 0 to 3 do
  begin
    Key := 'Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\' + Exts[I] + '\UserChoice';
    if RegQueryStringValue(HKCU, Key, 'ProgId', ProgId) then
      // Only ours. Somebody else's default is none of our business.
      if Copy(ProgId, 1, 5) = 'Arca.' then
        RegDeleteKeyIncludingSubkeys(HKCU, Key);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    UnregisterModernMenu();
    ForgetDefaults();
    RemoveFromPath();
    // Runs before the files are deleted, so the Explorer lets go of the DLL
    // and the uninstall does not leave it behind.
    RestartExplorer();
  end;
end;

[UninstallDelete]
; Registering the sparse MSIX package leaves this hidden folder behind, and
; Inno knows nothing about it, so the install directory would survive an
; uninstall with it inside.
Type: filesandordirs; Name: "{app}\microsoft.system.package.metadata"
Type: dirifempty; Name: "{app}"

[Run]
Filename: "{app}\arca-gui.exe"; Description: "{cm:LaunchProgram,Arca}"; Flags: nowait postinstall skipifsilent
