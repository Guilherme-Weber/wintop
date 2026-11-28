; Instalador do wintop (Inno Setup 6). Gerado por scripts\package.ps1 a partir da pasta do pacote:
;   ISCC /DAppVersion=1.4.6 /DPayload=<dist\wintop-1.4.6-windows-x64> installer\wintop.iss
;
; O assistente não copia nada para Program Files por conta própria: extrai o exe e os módulos
; numa pasta temporária e roda `wintop.exe --install`, o mesmo caminho endurecido de sempre
; (pasta com dono Administradores e ACL protegida, tarefa agendada sem UAC, atalho no Menu
; Iniciar e a entrada em "Aplicativos instalados", cuja desinstalação é o `--uninstall`).
; Também instala o PawnIO (download do release oficial, SHA-256 conferido) e oferece o RTSS.

#ifndef AppVersion
  #error Defina AppVersion (/DAppVersion=x.y.z)
#endif
#ifndef Payload
  #error Defina Payload (/DPayload=<pasta do pacote>)
#endif

; PawnIO fixado numa versão: o hash é conferido antes de executar o instalador dele.
#define PawnIOVersion "2.2.0"
#define PawnIOUrl "https://github.com/namazso/PawnIO.Setup/releases/download/" + PawnIOVersion + "/PawnIO_setup.exe"
#define PawnIOSha256 "1f519a22e47187f70a1379a48ca604981c4fcf694f4e65b734aaa74a9fba3032"
#define RtssUrl "https://www.guru3d.com/download/rtss-rivatuner-statistics-server-download/"

[Setup]
AppId={{A23D9621-3052-4E2C-893E-0FB5326E0F0B}
AppName=wintop
AppVersion={#AppVersion}
AppVerName=wintop {#AppVersion}
AppPublisher=Guilherme da Luz Weber
AppPublisherURL=https://github.com/Guilherme-Weber/wintop
AppSupportURL=https://github.com/Guilherme-Weber/wintop/issues
VersionInfoVersion={#AppVersion}
VersionInfoDescription=wintop setup
; A pasta é a fixa do wintop (C:\Program Files\wintop), criada pelo --install.
CreateAppDir=no
DisableProgramGroupPage=yes
; A desinstalação é a do próprio wintop (entrada gravada pelo --install).
Uninstallable=no
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=wintop-{#AppVersion}-setup
SetupIconFile=..\assets\wintop.ico
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
CloseApplications=no
ShowLanguageDialog=auto

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ptbr"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[CustomMessages]
en.TaskPawnIO=Install PawnIO {#PawnIOVersion} (CPU temperature and power; official release, downloaded now)
ptbr.TaskPawnIO=Instalar o PawnIO {#PawnIOVersion} (temperatura e potência da CPU; release oficial, baixado agora)
en.TaskRtss=Open the RivaTuner Statistics Server download page (identifies the game: API, resolution, GPU time)
ptbr.TaskRtss=Abrir a página de download do RivaTuner Statistics Server (identifica o jogo: API, resolução, tempo de GPU)
en.GroupDeps=Optional components:
ptbr.GroupDeps=Componentes opcionais:
en.Installing=Installing wintop...
ptbr.Installing=Instalando o wintop...
en.InstallingPawnIO=Installing PawnIO...
ptbr.InstallingPawnIO=Instalando o PawnIO...
en.InstallFailed=wintop could not be installed:%n%n%1
ptbr.InstallFailed=Não foi possível instalar o wintop:%n%n%1
en.PawnIOFailed=PawnIO setup ended with code %1. wintop works without it, but without CPU temperature and power; install it later with: winget install namazso.PawnIO
ptbr.PawnIOFailed=O instalador do PawnIO terminou com o código %1. O wintop funciona sem ele, mas sem temperatura e potência da CPU; instale depois com: winget install namazso.PawnIO
en.RunWintop=Open wintop
ptbr.RunWintop=Abrir o wintop

[Tasks]
Name: "pawnio"; Description: "{cm:TaskPawnIO}"; GroupDescription: "{cm:GroupDeps}"; Check: not PawnIOInstalled
Name: "rtss"; Description: "{cm:TaskRtss}"; GroupDescription: "{cm:GroupDeps}"; Flags: unchecked; Check: not RtssInstalled

[Files]
Source: "{#Payload}\wintop.exe"; DestDir: "{tmp}\wintop"; Flags: ignoreversion deleteafterinstall
Source: "{#Payload}\modules\*"; DestDir: "{tmp}\wintop\modules"; Flags: ignoreversion recursesubdirs deleteafterinstall

[Run]
Filename: "{#RtssUrl}"; Flags: shellexec nowait runasoriginaluser; Tasks: rtss
; Pela tarefa agendada, como o atalho do Menu Iniciar: a caixa da tela final roda como o usuário
; original (sem elevação) e o exe direto falharia com o erro 740 (requireAdministrator).
Filename: "{sys}\schtasks.exe"; Parameters: "/run /tn ""wintop"""; Description: "{cm:RunWintop}"; Flags: postinstall nowait skipifsilent runhidden; Check: WintopInstalled

[Code]
var
  DownloadPage: TDownloadWizardPage;
  InstallOk: Boolean;

function PawnIOInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM64, 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO');
end;

function RtssInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM32, 'SOFTWARE\Unwinder\RTSS');
end;

function WintopInstalled: Boolean;
begin
  Result := InstallOk and FileExists(ExpandConstant('{commonpf64}\wintop\wintop.exe'));
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing), SetupMessage(msgPreparingDesc), nil);
end;

{ O PawnIO é baixado antes da instalação (com o hash conferido); falha aqui volta ao assistente. }
function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpReady) and WizardIsTaskSelected('pawnio') then
  begin
    DownloadPage.Clear;
    DownloadPage.Add('{#PawnIOUrl}', 'PawnIO_setup.exe', '{#PawnIOSha256}');
    DownloadPage.Show;
    try
      try
        DownloadPage.Download;
      except
        if DownloadPage.AbortedByUser then
          Log('PawnIO: download cancelado')
        else
          SuppressibleMsgBox(AddPeriod(GetExceptionMessage), mbCriticalError, MB_OK, IDOK);
        Result := False;
      end;
    finally
      DownloadPage.Hide;
    end;
  end;
end;

{ Saída do --install (stdout e stderr), para mostrar a causa se falhar. }
function ReadLog(const Path: String): String;
var
  Lines: TArrayOfString;
  I: Integer;
begin
  Result := '';
  if LoadStringsFromFile(Path, Lines) then
    for I := 0 to GetArrayLength(Lines) - 1 do
      Result := Result + Lines[I] + #13#10;
end;

procedure RunInstall;
var
  Exe, LogFile: String;
  Code: Integer;
begin
  WizardForm.StatusLabel.Caption := CustomMessage('Installing');
  Exe := ExpandConstant('{tmp}\wintop\wintop.exe');
  LogFile := ExpandConstant('{tmp}\wintop-install.log');
  if not Exec(ExpandConstant('{cmd}'), '/d /c ""' + Exe + '" --install > "' + LogFile + '" 2>&1"',
    ExpandConstant('{tmp}\wintop'), SW_HIDE, ewWaitUntilTerminated, Code) then
    Code := -1;
  Log('wintop --install: ' + IntToStr(Code) + #13#10 + ReadLog(LogFile));
  InstallOk := Code = 0;
  if not InstallOk then
    SuppressibleMsgBox(FmtMessage(CustomMessage('InstallFailed'), [ReadLog(LogFile)]),
      mbCriticalError, MB_OK, IDOK);
end;

procedure RunPawnIO;
var
  Code: Integer;
begin
  WizardForm.StatusLabel.Caption := CustomMessage('InstallingPawnIO');
  if not Exec(ExpandConstant('{tmp}\PawnIO_setup.exe'), '-install -silent', '', SW_SHOW,
    ewWaitUntilTerminated, Code) then
    Code := -1;
  Log('PawnIO setup: ' + IntToStr(Code));
  if (Code <> 0) and not PawnIOInstalled then
    SuppressibleMsgBox(FmtMessage(CustomMessage('PawnIOFailed'), [IntToStr(Code)]),
      mbError, MB_OK, IDOK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    { PawnIO antes: o wintop aberto no fim já encontra o driver. }
    if WizardIsTaskSelected('pawnio') then
      RunPawnIO;
    RunInstall;
  end;
end;
