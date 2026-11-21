//! Uso das engines da GPU por processo (aba GPU): PDH `\GPU Engine(*)\Utilization Percentage`.
//!
//! O objeto tem uma instância por processo × engine (centenas a milhares), então a consulta só
//! existe com a aba visível: é aberta ao entrar e fechada ao sair. O contador é de taxa: a 1ª
//! coleta depois de abrir não tem valor (o motor faz o aquecimento).
//!
//! Instância: `pid_<PID>_luid_<LUID>_phys_<N>_eng_<E>_engtype_<tipo>`. Por processo e tipo vale
//! o maior entre as engines do mesmo tipo (como a coluna "GPU engine" do Task Manager).

use std::collections::HashMap;

use crate::pdh::{self, Query, Slot};

/// `pid_1234_luid_..._phys_0_eng_3_engtype_VideoDecode` → (1234, "luid_..._phys_0", "VideoDecode").
fn parse(name: &str) -> Option<(u32, &str, &str)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    let (adapter, engtype) = rest.split_once("_eng_")?;
    let (_, engtype) = engtype.split_once("_engtype_")?;
    Some((pid.parse().ok()?, adapter, engtype))
}

/// PID → (tipo de engine → uso %), só das instâncias do adaptador `luid`.
pub(super) type Engines = HashMap<u32, Vec<(String, f64)>>;

pub(super) fn group(rows: &[(String, f64)], luid: &str) -> Engines {
    let mut out: HashMap<u32, HashMap<&str, f64>> = HashMap::new();
    for (name, v) in rows {
        let Some((pid, adapter, engtype)) = parse(name) else {
            continue;
        };
        if !adapter.to_ascii_lowercase().starts_with(luid) {
            continue;
        }
        let e = out.entry(pid).or_default().entry(engtype).or_insert(0.0);
        *e = e.max(v.clamp(0.0, 100.0));
    }
    out.into_iter()
        .map(|(pid, m)| {
            let mut v: Vec<(String, f64)> =
                m.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            (pid, v)
        })
        .collect()
}

/// Consulta aberta só enquanto a aba GPU está visível.
pub(super) struct EngineCollector {
    open: Option<(Result<Query, String>, Slot)>,
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
        slot.as_ref().map_err(Clone::clone)?;
        q.as_ref().map_err(Clone::clone)?.collect()?;
        self.samples = self.samples.saturating_add(1);
        Ok(self.samples >= 2)
    }

    /// Linhas da última coleta.
    pub(super) fn rows(&self) -> Result<Vec<(String, f64)>, String> {
        match &self.open {
            Some((q, slot)) => pdh::read_array(q, slot),
            None => Err("GPU engine query is closed".into()),
        }
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
                    for engines in group(&rows, luid).values() {
                        assert!(engines.iter().all(|(_, v)| (0.0..=100.0).contains(v)));
                        assert!(engines.windows(2).all(|w| w[0].0 < w[1].0));
                    }
                }
            }
        });
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
