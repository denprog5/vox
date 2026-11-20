; Установщик Vox (Inno Setup 6). Собирается скриптом scripts\build-installer.ps1.
; Ставится в профиль пользователя без прав администратора.
;   полный:      модели звуков и слов внутри, Vox-Setup-<версия>.exe
;   обновление:  /DUpdateOnly — без моделей, ставится только поверх установленной программы
;                (его скачивает встроенное автообновление), Vox-Update-<версия>.exe

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

#define AppName "Vox"
#define AppExe "vox.exe"
; Программа раньше называлась «Мурлыка»: её установщик удаляем, настройки переезжают.
#define OldExe "murlyka.exe"
#define OldIdGuid "{B6E1D4A2-37C9-4F58-8D21-9A4E6C3F7B05}"
#define AppIdGuid "{5C0F9E7A-2B4D-4A61-9E3C-7D18B6F2A4C9}"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
; AppId не менять: по нему новая версия находит установленную.
AppId={{#AppIdGuid}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=denprog5
AppPublisherURL=https://github.com/denprog5/vox
AppSupportURL=https://github.com/denprog5/vox/issues
AppUpdatesURL=https://github.com/denprog5/vox/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Vox
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
#ifdef UpdateOnly
OutputBaseFilename=Vox-Update-{#AppVersion}
#else
OutputBaseFilename=Vox-Setup-{#AppVersion}
#endif
SetupIconFile=..\assets\icon.ico
WizardSmallImageFile=..\assets\wizard-small.png,..\assets\wizard-small-2x.png
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/ultra64
SolidCompression=yes
LZMAUseSeparateProcess=yes
WizardStyle=modern
; Приложение висит в трее: перед заменой файлов его нужно закрыть.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "autostart"; Description: "Запускать вместе с Windows"; GroupDescription: "Дополнительно:"
Name: "desktopicon"; Description: "Ярлык на рабочем столе"; GroupDescription: "Дополнительно:"; Flags: unchecked

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
#ifndef UpdateOnly
Source: "{#BinDir}\models\ced-small\model.int8.onnx"; DestDir: "{app}\models\ced-small"; Flags: ignoreversion
Source: "{#BinDir}\models\ced-small\class_labels_indices.csv"; DestDir: "{app}\models\ced-small"; Flags: ignoreversion
Source: "{#BinDir}\models\zipformer-ru\encoder.int8.onnx"; DestDir: "{app}\models\zipformer-ru"; Flags: ignoreversion
Source: "{#BinDir}\models\zipformer-ru\decoder.onnx"; DestDir: "{app}\models\zipformer-ru"; Flags: ignoreversion
Source: "{#BinDir}\models\zipformer-ru\joiner.int8.onnx"; DestDir: "{app}\models\zipformer-ru"; Flags: ignoreversion
Source: "{#BinDir}\models\zipformer-ru\tokens.txt"; DestDir: "{app}\models\zipformer-ru"; Flags: ignoreversion
#endif

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Дальше автозапуск переключается в трее или на странице настроек. Тихое обновление его
; не трогает, чтобы не вернуть то, что пользователь выключил.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "Vox"; ValueData: """{app}\{#AppExe}"""; Tasks: autostart; Check: not WizardSilent

[Run]
Filename: "{app}\{#AppExe}"; Description: "Запустить {#AppName}"; Flags: nowait postinstall skipifsilent
; Автообновление запускает установщик с /RELAUNCH=1 — после него программа стартует снова.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: IsRelaunch

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"

[Code]
var
  OldAutostart: Boolean;

function IsRelaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:relaunch|0}') = '1');
end;

#ifdef UpdateOnly
// Облегчённый установщик без моделей: только поверх уже установленной программы.
function InitializeSetup: Boolean;
var
  Dir: String;
begin
  Result := RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppIdGuid}_is1',
    'InstallLocation', Dir) and FileExists(AddBackslash(Dir) + 'models\zipformer-ru\encoder.int8.onnx')
    and FileExists(AddBackslash(Dir) + 'models\ced-small\model.int8.onnx');
  if not Result then
    SuppressibleMsgBox('Это обновление для уже установленного Vox.' + #13#10 +
      'Для первой установки скачайте Vox-Setup со страницы github.com/denprog5/vox/releases',
      mbError, MB_OK, IDOK);
end;
#endif

// Закрыть работающий экземпляр до копирования файлов (Restart Manager не всегда закрывает
// трей-приложения). Стоит старая «Мурлыка» — тихо удаляем её: настройки, записи голоса и счёт
// остаются в %APPDATA%\Murlyka, Vox при первом запуске забирает их себе.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  Uninst: String;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#OldExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  OldAutostart := RegValueExists(HKCU, '{#RunKey}', 'Murlyka');
  if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#OldIdGuid}_is1',
      'UninstallString', Uninst) then
    Exec(RemoveQuotes(Uninst), '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

// У «Мурлыки» был автозапуск — оставляем его и у Vox.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and OldAutostart then
    RegWriteStringValue(HKCU, '{#RunKey}', 'Vox', '"' + ExpandConstant('{app}\{#AppExe}') + '"');
end;

// Настройки и записанные примеры голоса лежат в %APPDATA%\Vox — спрашиваем, удалять ли их.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Dir: String;
begin
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKCU, '{#RunKey}', 'Vox');
  if CurUninstallStep = usPostUninstall then
  begin
    Dir := ExpandConstant('{userappdata}\Vox');
    if DirExists(Dir) and (not UninstallSilent) and (MsgBox('Удалить также настройки и записанные примеры голоса?' + #13#10 + Dir,
        mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
      DelTree(Dir, True, True, True);
  end;
end;
