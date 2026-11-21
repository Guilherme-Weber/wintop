//! Temperatura dos discos (thread lenta: cada leitura leva até 156 ms nos HDDs).
//!
//! - nativa: `StorageDeviceTemperatureProperty` (NVMe e alguns SATA), a cada 10 s;
//! - SMART: `IOCTL_STORAGE_PREDICT_FAILURE`, atributo 194 (senão 190), a cada 60 s;
//! - HDD em repouso (`GetDevicePowerState`) não é lido, para não acordá-lo;
//! - Storage Space: máximo dos membros (com um pool só).
//!
//! Offsets conferidos com o MSVC (winioctl.h) na Etapa 0.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::System::Ioctl::IOCTL_STORAGE_PREDICT_FAILURE;

use super::disk::{Handle, Inventory, discover_all, refresh};
use crate::model::{Bus, DiskTemp, Metric, Missing, fail, neutral};

const PROP_TEMPERATURE: u32 = 52; // StorageDeviceTemperatureProperty
const TEMP_NOT_REPORTED: i16 = i16::MIN; // STORAGE_TEMPERATURE_VALUE_NOT_REPORTED
// STORAGE_TEMPERATURE_DATA_DESCRIPTOR / STORAGE_TEMPERATURE_INFO
const T_INFO_COUNT: usize = 12;
const T_INFO: usize = 24;
const T_INFO_SIZE: usize = 16;
const T_TEMP: usize = 2;
/// STORAGE_PREDICT_FAILURE { ULONG PredictFailure; UCHAR VendorSpecific[512]; }.
const PREDICT_SIZE: usize = 516;

const NATIVE_EVERY: Duration = Duration::from_secs(10);
const SMART_EVERY: Duration = Duration::from_secs(60);
const REDISCOVER: Duration = Duration::from_secs(60);
/// Falha do SMART com leitura boa antes: mantém o último valor e tenta de novo mais cedo; só
/// depois de `SMART_TOLERATED` falhas seguidas a métrica vira falha (SSD SATA em economia de
/// energia recusa a consulta de vez em quando).
const SMART_RETRY: Duration = Duration::from_secs(10);
const SMART_TOLERATED: u8 = 3;

/// Primeira temperatura reportada (índice 0 = composta no NVMe), ignorando "não reportado".
fn native_temp(b: &[u8]) -> Option<i16> {
    let count = usize::from(u16::from_le_bytes(
        b.get(T_INFO_COUNT..T_INFO_COUNT + 2)?.try_into().ok()?,
    ));
    // Não passa do que cabe no buffer (contagem corrompida não vira 65 mil voltas).
    let count = count.min(b.len().saturating_sub(T_INFO) / T_INFO_SIZE);
    (0..count).find_map(|i| {
        let off = T_INFO + i * T_INFO_SIZE + T_TEMP;
        let t = i16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?);
        (t != TEMP_NOT_REPORTED).then_some(t)
    })
}

/// Temperatura SMART: 30 atributos de 12 bytes após 2 bytes de revisão; valor bruto no byte 5.
fn smart_temp(vendor: &[u8]) -> Option<(u8, u8)> {
    let attr = |want: u8| {
        (0..30).find_map(|i| {
            let b = 2 + i * 12;
            let a = vendor.get(b..b + 12)?;
            (a[0] == want && a[5] > 0).then_some((want, a[5]))
        })
    };
    attr(194).or_else(|| attr(190))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Unknown,
    Native,
    Smart,
    /// Nem nativa nem SMART responderam: nova tentativa no ritmo do SMART.
    None,
}

struct State {
    method: Method,
    last: Option<Instant>,
    value: Metric<i16>,
    source: &'static str,
    /// Falhas seguidas do SMART desde a última leitura boa.
    smart_fails: u8,
}

impl State {
    fn new() -> Self {
        Self {
            method: Method::Unknown,
            last: None,
            value: fail("not read yet"),
            source: "",
            smart_fails: 0,
        }
    }

    fn due(&self, now: Instant) -> bool {
        let every = match self.method {
            Method::Smart if self.smart_fails > 0 => SMART_RETRY,
            Method::Smart | Method::None => SMART_EVERY,
            Method::Unknown | Method::Native => NATIVE_EVERY,
        };
        self.last.is_none_or(|t| now.duration_since(t) >= every)
    }
}

pub struct DiskTempCollector {
    disks: Vec<(Inventory, State)>,
    discovered: Instant,
}

impl DiskTempCollector {
    pub fn new() -> Self {
        Self {
            disks: discover_all()
                .into_iter()
                .map(|d| (d, State::new()))
                .collect(),
            discovered: Instant::now(),
        }
    }

    fn rediscover(&mut self) {
        self.discovered = Instant::now();
        // O estado volta pelo número; o de um disco perdido é descartado (o número pode ser
        // de outro disco agora).
        let mut inv = Vec::with_capacity(self.disks.len());
        let mut states = Vec::with_capacity(self.disks.len());
        for (d, st) in self.disks.drain(..) {
            if !d.lost {
                states.push((d.number, st));
            }
            inv.push(d);
        }
        refresh(&mut inv);
        self.disks = inv
            .into_iter()
            .map(|d| {
                let st = states
                    .iter()
                    .position(|(n, _)| *n == d.number)
                    .map_or_else(State::new, |i| states.swap_remove(i).1);
                (d, st)
            })
            .collect();
    }

    pub fn read(&mut self, stop: &AtomicBool) -> Metric<Vec<DiskTemp>> {
        if self.discovered.elapsed() >= REDISCOVER || self.disks.iter().any(|(d, _)| d.lost) {
            self.rediscover();
        }
        if self.disks.is_empty() {
            return fail("no physical disk could be opened");
        }
        let now = Instant::now();
        for (d, st) in &mut self.disks {
            // Cada HDD leva até ~156 ms: a parada do motor não espera a volta inteira.
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if !st.due(now) {
                continue;
            }
            st.last = Some(now);
            if d.bus == Ok(Bus::Spaces) {
                // O disco virtual não é lido; só confere que continua existindo.
                if Handle::open(d.number, 0).is_none() {
                    d.lost = true;
                }
                continue;
            }
            read_one(d, st);
        }
        let pools = self
            .disks
            .iter()
            .filter(|(d, _)| d.bus == Ok(Bus::Spaces))
            .count();
        let members: Vec<&Metric<i16>> = self
            .disks
            .iter()
            .filter(|(d, _)| d.pool_member == Ok(true))
            .map(|(_, st)| &st.value)
            .collect();
        // Discos cujo layout ainda não foi lido porque estavam em repouso: podem ser membros.
        let unknown_asleep = self
            .disks
            .iter()
            .any(|(d, _)| matches!(d.pool_member, Err(Missing::Neutral(_))));
        Ok(self
            .disks
            .iter()
            .map(|(d, st)| {
                if d.bus == Ok(Bus::Spaces) {
                    DiskTemp {
                        number: d.number,
                        model: d.model.as_ref().ok().cloned(),
                        celsius: pool_temp(pools, &members, unknown_asleep),
                        source: "pool members",
                    }
                } else {
                    DiskTemp {
                        number: d.number,
                        model: d.model.as_ref().ok().cloned(),
                        celsius: st.value.clone(),
                        source: st.source,
                    }
                }
            })
            .collect())
    }
}

/// Máximo dos membros do pool. Com mais de um pool, a associação membro-pool é pendente.
/// `unknown_asleep`: há discos em repouso cujo layout (membro ou não) ainda não foi lido.
fn pool_temp(pools: usize, members: &[&Metric<i16>], unknown_asleep: bool) -> Metric<i16> {
    if pools > 1 {
        return neutral("several pools: association pending");
    }
    if let Some(max) = members.iter().filter_map(|m| m.as_ref().ok()).max() {
        return Ok(*max);
    }
    let all_asleep = members
        .iter()
        .all(|m| matches!(m, Err(Missing::Neutral(_))));
    if (members.is_empty() && unknown_asleep) || (!members.is_empty() && all_asleep) {
        return neutral("asleep");
    }
    fail("no member temperature")
}

fn read_one(d: &mut Inventory, st: &mut State) {
    // Handle guardado morto (disco removido, talvez outro no mesmo número): não lê nada por
    // ele nem por um handle novo (seria o outro disco com o modelo antigo).
    if d.handle.is_some() && !d.alive() {
        d.lost = true;
        d.handle = None;
        st.value = fail("disk removed");
        return;
    }
    // O handle guardado sai do inventário durante a leitura (para `d` poder ser alterado) e
    // volta no fim; sem handle guardado (USB), abre só para esta leitura.
    let held = d.handle.take();
    let temp = if held.is_none() {
        Handle::open(d.number, 0)
    } else {
        None
    };
    if let Some(meta) = held.as_ref().or(temp.as_ref()) {
        read_with(d, st, meta);
    } else {
        d.lost = true;
        st.value = fail("disk not found");
    }
    // Disco perdido: o handle velho não volta (a redescoberta abre outro).
    d.handle = if d.lost { None } else { held };
}

fn read_with(d: &mut Inventory, st: &mut State, meta: &Handle) {
    // Repouso primeiro: um HDD parado não recebe nenhuma consulta de temperatura.
    if d.ssd == Ok(false) && meta.awake() == Some(false) {
        st.smart_fails = 0;
        st.value = neutral("asleep");
        return;
    }
    // Acordou depois de ter sido visto em repouso: relê o layout (membro do pool) agora.
    d.reread_if_was_asleep(meta);
    if st.method != Method::Smart {
        let native = meta.property(PROP_TEMPERATURE);
        match native.as_deref().ok().and_then(native_temp) {
            Some(t) => {
                st.method = Method::Native;
                st.value = Ok(t);
                st.source = "native";
                return;
            }
            // Disco com sensor nativo que falhou agora: não cai no SMART (handle de escrita).
            None if st.method == Method::Native => {
                if Handle::open(d.number, 0).is_none() {
                    d.lost = true;
                }
                st.value = fail(native.err().unwrap_or_else(|| "no native reading".into()));
                return;
            }
            None => {}
        }
    }
    // Sem temperatura nativa: SMART (exige handle de leitura e escrita).
    let smart = Handle::open(d.number, GENERIC_READ.0 | GENERIC_WRITE.0)
        .ok_or_else(|| "no read/write access (not elevated?)".to_string())
        .and_then(|rw| rw.ioctl(IOCTL_STORAGE_PREDICT_FAILURE, None, PREDICT_SIZE))
        .and_then(|b| {
            b.get(4..)
                .and_then(smart_temp)
                .ok_or_else(|| "no temperature sensor (native or SMART 194/190)".to_string())
        });
    match smart {
        Ok((id, c)) => {
            st.method = Method::Smart;
            st.smart_fails = 0;
            st.value = Ok(i16::from(c));
            st.source = if id == 194 { "SMART 194" } else { "SMART 190" };
        }
        Err(e) => {
            // O disco sumiu (o handle guardado não percebe): sai na redescoberta.
            if Handle::open(d.number, 0).is_none() {
                d.lost = true;
            }
            if st.method == Method::Unknown {
                st.method = Method::None;
            }
            // Falha passageira depois de leituras boas: segura o último valor por enquanto.
            if st.method == Method::Smart && st.value.is_ok() && !d.lost {
                st.smart_fails = st.smart_fails.saturating_add(1);
                if st.smart_fails < SMART_TOLERATED {
                    return;
                }
            }
            // Falha persistente: volta ao ritmo normal do SMART.
            st.smart_fails = 0;
            st.value = fail(format!("SMART: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperatura_nativa_ignora_nao_reportado() {
        let mut b = vec![0u8; T_INFO + 2 * T_INFO_SIZE];
        b[T_INFO_COUNT..T_INFO_COUNT + 2].copy_from_slice(&2u16.to_le_bytes());
        b[T_INFO + T_TEMP..T_INFO + T_TEMP + 2].copy_from_slice(&TEMP_NOT_REPORTED.to_le_bytes());
        let second = T_INFO + T_INFO_SIZE + T_TEMP;
        b[second..second + 2].copy_from_slice(&41i16.to_le_bytes());
        assert_eq!(native_temp(&b), Some(41));
        b[T_INFO_COUNT..T_INFO_COUNT + 2].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(native_temp(&b), None);
        assert_eq!(native_temp(&[0u8; 4]), None);
    }

    #[test]
    fn smart_falha_passageira_reduz_o_intervalo() {
        let mut st = State::new();
        st.method = Method::Smart;
        st.value = Ok(40);
        let t0 = Instant::now();
        st.last = Some(t0);
        assert!(!st.due(t0 + SMART_RETRY));
        st.smart_fails = 1;
        assert!(st.due(t0 + SMART_RETRY));
    }

    #[test]
    fn fuzz_temperatura_nativa_e_smart() {
        let mut n = vec![0u8; T_INFO + 2 * T_INFO_SIZE];
        n[T_INFO_COUNT..T_INFO_COUNT + 2].copy_from_slice(&2u16.to_le_bytes());
        n[T_INFO + T_TEMP..T_INFO + T_TEMP + 2].copy_from_slice(&38i16.to_le_bytes());
        let mut s = vec![0u8; PREDICT_SIZE];
        s[2] = 194;
        s[2 + 5] = 35;
        crate::fuzz::run("temperatura", &[n, s], 1024, |_, b| {
            if let Some(t) = native_temp(b) {
                assert_ne!(t, TEMP_NOT_REPORTED);
            }
            if let Some((id, v)) = smart_temp(b) {
                assert!(matches!(id, 190 | 194) && v > 0);
            }
        });
    }

    #[test]
    fn smart_prefere_194() {
        let mut v = vec![0u8; 2 + 30 * 12];
        v[2] = 190;
        v[2 + 5] = 30;
        v[2 + 12] = 194;
        v[2 + 12 + 5] = 35;
        assert_eq!(smart_temp(&v), Some((194, 35)));
        v[2 + 12] = 0;
        assert_eq!(smart_temp(&v), Some((190, 30)));
    }

    #[test]
    fn temperatura_do_pool() {
        let (a, b): (Metric<i16>, Metric<i16>) = (Ok(35), Ok(41));
        let asleep: Metric<i16> = neutral("asleep");
        let bad: Metric<i16> = fail("x");
        assert_eq!(pool_temp(1, &[&a, &asleep, &b], false), Ok(41));
        assert_eq!(pool_temp(1, &[&asleep, &asleep], false), neutral("asleep"));
        assert!(matches!(
            pool_temp(1, &[&asleep, &bad], false),
            Err(Missing::Fail(_))
        ));
        assert!(matches!(
            pool_temp(2, &[&a], false),
            Err(Missing::Neutral(_))
        ));
        // Todos os membros dormiam na descoberta: o layout não foi lido, o pool está em repouso.
        assert_eq!(pool_temp(1, &[], true), neutral("asleep"));
        assert!(matches!(pool_temp(1, &[], false), Err(Missing::Fail(_))));
    }
}
