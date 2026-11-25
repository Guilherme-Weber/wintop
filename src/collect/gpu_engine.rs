//! Uso das engines da GPU por processo (aba GPU): PDH `\GPU Engine(*)\Utilization Percentage`.
//!
//! O objeto tem uma instância por processo × engine (centenas a milhares), então a consulta só
//! existe com a aba visível: é aberta ao entrar e fechada ao sair. O contador é de taxa: a 1ª
//! coleta depois de abrir não tem valor (o motor faz o aquecimento).
//!
//! Instância: `pid_<PID>_luid_<LUID>_phys_<N>_eng_<E>_engtype_<tipo>`. Por processo e tipo vale
//! o maior entre as engines do mesmo tipo (como a coluna "GPU engine" do Task Manager).

use std::collections::HashMap;

use crate::pdh::{self, PdhError, Query, Slot};
#[cfg(test)]
use crate::win::starts_with_ignore_ascii_case;

/// `pid_1234_luid_..._phys_0_eng_3_engtype_VideoDecode` → (1234, "luid_..._phys_0", "VideoDecode").
#[cfg(test)]
fn parse(name: &str) -> Option<(u32, &str, &str)> {
    parse_engine(name).map(|(pid, adapter, _, engtype)| (pid, adapter, engtype))
}

/// Como `parse`, com o número da engine ("3").
pub(super) fn parse_engine(name: &str) -> Option<(u32, &str, &str, &str)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    let (adapter, engine) = rest.split_once("_eng_")?;
    let (eng, engtype) = engine.split_once("_engtype_")?;
    Some((pid.parse().ok()?, adapter, eng, engtype))
}

/// Uso do adaptador inteiro (GPU sem NVML), como o Gerenciador de Tarefas: cada engine física
/// soma o uso de todos os processos (teto 100); o uso da GPU é a engine mais ocupada.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct AdapterUse {
    pub usage: f64,
    /// Engines de vídeo: `None` = o adaptador não tem engine desse tipo.
    pub encode: Option<f64>,
    pub decode: Option<f64>,
    /// Engine mais usada que não é 3D nem de vídeo (tipo, uso); `None` = não existe.
    pub other: Option<(String, f64)>,
}

/// Acumulador em streaming de `AdapterUse`: guarda uma entrada por engine física (~10 por GPU)
/// sem alocar `String` por instância PDH (~540/s).
#[derive(Default)]
pub(super) struct AdapterUseAcc {
    /// `(phys, eng, engtype, soma)`.
    per: Vec<(String, String, String, f64)>,
}

impl AdapterUseAcc {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn push_parsed(&mut self, phys: &str, eng: &str, engtype: &str, v: f64) {
        let v = if v.is_finite() {
            v.clamp(0.0, 100.0)
        } else {
            0.0
        };
        if let Some(entry) = self.per.iter_mut().find(|e| e.0 == phys && e.1 == eng) {
            entry.3 += v;
        } else {
            self.per
                .push((phys.to_string(), eng.to_string(), engtype.to_string(), v));
        }
    }

    pub(super) fn finish(self) -> AdapterUse {
        let mut out = AdapterUse::default();
        let max = |slot: &mut Option<f64>, v: f64| *slot = Some(slot.unwrap_or(0.0).max(v));
        for (_, _, engtype, sum) in self.per {
            let v = sum.min(100.0);
            out.usage = out.usage.max(v);
            if is_encode(&engtype) {
                max(&mut out.encode, v);
            }
            if is_decode(&engtype) {
                max(&mut out.decode, v);
            }
            if engtype != "3D"
                && !is_encode(&engtype)
                && !is_decode(&engtype)
                && out
                    .other
                    .as_ref()
                    .is_none_or(|(t, o)| v > *o || (v == *o && engtype.as_str() < t.as_str()))
            {
                out.other = Some((engtype, v));
            }
        }
        out
    }
}

#[cfg(test)]
pub(super) fn adapter_use(rows: &[(String, f64)], luid: &str) -> AdapterUse {
    let mut acc = AdapterUseAcc::new();
    for (name, v) in rows {
        let Some((_, adapter, eng, engtype)) = parse_engine(name) else {
            continue;
        };
        if !starts_with_ignore_ascii_case(adapter, luid) {
            continue;
        }
        acc.push_parsed(&adapter[luid.len()..], eng, engtype, *v);
    }
    acc.finish()
}

/// Engine de codificação de vídeo. A AMD expõe encode e decode numa engine só
/// ("VideoCodec"): vale para os dois.
pub fn is_encode(engtype: &str) -> bool {
    engtype.contains("Encode") || engtype.contains("Codec")
}

/// Engine de decodificação de vídeo (ver `is_encode`).
pub fn is_decode(engtype: &str) -> bool {
    engtype.contains("Decode") || engtype.contains("Codec")
}

/// PID → (tipo de engine → uso %), só das instâncias do adaptador `luid`.
pub(super) type Engines = HashMap<u32, Vec<(String, f64)>>;

/// Acumulador em streaming de `Engines` (aba GPU): evita materializar `Vec<(String, f64)>`.
#[derive(Default)]
pub(super) struct EnginesAcc {
    by_pid: Engines,
}

impl EnginesAcc {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn push_parsed(&mut self, pid: u32, engtype: &str, v: f64) {
        let v = if v.is_finite() {
            v.clamp(0.0, 100.0)
        } else {
            0.0
        };
        let list = self.by_pid.entry(pid).or_default();
        if let Some((_, cur)) = list.iter_mut().find(|(t, _)| t == engtype) {
            *cur = cur.max(v);
        } else {
            list.push((engtype.to_string(), v));
        }
    }

    pub(super) fn finish(mut self) -> Engines {
        for v in self.by_pid.values_mut() {
            v.sort_by(|a, b| a.0.cmp(&b.0));
        }
        self.by_pid
    }
}

#[cfg(test)]
pub(super) fn group(rows: &[(String, f64)], luid: &str) -> Engines {
    let mut acc = EnginesAcc::new();
    for (name, v) in rows {
        let Some((pid, adapter, engtype)) = parse(name) else {
            continue;
        };
        if !starts_with_ignore_ascii_case(adapter, luid) {
            continue;
        }
        acc.push_parsed(pid, engtype, *v);
    }
    acc.finish()
}

/// Consulta aberta só enquanto a aba GPU está visível.
pub(super) struct EngineCollector {
    open: Option<(Result<Query, PdhError>, Slot)>,
    /// Coletas desde a abertura (taxa só existe a partir da 2ª).
    samples: u32,
}

impl EngineCollector {
    pub(super) fn new() -> Self {
        Self {
            open: None,
            samples: 0,
        }
    }

    /// Fecha a consulta (aba fora de vista).
    pub(super) fn close(&mut self) {
        self.open = None;
        self.samples = 0;
    }

    /// Coleta (abrindo a consulta se preciso). Devolve se já há taxa (2ª coleta em diante).
    pub(super) fn collect(&mut self) -> Result<bool, String> {
        let (q, slot) = self.open.get_or_insert_with(|| {
            let q = Query::open();
            let slot = match &q {
                Ok(q) => q.add(r"\GPU Engine(*)\Utilization Percentage"),
                Err(e) => Err(e.clone()),
            };
            (q, slot)
        });
        // Contador recusado: a causa real (ex.: objeto PDH ausente), não "sem dados".
        slot.as_ref().map_err(ToString::to_string)?;
        q.as_ref()
            .map_err(ToString::to_string)?
            .collect()
            .map_err(String::from)?;
        self.samples = self.samples.saturating_add(1);
        Ok(self.samples >= 2)
    }

    /// Percorre as linhas da última coleta sem alocar `String` por instância.
    pub(super) fn for_each_row(&self, f: impl FnMut(&str, f64)) -> Result<(), String> {
        match &self.open {
            Some((q, slot)) => pdh::for_each_array(q, slot, f).map_err(String::from),
            None => Err("GPU engine query is closed".into()),
        }
    }

    /// Linhas da última coleta.
    #[cfg(test)]
    pub(super) fn rows(&self) -> Result<Vec<(String, f64)>, String> {
        let mut out = Vec::new();
        self.for_each_row(|name, v| out.push((name.to_string(), v)))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nomes de instância e valores quaisquer do PDH (NaN, infinitos, negativos): uso sempre
    /// em 0..=100.
    #[test]
    fn fuzz_instancias_do_pdh() {
        let seed = "pid_1234_luid_0x00000000_0x0000d1e1_phys_0_eng_3_engtype_VideoDecode";
        let other = "pid_1234_luid_0x00000000_0x0000d1e1_phys_0_eng_0_engtype_3D";
        crate::fuzz::run_text("gpu_engine", &[seed, other], 256, |name| {
            // Um valor por chamada: um 100 de outro valor não esconde um NaN que passou.
            let edges = [
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                -5.0,
                50.0,
                1e300,
            ];
            for v in edges {
                let rows = [(name.to_string(), v), (other.to_string(), v)];
                for luid in ["luid_0x00000000_0x0000d1e1", ""] {
                    let u = adapter_use(&rows, luid);
                    for x in [Some(u.usage), u.encode, u.decode, u.other.map(|o| o.1)]
                        .into_iter()
                        .flatten()
                    {
                        assert!((0.0..=100.0).contains(&x));
                    }
                    for engines in group(&rows, luid).values() {
                        assert!(engines.iter().all(|(_, v)| (0.0..=100.0).contains(v)));
                        assert!(engines.windows(2).all(|w| w[0].0 < w[1].0));
                    }
                }
            }
        });
    }

    /// Uso do adaptador: soma por engine física entre os processos, máximo entre as engines.
    #[test]
    fn uso_do_adaptador() {
        let l = "luid_0x00000000_0x0000d1e1_phys_";
        let row = |pid: u32, luid: &str, eng: u32, t: &str, v: f64| {
            (
                format!("pid_{pid}_luid_0x00000000_0x0000{luid}_phys_0_eng_{eng}_engtype_{t}"),
                v,
            )
        };
        let rows = vec![
            row(10, "D1E1", 0, "3D", 30.0),
            row(11, "D1E1", 0, "3D", 25.0),
            row(10, "D1E1", 1, "Copy", 70.0),
            row(12, "D1E1", 4, "VideoDecode", 80.0),
            row(12, "D1E1", 4, "VideoDecode", 50.0),
            row(13, "AAAA", 0, "3D", 99.0),
            row(14, "D1E1", 2, "3D", f64::NAN),
        ];
        let u = adapter_use(&rows, l);
        assert_eq!(u.usage, 100.0); // 80 + 50 na mesma engine de decode, teto 100
        assert_eq!(u.decode, Some(100.0));
        assert_eq!(u.encode, None);
        let only3d = adapter_use(&rows[..3], l);
        assert_eq!(only3d.usage, 70.0);
        let codec = adapter_use(&[row(1, "D1E1", 5, "VideoCodec", 12.0)], l);
        assert_eq!((codec.encode, codec.decode), (Some(12.0), Some(12.0)));
        assert_eq!(adapter_use(&rows, "luid_0x1_"), AdapterUse::default());
        // Fora o 3D e o vídeo: a Copy (a engine 3D do outro adaptador não entra).
        assert_eq!(u.other, Some(("Copy".into(), 70.0)));
        assert_eq!(adapter_use(&rows[..2], l).other, None);
        // Empate: o tipo de nome menor (resultado estável entre coletas).
        let tie = adapter_use(
            &[
                row(1, "D1E1", 6, "Compute_0", 5.0),
                row(1, "D1E1", 1, "Copy", 5.0),
            ],
            l,
        );
        assert_eq!(tie.other, Some(("Compute_0".into(), 5.0)));
    }

    #[test]
    fn instancia_de_engine() {
        assert_eq!(
            parse("pid_1234_luid_0x00000000_0x0000D1E1_phys_0_eng_3_engtype_VideoDecode"),
            Some((1234, "luid_0x00000000_0x0000D1E1_phys_0", "VideoDecode"))
        );
        assert_eq!(parse("luid_0x0_phys_0_eng_0_engtype_3D"), None);
        assert_eq!(parse("pid_x_luid_0_eng_0_engtype_3D"), None);
    }

    #[test]
    fn maior_por_tipo_e_filtro_do_adaptador() {
        let l = "luid_0x00000000_0x0000d1e1_phys_";
        let rows: Vec<(String, f64)> = [
            (
                "pid_10_luid_0x00000000_0x0000D1E1_phys_0_eng_0_engtype_3D",
                40.0,
            ),
            (
                "pid_10_luid_0x00000000_0x0000D1E1_phys_0_eng_1_engtype_Copy",
                3.0,
            ),
            (
                "pid_10_luid_0x00000000_0x0000D1E1_phys_0_eng_2_engtype_Copy",
                9.0,
            ),
            (
                "pid_10_luid_0x00000000_0x0000AAAA_phys_0_eng_0_engtype_3D",
                99.0,
            ),
            (
                "pid_11_luid_0x00000000_0x0000D1E1_phys_0_eng_0_engtype_3D",
                150.0,
            ),
        ]
        .into_iter()
        .map(|(n, v)| (n.to_string(), v))
        .collect();
        let g = group(&rows, l);
        assert_eq!(
            g[&10],
            vec![("3D".to_string(), 40.0), ("Copy".to_string(), 9.0)]
        );
        assert_eq!(g[&11], vec![("3D".to_string(), 100.0)]);
    }

    /// Abre a consulta real: a 1ª coleta não tem taxa; a 2ª tem linhas de algum processo.
    #[test]
    #[ignore = "hardware: GPU com WDDM; varia com o que está aberto"]
    fn consulta_real() {
        let mut c = EngineCollector::new();
        assert_eq!(c.collect(), Ok(false));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let t = std::time::Instant::now();
        assert_eq!(c.collect(), Ok(true));
        let rows = c.rows().expect("linhas");
        println!("{} instâncias em {:?}", rows.len(), t.elapsed());
        assert!(!rows.is_empty());
    }
}
