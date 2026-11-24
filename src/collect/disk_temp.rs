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
use windows::Win32::System::Ioctl::{IOCTL_STORAGE_PREDICT_FAILURE, SMART_RCV_DRIVE_DATA};

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
/// SENDCMDOUTPARAMS (#pragma pack(1)): 4 (cBufferSize) + 12 (DriverStatus) + 512 (bBuffer).
const SMART_RCV_OUT_SIZE: usize = 528;

const NATIVE_EVERY: Duration = Duration::from_secs(10);
const SMART_EVERY: Duration = Duration::from_secs(60);
const REDISCOVER: Duration = Duration::from_secs(60);
/// Falha do SMART com leitura boa antes: mantém o último valor (SSD SATA em economia de energia
/// recusa a consulta às vezes por minutos, e o disco está bem). As primeiras `SMART_QUICK`
/// tentativas vêm a cada `SMART_RETRY`, depois no ritmo normal; só sem nenhuma leitura boa por
/// `SMART_GRACE` a métrica vira falha.
const SMART_RETRY: Duration = Duration::from_secs(10);
const SMART_QUICK: u8 = 3;
const SMART_GRACE: Duration = Duration::from_secs(600);

/// `SENDCMDINPARAMS` (#pragma pack(1), 33 bytes) para `SMART_RCV_DRIVE_DATA` (`READ_ATTRIBUTES`).
/// Reserva quando `IOCTL_STORAGE_PREDICT_FAILURE` (0x2D1100) devolve `ERROR_GEN_FAILURE` (0x1F)
/// em controladoras/SSDs SATA (ele manda só `SMART READ DATA` 0xD0, sem `RETURN STATUS`).
fn smart_rcv_params(number: u32) -> [u8; 33] {
    let mut p = [0u8; 33];
    p[..4].copy_from_slice(&512u32.to_le_bytes()); // cBufferSize = READ_ATTRIBUTE_BUFFER_SIZE
    p[4] = 0xD0; // bFeaturesReg = READ_ATTRIBUTES
    p[5] = 1; // bSectorCountReg
    p[6] = 1; // bSectorNumberReg
    p[7] = 0x4F; // bCylLowReg = SMART_CYL_LOW
    p[8] = 0xC2; // bCylHighReg = SMART_CYL_HI
    p[9] = 0xA0; // bDriveHeadReg
    p[10] = 0xB0; // bCommandReg = SMART_CMD
    p[12] = number as u8; // bDriveNumber
    p
}

/// Temperatura do `SENDCMDOUTPARAMS` devolvido por `SMART_RCV_DRIVE_DATA`.
fn parse_smart_rcv(b: &[u8]) -> Result<(u8, u8), String> {
    let drv_err = *b
        .get(4)
        .ok_or_else(|| "SMART_RCV response too short".to_string())?;
    if drv_err != 0 {
        let ide_err = b.get(5).copied().unwrap_or(0);
        return Err(format!(
            "SMART_RCV driver error 0x{drv_err:X} (IDE 0x{ide_err:X})"
        ));
    }
    b.get(16..)
        .and_then(smart_temp)
        .ok_or_else(|| "no temperature sensor (SMART_RCV 194/190)".to_string())
}

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
    /// Última leitura boa do SMART.
    smart_ok_at: Option<Instant>,
    /// Previsão de falha do SMART (`PredictFailure`): `Ok(true)` = o disco prevê falha.
    health: Metric<bool>,
    health_at: Option<Instant>,
}

impl State {
    fn new() -> Self {
        Self {
            method: Method::Unknown,
            last: None,
            value: fail("not read yet"),
            source: "",
            smart_fails: 0,
            smart_ok_at: None,
            health: neutral("not read yet"),
            health_at: None,
        }
    }

    fn due(&self, now: Instant) -> bool {
        let every = match self.method {
            Method::Smart | Method::None if (1..=SMART_QUICK).contains(&self.smart_fails) => {
                SMART_RETRY
            }
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
                        health: neutral("see the member disks"),
                    }
                } else {
                    DiskTemp {
                        number: d.number,
                        model: d.model.as_ref().ok().cloned(),
                        celsius: st.value.clone(),
                        source: st.source,
                        health: st.health.clone(),
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
    // Sem handle guardado (USB), `alive` confere o modelo: outro disco no mesmo número.
    if !d.alive() {
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
    // Repouso primeiro: um HDD parado (ou SSD SATA em DevSleep/D3 informado pelo Windows) não
    // recebe consulta de temperatura nem SMART.
    let awake = meta.awake();
    if awake == Some(false) {
        st.smart_fails = 0;
        st.value = neutral("asleep");
        return;
    }
    let health_done = read_temp(d, st, meta, awake);
    // Discos SATA já leram `PredictFailure` dentro do mesmo `IOCTL_STORAGE_PREDICT_FAILURE` da
    // temperatura (evita disparar o mesmo IOCTL duas vezes seguidas na controladora SATA). Só
    // discos com temperatura nativa precisam desta consulta separada.
    if !health_done && !d.lost && st.health_at.is_none_or(|t| t.elapsed() >= SMART_EVERY) {
        st.health_at = Some(Instant::now());
        // Uma consulta recusada não apaga um "OK" já lido (o disco não ficou doente por isso);
        // uma previsão de falha lida de verdade sempre aparece.
        let h = predict_failure(d.number);
        if h.is_ok() || st.health.is_err() {
            st.health = h;
        }
    }
}

/// `PredictFailure` do `IOCTL_STORAGE_PREDICT_FAILURE` (o mesmo do SMART): o disco prevê uma
/// falha próxima. Sem suporte do disco ou do driver: estado neutro.
fn predict_failure(number: u32) -> Metric<bool> {
    let Some(rw) = Handle::open(number, GENERIC_READ.0 | GENERIC_WRITE.0) else {
        return fail("no read/write access (not elevated?)");
    };
    match rw.ioctl(IOCTL_STORAGE_PREDICT_FAILURE, None, PREDICT_SIZE) {
        Ok(b) => parse_predict(&b),
        Err(_) => neutral("not reported by the drive"),
    }
}

fn parse_predict(b: &[u8]) -> Metric<bool> {
    b.get(..4)
        .map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]) != 0)
        .map_or_else(|| fail("response too short"), Ok)
}

/// Atualiza `st.health` sem apagar um "OK" já lido se a consulta falhou.
fn update_health(st: &mut State, h: Metric<bool>) {
    st.health_at = Some(Instant::now());
    if h.is_ok() || st.health.is_err() {
        st.health = h;
    }
}

/// Lê a temperatura; devolve `true` se o SMART (`PredictFailure`) já foi consultado aqui.
fn read_temp(d: &mut Inventory, st: &mut State, meta: &Handle, awake: Option<bool>) -> bool {
    // Acordou depois de ter sido visto em repouso: relê o layout (membro do pool) agora.
    d.reread_if_was_asleep(meta);
    if st.method != Method::Smart {
        let native = meta.property(PROP_TEMPERATURE);
        match native.as_deref().ok().and_then(native_temp) {
            Some(t) => {
                st.method = Method::Native;
                st.value = Ok(t);
                st.source = "native";
                return false;
            }
            // Disco com sensor nativo que falhou agora: não cai no SMART (handle de escrita).
            None if st.method == Method::Native => {
                if Handle::open(d.number, 0).is_none() {
                    d.lost = true;
                }
                let cause = native.err().unwrap_or_else(|| "no native reading".into());
                crate::debug::log(
                    "disk_temp",
                    &format!("disk {} native temp failed: {cause}", d.number),
                );
                st.value = fail(cause);
                return false;
            }
            None => {}
        }
    }
    // Sem temperatura nativa: SMART (exige handle de leitura e escrita). Uma única chamada de
    // `IOCTL_STORAGE_PREDICT_FAILURE` já traz tanto `PredictFailure` (bytes 0..4) quanto os
    // atributos SMART (bytes 4..516). Se ela falhar (ex.: 0x1F em SSD SATA / controladora
    // Marvell), tenta `SMART_RCV_DRIVE_DATA` (0x07C088) no mesmo handle.
    let model = d.model.as_deref().unwrap_or("?");
    let smart = match Handle::open(d.number, GENERIC_READ.0 | GENERIC_WRITE.0) {
        None => {
            update_health(st, fail("no read/write access (not elevated?)"));
            Err("no read/write access (not elevated?)".to_string())
        }
        Some(rw) => match rw.ioctl(IOCTL_STORAGE_PREDICT_FAILURE, None, PREDICT_SIZE) {
            Ok(b) => {
                update_health(st, parse_predict(&b));
                b.get(4..)
                    .and_then(smart_temp)
                    .ok_or_else(|| "no temperature sensor (native or SMART 194/190)".to_string())
            }
            Err(e1) => {
                update_health(st, neutral("not reported by the drive"));
                let params = smart_rcv_params(d.number);
                match rw
                    .ioctl(SMART_RCV_DRIVE_DATA, Some(&params), SMART_RCV_OUT_SIZE)
                    .and_then(|b| parse_smart_rcv(&b))
                {
                    Ok(t) => {
                        crate::debug::log(
                            "disk_temp",
                            &format!(
                                "disk {} ({model}) PREDICT_FAILURE failed ({e1}, awake={awake:?}); \
                                 SMART_RCV_DRIVE_DATA fallback succeeded ({} °C)",
                                d.number, t.1
                            ),
                        );
                        Ok(t)
                    }
                    Err(e2) => {
                        crate::debug::log(
                            "disk_temp",
                            &format!(
                                "disk {} ({model}) SMART failed (awake={awake:?}): \
                                 PREDICT_FAILURE=[{e1}], SMART_RCV=[{e2}]",
                                d.number
                            ),
                        );
                        Err(e1)
                    }
                }
            }
        },
    };
    match smart {
        Ok((id, c)) => {
            st.method = Method::Smart;
            st.smart_fails = 0;
            st.smart_ok_at = Some(Instant::now());
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
            let now = Instant::now();
            if smart_tolerated(st, d.lost, now) {
                st.smart_fails = st.smart_fails.saturating_add(1);
                let age = st
                    .smart_ok_at
                    .map_or(0, |t| now.duration_since(t).as_secs());
                crate::debug::log(
                    "disk_temp",
                    &format!(
                        "disk {} ({model}) SMART error tolerated (#{}, last ok {age}s ago, \
                         awake={awake:?}): {e}",
                        d.number, st.smart_fails
                    ),
                );
                return true;
            }
            // Na partida (sem leitura boa ainda), tenta mais cedo nas primeiras falhas em vez
            // de esperar 60 s. Depois da janela de tolerância vencida, volta ao ritmo normal.
            if st.method == Method::None && !d.lost && st.smart_fails < SMART_QUICK {
                st.smart_fails = st.smart_fails.saturating_add(1);
            } else {
                st.smart_fails = 0;
            }
            st.value = fail(format!("SMART: {e}"));
        }
    }
    true
}

/// Falha do SMART que não aparece: já houve leitura boa há menos de `SMART_GRACE` e o disco
/// continua lá.
fn smart_tolerated(st: &State, lost: bool, now: Instant) -> bool {
    st.method == Method::Smart
        && st.value.is_ok()
        && !lost
        && st
            .smart_ok_at
            .is_some_and(|t| now.duration_since(t) < SMART_GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_recusado_por_minutos_nao_vira_falha() {
        let mut st = State::new();
        st.method = Method::Smart;
        st.value = Ok(37);
        let t0 = Instant::now();
        st.smart_ok_at = Some(t0);
        // Recusas por 9 minutos: o último valor fica.
        assert!(smart_tolerated(&st, false, t0 + Duration::from_secs(540)));
        // Passou do prazo, ou o disco sumiu: aparece.
        assert!(!smart_tolerated(&st, false, t0 + SMART_GRACE));
        assert!(!smart_tolerated(&st, true, t0));
        // Depois das tentativas rápidas, volta ao ritmo normal (sem consultar a cada 10 s).
        st.last = Some(t0);
        st.smart_fails = SMART_QUICK + 1;
        assert!(!st.due(t0 + SMART_RETRY));
        assert!(st.due(t0 + SMART_EVERY));
    }

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

    #[test]
    fn smart_rcv_parametros_e_leitura() {
        let p = smart_rcv_params(2);
        assert_eq!(u32::from_le_bytes(p[..4].try_into().unwrap()), 512);
        assert_eq!(
            (p[4], p[7], p[8], p[10], p[12]),
            (0xD0, 0x4F, 0xC2, 0xB0, 2)
        );
        // Resposta válida com atributo 194 = 38 °C (offset 16 do bBuffer + 2 revisão + 5 raw).
        let mut out = vec![0u8; SMART_RCV_OUT_SIZE];
        out[16 + 2] = 194;
        out[16 + 2 + 5] = 38;
        assert_eq!(parse_smart_rcv(&out), Ok((194, 38)));
        // Erro reportado pelo driver no DriverStatus (offset 4).
        out[4] = 3;
        out[5] = 0x51;
        assert!(parse_smart_rcv(&out).unwrap_err().contains("0x3"));
        // Buffer curto demais.
        assert!(parse_smart_rcv(&[0u8; 3]).is_err());
    }
}
