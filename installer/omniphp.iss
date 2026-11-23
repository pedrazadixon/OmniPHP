; Inno Setup script for OmniPHP.
;
; Build the app first:   cd omniphp && cargo build --release
; Then compile this:     ISCC.exe installer\omniphp.iss
;
; The installer is complete by default: OmniPHP plus the runtimes found in
; ..\bin (Apache, every PHP version, MySQL, Mailpit, Redis, cloudflared,
; Composer, Adminer, Notepad++); fetch-runtimes.bat downloads them. For a small
; installer with OmniPHP alone, compile with:
;                        ISCC.exe /DBundleRuntimes=0 installer\omniphp.iss

#define AppName "OmniPHP"
#define AppVersion "1.0.0-beta.1"
#ifndef BundleRuntimes
  #define BundleRuntimes "1"
#endif
; Where the runtimes are taken from. A release is built from a clean download
; of runtimes.json rather than a bin\ that has been worked in:
;   fetch-runtimes.bat -Root C:\clean
;   ISCC.exe /DBinDir=C:\clean\bin installer\omniphp.iss
#ifndef BinDir
  #define BinDir "..\bin"
#endif

[Setup]
AppId={{6B0E7A52-3C1D-4F8E-9A47-5D2E8F1C0B93}
AppName={#AppName}
AppVersion={#AppVersion}
; PHP's FastCGI launcher cannot handle spaces in its path, and the app writes
; its configuration next to itself, so "Program Files" is not an option.
DefaultDirName=C:\OmniPHP
DisableProgramGroupPage=yes
; Installs for the current user; no administrator rights needed.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
OutputDir=..\dist
#if BundleRuntimes == "1"
OutputBaseFilename=OmniPHP-Setup-{#AppVersion}
#else
OutputBaseFilename=OmniPHP-Setup-{#AppVersion}-light
#endif
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\omniphp\assets\icon.ico
UninstallDisplayIcon={app}\OmniPHP.exe
UninstallDisplayName={#AppName}
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"

[Dirs]
; The app finds its root by looking for bin\ and www\ next to the executable.
Name: "{app}\bin"
Name: "{app}\www"
Name: "{app}\data"

[Files]
Source: "..\omniphp\target\release\omniphp.exe"; DestDir: "{app}"; DestName: "OmniPHP.exe"; Flags: ignoreversion
; Apache's FastCGI module is shipped even without the runtimes: it is small,
; and no site can run PHP without it.
Source: "{#BinDir}\mod_fcgid\*"; DestDir: "{app}\bin\mod_fcgid"; Flags: recursesubdirs createallsubdirs ignoreversion
#if BundleRuntimes == "1"
Source: "{#BinDir}\apache\*"; DestDir: "{app}\bin\apache"; Excludes: "logs\*"; Flags: recursesubdirs createallsubdirs ignoreversion
; A php.ini left in a PHP folder is not OmniPHP's (it writes one per site)
; and may point at another machine's paths, so it is not shipped.
Source: "{#BinDir}\php\*"; DestDir: "{app}\bin\php"; Excludes: "php.ini"; Flags: recursesubdirs createallsubdirs ignoreversion
Source: "{#BinDir}\mysql\*"; DestDir: "{app}\bin\mysql"; Excludes: "my.ini"; Flags: recursesubdirs createallsubdirs ignoreversion
Source: "{#BinDir}\mailpit\*"; DestDir: "{app}\bin\mailpit"; Excludes: "*.log,*.db"; Flags: recursesubdirs createallsubdirs ignoreversion
Source: "{#BinDir}\redis\*"; DestDir: "{app}\bin\redis"; Excludes: "*.log,*.rdb"; Flags: recursesubdirs createallsubdirs ignoreversion
Source: "{#BinDir}\cloudflared\cloudflared-windows-amd64.exe"; DestDir: "{app}\bin\cloudflared"; Flags: ignoreversion
Source: "{#BinDir}\composer\*"; DestDir: "{app}\bin\composer"; Flags: recursesubdirs createallsubdirs ignoreversion
Source: "{#BinDir}\adminer\*"; DestDir: "{app}\bin\adminer"; Flags: recursesubdirs createallsubdirs ignoreversion
; The editor behind the Hosts button. Its own settings and session are left out.
Source: "{#BinDir}\notepad++\*"; DestDir: "{app}\bin\notepad++"; Excludes: "config.xml,session.xml,\backup\*"; Flags: recursesubdirs createallsubdirs ignoreversion
#endif

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\OmniPHP.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\OmniPHP.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\OmniPHP.exe"; Description: "Start {#AppName}"; Flags: nowait postinstall skipifsilent

[InstallDelete]
; Where mod_fcgid lived before it moved to bin\mod_fcgid.
Type: filesandordirs; Name: "{app}\etc\apache\modules"

[UninstallRun]
; Undoes what OmniPHP changed in Windows itself: the trusted certificate
; authority, the site addresses in the hosts file and the startup entry.
; Windows asks for administrator permission only if there is something to
; remove; declining leaves it in place and the uninstall goes on.
Filename: "{app}\OmniPHP.exe"; Parameters: "cleanup"; RunOnceId: "RemoveWindowsChanges"

[UninstallDelete]
; Generated configuration and logs. Sites (www) and databases (data) are kept.
Type: filesandordirs; Name: "{app}\logs"
Type: filesandordirs; Name: "{app}\tmp"

[Code]
function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpSelectDir) and (Pos(' ', WizardDirValue) > 0) then
  begin
    MsgBox('Please choose a folder without spaces in its path. PHP''s FastCGI launcher cannot run from one.', mbError, MB_OK);
    Result := False;
  end;
end;
