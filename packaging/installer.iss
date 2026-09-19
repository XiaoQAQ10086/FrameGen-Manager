; FrameGen Manager - Inno Setup 6 打包脚本
; 编译： iscc packaging\installer.iss
;
; 设计要点：
;  1. 不把那些大文件（version.dll、nvngx_dlssg.dll、nvngx_dlss.dll）打进安装包，
;     它们是运行时按需下载的。这是能把安装包压到 10 MB 以下的关键。
;     这是能把安装包压到 10 MB 以下的关键。
;  2. 默认按用户安装到 %LOCALAPPDATA%\Programs，不需要管理员权限。
;  3. 卸载只删程序自己的文件，绝不碰游戏目录（游戏目录里的文件由 App 内的「还原」处理）。
;  4. 不使用 UPX：加壳会显著提高杀软误报率，而这个工具本身就在做 DLL 代理，对误报很敏感。

#define AppName "FrameGen Manager"
; 版本号可由打包脚本传入：iscc /DAppVersion=1.2.3 installer.iss
#ifndef AppVersion
#define AppVersion "0.9.13"
#endif
#define AppExeName "framegen-manager.exe"

[Setup]
AppId={{7C4B1E2A-9D53-4A6E-8F1C-5B2D9E4A77C1}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
; 升级时用上一次装到的目录，用户直接下一步就是覆盖安装（设置/资产/游戏库都在那儿）
DefaultDirName={localappdata}\Programs\{#AppName}
UsePreviousAppDir=yes
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; 卸载时不删用户数据（设置、游戏库、已下载资产、部署备份），见 [UninstallRun]/[Code]
Uninstallable=yes
OutputDir=..\dist
OutputBaseFilename=FrameGen-Manager-v{#AppVersion}-setup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName={#AppName}
; 图标：packaging\app.ico 由 make-ico.ps1 从母版生成，同一个文件也被 build.rs 编进了 exe
SetupIconFile=app.ico
UninstallDisplayIcon={app}\{#AppExeName}

[Languages]
Name: "chinese"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\packaging\使用说明.txt"; DestDir: "{app}"; Flags: ignoreversion
; 注意：这里刻意不包含 version.dll / dlssg_sm86.ini。
; 它们在 App 内点「下载 / 更新资产」时从上游仓库获取，并做 git blob sha 校验。

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "启动 {#AppName}"; Flags: nowait postinstall

[UninstallDelete]
; 只删程序自己带的那几个文件；用户数据（framegen-manager.json、game_library.json、
; assets\、backups\、logs\）一律保留 —— 删了它们等于把用户设置和 95 MB 资产清空。
Type: files; Name: "{app}\{#AppExeName}"
Type: files; Name: "{app}\README.md"
Type: files; Name: "{app}\使用说明.txt"
Type: files; Name: "{app}\LICENSE"
Type: files; Name: "{app}\THIRD_PARTY_NOTICES.txt"

[Code]
// 卸载时提醒：程序文件被删，设置/游戏库/资产/备份留在安装目录里。
// 如果还有游戏处于「已部署」状态，应该先在 App 里还原再卸载。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ErrorCode: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if MsgBox('程序已卸载。' + #13#10 + #13#10 +
              '你的设置、游戏库、已下载的资产和部署备份仍保留在安装目录里。' + #13#10 +
              '如果还有游戏里存在本工具部署的 DLL，请重新安装后用「还原」功能清理。' + #13#10 + #13#10 +
              '是否现在打开该目录？', mbConfirmation, MB_YESNO) = IDYES then
    begin
      ShellExec('open', ExpandConstant('{app}'), '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode);
    end;
  end;
end;
