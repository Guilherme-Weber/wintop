//! Rede: interfaces físicas (Ethernet e Wi-Fi), link e taxas de envio/recebimento.
//!
//! A lista vem do `GetIfTable2` (redescoberta a cada 30 s); a cada coleta só `GetIfEntry2` de
//! cada interface conhecida, mais barato.

use std::time::{Duration, Instant};

use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfEntry2, GetIfTable2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::MediaConnectStateConnected;

use crate::model::{Metric, Nic, fail, neutral};
use crate::win::from_wide;

/// `NDIS_LINK_SPEED_UNKNOWN`: o driver não informou a velocidade.
const LINK_SPEED_UNKNOWN: u64 = u64::MAX;

const REDISCOVER: Duration = Duration::from_secs(30);

// Bits de `InterfaceAndOperStatusFlags`.
const HARDWARE_INTERFACE: u8 = 1 << 0;
const FILTER_INTERFACE: u8 = 1 << 1;

struct Known {
    row: Box<MIB_IF_ROW2>,
    /// (instante, recebidos, enviados) da coleta anterior.
    last: Option<(Instant, u64, u64)>,
}

pub struct NetCollector {
    nics: Vec<Known>,
    discovered: Option<Instant>,
    error: Option<String>,
}

/// Interface física de rede de dados (descarta virtuais, filtros, loopback, túneis).
fn is_physical(row: &MIB_IF_ROW2) -> bool {
    let flags = row.InterfaceAndOperStatusFlags._bitfield;
    flags & HARDWARE_INTERFACE != 0
        && flags & FILTER_INTERFACE == 0
        && (row.Type == IF_TYPE_ETHERNET_CSMACD || row.Type == IF_TYPE_IEEE80211)
}

/// LUID da interface como inteiro.
fn luid(row: &MIB_IF_ROW2) -> u64 {
    // SAFETY: NET_LUID_LH é uma união de 64 bits sem bytes de preenchimento; todo padrão de bits
    // é um u64 válido.
    unsafe { row.InterfaceLuid.Value }
}

/// bit/s entre duas leituras de octetos (contadores de 64 bits; volta = sem taxa).
fn rate(prev: u64, now: u64, dt: Duration) -> Option<f64> {
    let d = now.checked_sub(prev)?;
    let s = dt.as_secs_f64();
    (s > 0.0).then(|| d as f64 * 8.0 / s)
}

impl NetCollector {
    pub fn new() -> Self {
        let mut me = Self {
            nics: Vec::new(),
            discovered: None,
            error: None,
        };
        me.discover();
        me
    }

    fn discover(&mut self) {
        self.discovered = Some(Instant::now());
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        // SAFETY: `table` recebe uma tabela alocada pelo sistema, liberada abaixo.
        let r = unsafe { GetIfTable2(&raw mut table) };
        if r.is_err() || table.is_null() {
            self.error = Some(format!("GetIfTable2: error {}", r.0));
            return;
        }
        // Libera a tabela também se algo abaixo entrar em pânico (catch_unwind do coletor).
        struct Table(*mut MIB_IF_TABLE2);
        impl Drop for Table {
            fn drop(&mut self) {
                // SAFETY: tabela devolvida por GetIfTable2, liberada uma única vez.
                unsafe { FreeMibTable(self.0.cast()) };
            }
        }
        let _guard = Table(table);
        // SAFETY: tabela válida até o guard soltar; `NumEntries` linhas a partir de `Table`.
        let rows = unsafe {
            std::slice::from_raw_parts(
                (&raw const (*table).Table).cast::<MIB_IF_ROW2>(),
                (*table).NumEntries as usize,
            )
        };
        let mut found: Vec<Known> = rows
            .iter()
            .filter(|r| is_physical(r))
            .map(|r| {
                // Mantém a taxa de quem já era conhecido.
                let last = self
                    .nics
                    .iter()
                    .find(|k| luid(&k.row) == luid(r))
                    .and_then(|k| k.last);
                Known {
                    row: Box::new(*r),
                    last,
                }
            })
            .collect();
        found.sort_by_key(|k| from_wide(&k.row.Alias));
        self.nics = found;
        self.error = None;
    }

    pub fn read(&mut self) -> Metric<Vec<Nic>> {
        if self.discovered.is_none_or(|t| t.elapsed() >= REDISCOVER) {
            self.discover();
        }
        if let Some(e) = &self.error {
            return fail(e.clone());
        }
        if self.nics.is_empty() {
            return fail("no physical network interface");
        }
        let mut out = Vec::with_capacity(self.nics.len());
        let mut lost = false;
        for k in &mut self.nics {
            // SAFETY: linha própria com InterfaceLuid preenchido (chave da consulta).
            let r = unsafe { GetIfEntry2(&raw mut *k.row) };
            let now = Instant::now();
            let nic = Nic {
                alias: from_wide(&k.row.Alias),
                description: from_wide(&k.row.Description),
                link: fail(""),
                send: fail(""),
                recv: fail(""),
                errors: fail(""),
            };
            if r.is_err() {
                lost = true;
                out.push(Nic {
                    link: fail(format!("GetIfEntry2: error {}", r.0)),
                    send: fail("interface disappeared"),
                    recv: fail("interface disappeared"),
                    errors: fail("interface disappeared"),
                    ..nic
                });
                continue;
            }
            let (rx, tx) = (k.row.InOctets, k.row.OutOctets);
            let prev = k.last.replace((now, rx, tx));
            let connected = k.row.MediaConnectState == MediaConnectStateConnected;
            let taxa = |f: fn(&(Instant, u64, u64)) -> u64, cur: u64| -> Metric<f64> {
                match prev {
                    None => fail("waiting for a second sample"),
                    Some(p) => rate(f(&p), cur, now - p.0)
                        .map_or_else(|| fail("counter went backwards (interface reset)"), Ok),
                }
            };
            out.push(Nic {
                link: match (connected, k.row.ReceiveLinkSpeed) {
                    (false, _) => neutral("disconnected"),
                    (true, LINK_SPEED_UNKNOWN) => fail("link speed unknown"),
                    (true, speed) => Ok(speed),
                },
                send: taxa(|p| p.2, tx),
                recv: taxa(|p| p.1, rx),
                // Só erros (pacote com defeito); descartes são comuns (filtros, protocolos
                // desconhecidos) e não indicam problema.
                errors: Ok((k.row.InErrors, k.row.OutErrors)),
                ..nic
            });
        }
        if lost {
            // Interface removida ou reiniciada: a próxima coleta redescobre.
            self.discovered = None;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxa_em_bits() {
        assert_eq!(rate(1000, 2000, Duration::from_secs(2)), Some(4000.0));
        assert_eq!(rate(2000, 1000, Duration::from_secs(1)), None);
        assert_eq!(rate(0, 10, Duration::ZERO), None);
    }
}
