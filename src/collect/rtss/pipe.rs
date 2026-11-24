//! Servidor overlapped do named pipe \\.\pipe\RTSS_Frametime (8 instâncias com DACL restrita).

use std::collections::{HashMap, HashSet};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::Instant;

use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    PIPE_ACCESS_DUPLEX, ReadFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};
use windows::core::PCWSTR;

use super::Ring;
use super::shm::MAX_APPS;
use crate::win::{own, wide};

/// Registros completos de 8 bytes; a sobra de uma mensagem partida (`ERROR_MORE_DATA`) fica em
/// `carry`; sobra numa mensagem completa (malformada) é descartada.
///
/// `known`: índices de apps (fora dos ignorados) na última leitura da Shared Memory. Um índice
/// ainda desconhecido (jogo que acabou de abrir, antes da próxima amostra) entra com teto
/// pequeno, em no máximo `MAX_UNKNOWN_RINGS` filas: um cliente local inundando o pipe com
/// índices inventados não faz a memória crescer.
pub(super) fn feed(
    rings: &mut HashMap<u32, Ring>,
    known: &HashSet<u32>,
    ignored: &HashSet<u32>,
    carry: &mut Vec<u8>,
    data: &[u8],
    complete: bool,
) {
    carry.extend_from_slice(data);
    let whole = carry.len() / 8 * 8;
    let now = Instant::now();
    let mut unknown = rings.keys().filter(|i| !known.contains(i)).count();
    for rec in carry[..whole].as_chunks::<8>().0 {
        let app = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
        let us = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        // Índice fora da Shared Memory ou frametime 0 (sem delta): registro ignorado.
        if (app as usize) >= MAX_APPS || us == 0 || ignored.contains(&app) {
            continue;
        }
        if known.contains(&app) {
            rings.entry(app).or_default().push(now, us);
        } else if let Some(r) = rings.get_mut(&app) {
            r.push_capped(now, us, UNKNOWN_FRAMES);
        } else if unknown < MAX_UNKNOWN_RINGS {
            unknown += 1;
            rings
                .entry(app)
                .or_default()
                .push_capped(now, us, UNKNOWN_FRAMES);
        }
    }
    carry.drain(..whole);
    if complete {
        carry.clear();
    }
}

// --- Pipe ---

pub(super) const INSTANCES: usize = 8;
/// Buffer de leitura por instância: 512 registros de 8 bytes por leitura (mensagens maiores
/// chegam em partes, com ERROR_MORE_DATA).
const BUF: usize = 4 * 1024;
/// Frames guardados por app: 1000 FPS × 60 s.
pub(super) const MAX_FRAMES: usize = 60_000;
/// Conclusões de leitura atendidas por período: acima disso o pipe espera o próximo período
/// (um cliente inundando não prende a thread num laço).
pub(super) const MAX_SIGNALS_PER_PUMP: u32 = 1024;
/// Índices ainda fora da Shared Memory: quantos e quantos frames cada (4 s a 1000 FPS).
pub(super) const MAX_UNKNOWN_RINGS: usize = 8;
/// Apps com a janela completa de frames (os 3D mais recentes da Shared Memory).
pub(super) const MAX_KNOWN: usize = 8;
pub(super) const UNKNOWN_FRAMES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pending {
    None,
    Connect,
    /// Cliente conectou antes do `ConnectNamedPipe`: evento sinalizado à mão.
    Connected,
    /// Rearmar a conexão no próximo ciclo (ex.: `ERROR_NO_DATA`), sem recursão.
    Rearm,
    Read,
}

/// Uma instância do pipe. `ov` e `buf` em Box: o endereço não muda com I/O pendente. Toda
/// conclusão passa pelo evento e é tratada no laço (`on_signal`), uma por vez.
pub(super) struct Instance {
    pipe: OwnedHandle,
    event: OwnedHandle,
    ov: Box<OVERLAPPED>,
    buf: Box<[u8]>,
    carry: Vec<u8>,
    pub(super) pending: Pending,
    pub(super) connected: bool,
}

fn h(o: &OwnedHandle) -> HANDLE {
    HANDLE(o.as_raw_handle())
}

impl Instance {
    pub(super) fn event_handle(&self) -> HANDLE {
        h(&self.event)
    }

    fn create(name: &[u16], first: bool, sa: &SECURITY_ATTRIBUTES) -> Result<Self, String> {
        let mut mode: FILE_FLAGS_AND_ATTRIBUTES = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE; // falha se outro servidor já existe
        }
        // SAFETY: atributos válidos durante a chamada.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                mode,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                INSTANCES as u32, // limite fixo: ninguém cria instâncias além das nossas
                BUF as u32,
                BUF as u32,
                0,
                Some(sa),
            )
        };
        if pipe.is_invalid() {
            return Err(format!(
                "CreateNamedPipe: {}",
                windows::core::Error::from_thread()
            ));
        }
        // SAFETY: handle válido recém-criado, com um único dono.
        let pipe = unsafe { own(pipe) };
        // SAFETY: evento manual sem nome.
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEvent: {e}"))?;
        // SAFETY: handle válido recém-criado, com um único dono.
        let event = unsafe { own(event) };
        let ov = Box::new(OVERLAPPED {
            hEvent: h(&event),
            ..Default::default()
        });
        Ok(Self {
            pipe,
            event,
            ov,
            buf: vec![0u8; BUF].into_boxed_slice(),
            carry: Vec::new(),
            pending: Pending::None,
            connected: false,
        })
    }

    fn fresh_ov(&mut self) {
        *self.ov = OVERLAPPED {
            hEvent: h(&self.event),
            ..Default::default()
        };
    }

    fn signal(&self) {
        // SAFETY: evento válido.
        unsafe {
            let _ = SetEvent(h(&self.event));
        }
    }

    pub(super) fn start_connect(&mut self) {
        self.connected = false;
        self.carry.clear();
        self.fresh_ov();
        // SAFETY: `ov` (em Box) vive até a conclusão ou o cancelamento.
        match unsafe { ConnectNamedPipe(h(&self.pipe), Some(&raw mut *self.ov)) } {
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => self.pending = Pending::Connect,
            Ok(()) => {
                self.pending = Pending::Connected;
                self.signal();
            }
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                self.pending = Pending::Connected;
                self.signal();
            }
            Err(e) if e.code() == ERROR_NO_DATA.to_hresult() => {
                // O cliente conectou e fechou antes: desconecta e rearma no próximo ciclo.
                // SAFETY: pipe válido; sem I/O pendente.
                unsafe {
                    let _ = DisconnectNamedPipe(h(&self.pipe));
                }
                self.pending = Pending::Rearm;
                self.signal();
            }
            Err(_) => self.pending = Pending::None, // instância desativada
        }
    }

    fn reconnect(&mut self) {
        // SAFETY: pipe válido; sem I/O pendente aqui.
        unsafe {
            let _ = DisconnectNamedPipe(h(&self.pipe));
        }
        self.start_connect();
    }

    fn start_read(&mut self) {
        self.fresh_ov();
        // SAFETY: `buf` e `ov` (em Box) vivem até a conclusão ou o cancelamento.
        let r = unsafe {
            ReadFile(
                h(&self.pipe),
                Some(&mut self.buf),
                None,
                Some(&raw mut *self.ov),
            )
        };
        match r {
            Ok(()) => self.pending = Pending::Read,
            Err(e)
                if e.code() == ERROR_IO_PENDING.to_hresult()
                    || e.code() == ERROR_MORE_DATA.to_hresult() =>
            {
                self.pending = Pending::Read;
            }
            Err(_) => {
                self.pending = Pending::None;
                self.reconnect();
            }
        }
    }

    /// O evento sinalizou: trata uma conclusão e arma a próxima operação.
    pub(super) fn on_signal(
        &mut self,
        rings: &mut HashMap<u32, Ring>,
        known: &HashSet<u32>,
        ignored: &HashSet<u32>,
    ) {
        // SAFETY: evento válido; a próxima operação (ou `signal`) volta a sinalizá-lo.
        unsafe {
            let _ = ResetEvent(h(&self.event));
        }
        let state = std::mem::replace(&mut self.pending, Pending::None);
        let mut n = 0u32;
        match state {
            Pending::None => {}
            Pending::Rearm => self.start_connect(),
            Pending::Connected => {
                self.connected = true;
                self.start_read();
            }
            Pending::Connect => {
                // SAFETY: conexão concluída sobre `ov`.
                let r = unsafe {
                    GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, false)
                };
                if r.is_ok() {
                    self.connected = true;
                    self.start_read();
                } else {
                    self.reconnect();
                }
            }
            Pending::Read => {
                // SAFETY: leitura concluída sobre `ov` (o evento sinalizou).
                let r = unsafe {
                    GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, false)
                };
                let got = (n as usize).min(self.buf.len());
                match r {
                    Ok(()) => {
                        feed(
                            rings,
                            known,
                            ignored,
                            &mut self.carry,
                            &self.buf[..got],
                            true,
                        );
                        self.start_read();
                    }
                    Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                        feed(
                            rings,
                            known,
                            ignored,
                            &mut self.carry,
                            &self.buf[..got],
                            false,
                        );
                        self.start_read();
                    }
                    Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {}
                    Err(_) => self.reconnect(),
                }
            }
        }
    }

    /// Cancela o I/O pendente e espera a conclusão (nunca solta `ov`/`buf` com I/O no kernel).
    fn cancel(&mut self) {
        if !matches!(self.pending, Pending::Connect | Pending::Read) {
            return;
        }
        let mut n = 0u32;
        // SAFETY: cancela a operação sobre `ov` e espera a conclusão antes de soltar o Box.
        unsafe {
            let _ = CancelIoEx(h(&self.pipe), Some(&raw const *self.ov));
            let _ = GetOverlappedResult(h(&self.pipe), &raw const *self.ov, &raw mut n, true);
        }
        self.pending = Pending::None;
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// As 8 instâncias do pipe, ou erro. Todas ou nenhuma: uma vaga livre deixaria outro processo
/// do usuário criar uma instância e receber conexões do hook (GW inclui CREATE_PIPE_INSTANCE).
/// Nome do pipe que o hook do RTSS procura.
const PIPE_NAME: &str = r"\\.\pipe\RTSS_Frametime";

pub(super) fn create_pipe() -> Result<Vec<Instance>, String> {
    create_pipe_named(PIPE_NAME)
}

pub(super) fn create_pipe_named(name: &str) -> Result<Vec<Instance>, String> {
    let name = wide(name);
    let sid = crate::win::user_sid().ok_or("could not read the user SID")?;
    // DACL protegida: SYSTEM e Administradores com controle total; o usuário (jogo sem
    // elevação) com GRGW, sem WRITE_DAC/WRITE_OWNER. Rótulo de integridade média.
    let sddl = wide(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)"
    ));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL terminado em zero; o descritor é liberado abaixo.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut sd,
            None,
        )
    }
    .map_err(|e| format!("pipe SDDL: {e}"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let instances: Result<Vec<Instance>, String> = (0..INSTANCES)
        .map(|i| Instance::create(&name, i == 0, &sa))
        .collect();
    // SAFETY: descritor alocado acima; as instâncias já foram criadas.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    let mut instances =
        instances.map_err(|e| format!("frametime pipe (another server running?): {e}"))?;
    for inst in &mut instances {
        inst.start_connect();
    }
    Ok(instances)
}
