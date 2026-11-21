//! Runtimes de GPU de um processo, **sob demanda** (tecla na aba GPU): os módulos carregados
//! pelo snapshot do Toolhelp (`TH32CS_SNAPMODULE`), que abre o processo com leitura de memória.
//!
//! Por isso nunca roda num processo com o hook do RTSS (apps 3D, possivelmente o jogo com
//! anticheat): quem chama passa a lista e o pedido é recusado antes de abrir qualquer coisa.
//! Acesso negado (processo protegido) vira o estado neutro "protected".

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PARTIAL_COPY};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};

use crate::model::{Metric, fail, neutral};
use crate::win::{from_wide, own};

/// Módulo (minúsculo) → runtime exibido.
const RUNTIMES: &[(&str, &str)] = &[
    ("nvcuda.dll", "CUDA"),
    ("cudart64_", "CUDA"),
    ("vulkan-1.dll", "Vulkan"),
    ("d3d12.dll", "D3D12"),
    ("d3d12core.dll", "D3D12"),
    ("d3d11.dll", "D3D11"),
    ("d3d10.dll", "D3D10"),
    ("d3d9.dll", "D3D9"),
    ("opengl32.dll", "OpenGL"),
    ("onnxruntime.dll", "ONNX"),
    ("directml.dll", "DirectML"),
    ("nvencodeapi64.dll", "NVENC"),
    ("nvcuvid.dll", "NVDEC"),
    ("opencl.dll", "OpenCL"),
];

/// Runtimes reconhecidos numa lista de módulos, sem repetição, na ordem de `RUNTIMES`.
pub fn classify<'a>(modules: impl Iterator<Item = &'a str>) -> Vec<&'static str> {
    let lower: Vec<String> = modules.map(str::to_ascii_lowercase).collect();
    let mut out: Vec<&'static str> = Vec::new();
    for (prefix, name) in RUNTIMES {
        let hit = lower.iter().any(|m| {
            if prefix.ends_with('_') {
                m.starts_with(prefix)
            } else {
                m == prefix
            }
        });
        if hit && !out.contains(name) {
            out.push(name);
        }
    }
    out
}

/// Runtimes do processo `pid`, que a interface mostrava com o nome `name`. `hooked`: PIDs com
/// o hook do RTSS na última amostra da thread do RTSS.
///
/// Antes de abrir: (1) `hooked`; (2) a Shared Memory do RTSS relida agora (um jogo que abriu
/// depois da última amostra); (3) o PID ainda é o mesmo processo (PID reaproveitado por um
/// jogo novo). Qualquer dúvida = não abre. Fica o caso de um jogo que o RTSS não engancha
/// (perfil excluído): por isso a leitura é só sob demanda, nunca automática.
pub fn runtimes(pid: u32, name: &str, hooked: &[u32]) -> Metric<Vec<&'static str>> {
    const SKIP: &str = "skipped: 3D app hooked by RTSS (anticheat safety)";
    if hooked.contains(&pid) {
        return neutral(SKIP);
    }
    if pid <= 4 {
        return neutral("protected");
    }
    match super::rtss::hooked_now() {
        Ok(now) if now.contains(&pid) => return neutral(SKIP),
        Ok(_) => {}
        Err(e) => return neutral(format!("skipped: {e} (anticheat safety)")),
    }
    match super::procs::name_of(pid) {
        Ok(Some(n)) if n == name => {}
        Ok(_) => return neutral("process changed: select it again"),
        Err(e) => return fail(e),
    }
    // ERROR_PARTIAL_COPY: o processo ainda está carregando módulos; uma nova tentativa basta.
    let mut last = String::new();
    for _ in 0..2 {
        // SAFETY: sem pré-condições; o handle devolvido passa a ter dono logo abaixo.
        match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) } {
            Ok(h) => {
                // SAFETY: snapshot válido e sem outro dono.
                let snap = unsafe { own(h) };
                let h = windows::Win32::Foundation::HANDLE(
                    std::os::windows::io::AsRawHandle::as_raw_handle(&snap),
                );
                let mut e = MODULEENTRY32W {
                    dwSize: size_of::<MODULEENTRY32W>() as u32,
                    ..Default::default()
                };
                let mut names = Vec::new();
                // SAFETY: estrutura local com dwSize preenchido.
                let mut ok = unsafe { Module32FirstW(h, &raw mut e) }.is_ok();
                while ok {
                    names.push(from_wide(&e.szModule));
                    // SAFETY: idem.
                    ok = unsafe { Module32NextW(h, &raw mut e) }.is_ok();
                }
                return Ok(classify(names.iter().map(String::as_str)));
            }
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                return neutral("protected");
            }
            Err(e) if e.code() == ERROR_PARTIAL_COPY.to_hresult() => last = e.to_string(),
            Err(e) => return fail(format!("module list: {e}")),
        }
    }
    fail(format!("module list: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconhece_runtimes() {
        let m = [
            "game.exe",
            "D3D11.dll",
            "nvcuda.dll",
            "cudart64_12.dll",
            "vulkan-1.dll",
        ];
        assert_eq!(classify(m.into_iter()), vec!["CUDA", "Vulkan", "D3D11"]);
        assert!(classify(["kernel32.dll"].into_iter()).is_empty());
    }

    #[test]
    fn processo_com_hook_nem_e_aberto() {
        assert_eq!(
            runtimes(1234, "game.exe", &[1234]),
            neutral("skipped: 3D app hooked by RTSS (anticheat safety)")
        );
        assert_eq!(runtimes(4, "System", &[]), neutral("protected"));
    }

    fn my_name() -> String {
        super::super::procs::name_of(std::process::id())
            .expect("lista")
            .expect("o próprio processo")
    }

    /// Com o RTSS aberto: o próprio processo (sem hook) é lido; com outro nome, recusado.
    #[test]
    #[ignore = "hardware: precisa do RTSS aberto"]
    fn o_proprio_processo() {
        let r = runtimes(std::process::id(), &my_name(), &[]).expect("módulos do próprio processo");
        // O executável de teste não carrega runtime de GPU.
        assert!(!r.contains(&"CUDA"), "{r:?}");
        assert_eq!(
            runtimes(std::process::id(), "outro.exe", &[]),
            neutral("process changed: select it again")
        );
    }
}
