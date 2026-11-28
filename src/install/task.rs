//! Tarefa agendada (API COM `ITaskService`) que abre o wintop elevado sem UAC, e o atalho do
//! Menu Iniciar que a dispara.

use super::*;

pub(super) const TASK: &str = "wintop";

/// XML da tarefa (o `schtasks` quer UTF-16). `user`: SID de quem roda.
pub fn task_xml(exe: &Path, user: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>wintop elevated without the UAC prompt (wintop --install).</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        esc(user),
        esc(&exe.to_string_lossy())
    )
}

/// Guarda RAII para inicialização COM na thread atual (`CoUninitialize` só se esta guarda
/// realmente inicializou o apartamento; aceita `RPC_E_CHANGED_MODE` se já estava em MTA).
pub(super) struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    fn init() -> Result<Self, String> {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        // SAFETY: inicializa COM na thread atual; `Drop` desfaz só quando `uninit == true`.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            Ok(Self { uninit: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self { uninit: false })
        } else {
            Err(format!("COM: {}", windows::core::Error::from(hr)))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: pareado com o `CoInitializeEx` bem-sucedido nesta thread.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Conecta à pasta raiz (`\`) do Agendador de Tarefas via API COM nativa `ITaskService`.
pub(super) fn with_task_folder<T>(
    action: &str,
    f: impl FnOnce(&windows::Win32::System::TaskScheduler::ITaskFolder) -> windows::core::Result<T>,
) -> Result<T, String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::System::TaskScheduler::{ITaskService, TaskScheduler};
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::BSTR;

    let _com = ComGuard::init()?;
    let r = (|| -> windows::core::Result<T> {
        // SAFETY: COM inicializado; objetos COM liberados no Drop antes do `_com`.
        unsafe {
            let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let empty = VARIANT::default();
            svc.Connect(&empty, &empty, &empty, &empty)?;
            let folder = svc.GetFolder(&BSTR::from("\\"))?;
            f(&folder)
        }
    })();
    r.map_err(|e| format!("Task Scheduler ({action}): {e}"))
}

/// Registra (ou atualiza) a tarefa agendada `wintop` em memória via `ITaskService`, sem XML em
/// disco nem subprocesso `schtasks.exe`, com SDDL explícito sem `CREATOR OWNER`.
pub(super) fn register_task(exe: &Path, user: &str) -> Result<(), String> {
    use windows::Win32::System::TaskScheduler::{
        TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::BSTR;

    let xml = task_xml(exe, user);
    let sddl = VARIANT::from(BSTR::from(format!(
        "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;{user})"
    )));
    with_task_folder("register", |folder| {
        let empty = VARIANT::default();
        // SAFETY: `folder` é um `ITaskFolder` válido conectado ao serviço local.
        unsafe {
            folder.RegisterTask(
                &BSTR::from(TASK),
                &BSTR::from(xml),
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &sddl,
            )?;
        }
        Ok(())
    })
}

/// Remove a tarefa agendada `wintop` via `ITaskService`.
pub(super) fn delete_task() -> Result<(), String> {
    use windows::core::BSTR;

    with_task_folder("delete", |folder| {
        // SAFETY: `folder` é um `ITaskFolder` válido conectado ao serviço local.
        unsafe { folder.DeleteTask(&BSTR::from(TASK), 0) }
    })
}

/// Dispara a tarefa: abre a cópia instalada elevada (sem UAC) via `ITaskService`.
pub fn run_task() -> Result<(), String> {
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::BSTR;

    with_task_folder("run", |folder| {
        // SAFETY: `folder` e `task` são interfaces COM válidas conectadas ao serviço local.
        unsafe {
            let task = folder.GetTask(&BSTR::from(TASK))?;
            task.Run(&VARIANT::default())?;
        }
        Ok(())
    })
}

pub(super) fn create_shortcut(lnk: &Path, exe: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;
    use windows::core::Interface;

    let schtasks = crate::win::system_dir()
        .ok_or("System32 not found")?
        .join("schtasks.exe");
    let w = |s: &str| wide(s);
    let _com = ComGuard::init()?;
    let r = (|| -> windows::core::Result<()> {
        // SAFETY: COM inicializado; strings UTF-16 vivas durante cada chamada; objetos liberados
        // no Drop antes do `_com`.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let target = w(&schtasks.to_string_lossy());
            link.SetPath(PCWSTR(target.as_ptr()))?;
            let args = w(&format!("/run /tn \"{TASK}\""));
            link.SetArguments(PCWSTR(args.as_ptr()))?;
            let icon = w(&exe.to_string_lossy());
            link.SetIconLocation(PCWSTR(icon.as_ptr()), 0)?;
            let desc = w("wintop (elevated, no UAC prompt)");
            link.SetDescription(PCWSTR(desc.as_ptr()))?;
            // O schtasks só dispara a tarefa: janela minimizada (sem piscar).
            link.SetShowCmd(SW_SHOWMINNOACTIVE)?;
            let file: IPersistFile = link.cast()?;
            let p = w(&lnk.to_string_lossy());
            file.Save(PCWSTR(p.as_ptr()), true)
        }
    })();
    r.map_err(|e| format!("shortcut {}: {e}", lnk.display()))
}
