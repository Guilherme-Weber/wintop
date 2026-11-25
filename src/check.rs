//! `wintop --check`: coleta de aquecimento, espera ~300 ms, coleta de novo (contadores de
//! taxa) e imprime o estado de cada métrica. Código de saída 1 se alguma métrica falhou.

use std::io::Write;
use std::time::Duration;

use crate::collect::{FastCollectors, RtssCollector, SlowCollectors};
use crate::fmt::{bits, bytes, num, pct};
use crate::model::{Fast, Fps, Metric, Missing, Slow};
use crate::win::printable;

const WARMUP: Duration = Duration::from_millis(300);

/// Uma linha do relatório (para o `--json`).
struct Entry {
    section: String,
    subject: String,
    name: String,
    status: &'static str,
    value: String,
}

struct Report<W: Write> {
    ok: u32,
    neutral: u32,
    failed: u32,
    /// `--json`: nada é impresso durante a coleta; tudo sai num objeto no fim.
    json: bool,
    section: String,
    subject: String,
    entries: Vec<Entry>,
    out: W,
}

impl<W: Write> Report<W> {
    fn new(out: W, json: bool) -> Self {
        Self {
            ok: 0,
            neutral: 0,
            failed: 0,
            json,
            section: String::new(),
            subject: String::new(),
            entries: Vec::new(),
            out,
        }
    }

    fn section(&mut self, title: &str) {
        if !self.json {
            let _ = writeln!(self.out, "\n{title}");
        }
        self.section = title.to_string();
        self.subject.clear();
    }

    /// Dispositivo das linhas seguintes (disco, placa, GPU, jogo).
    fn subject(&mut self, text: &str) {
        let clean = printable(text);
        if !self.json {
            let _ = writeln!(self.out, "  {clean}");
        }
        self.subject = clean;
    }

    fn record(&mut self, label: &str, tag: &'static str, status: &'static str, text: &str) {
        let clean = printable(text);
        // Nomes e mensagens vêm de dispositivos e do sistema: sem controles no terminal.
        if self.json {
            self.entries.push(Entry {
                section: self.section.clone(),
                subject: self.subject.clone(),
                name: label.trim().to_string(),
                status,
                value: clean,
            });
        } else {
            let _ = writeln!(self.out, "  {tag}  {label:<34} {clean}");
        }
    }

    fn missing(&mut self, label: &str, e: &Missing) {
        let (tag, status, text) = match e {
            Missing::Neutral(s) => {
                self.neutral += 1;
                ("N/A   ", "neutral", s.as_str())
            }
            Missing::Fail(s) => {
                self.failed += 1;
                ("FAIL  ", "failed", s.as_str())
            }
        };
        self.record(label, tag, status, text);
    }

    fn line<T>(&mut self, label: &str, m: &Metric<T>, show: impl FnOnce(&T) -> String) {
        match m {
            Ok(v) => {
                self.ok += 1;
                let text = show(v);
                self.record(label, "OK    ", "ok", &text);
            }
            Err(e) => self.missing(label, e),
        }
    }

    /// O relatório inteiro num objeto JSON (uma linha).
    fn to_json(&self, elevated: bool, fast_ms: f64, slow_ms: f64) -> String {
        let metrics: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                format!(
                    r#"{{"section":{},"subject":{},"name":{},"status":{},"value":{}}}"#,
                    json_str(&e.section),
                    json_str(&e.subject),
                    json_str(&e.name),
                    json_str(e.status),
                    json_str(&e.value)
                )
            })
            .collect();
        format!(
            r#"{{"version":{},"elevated":{elevated},"ok":{},"neutral":{},"failed":{},"fast_ms":{fast_ms:.1},"slow_ms":{slow_ms:.1},"metrics":[{}]}}"#,
            json_str(env!("CARGO_PKG_VERSION")),
            self.ok,
            self.neutral,
            self.failed,
            metrics.join(",")
        )
    }
}

/// Texto JSON (aspas, barras e controles escapados).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!(r"\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn run(json: bool) -> i32 {
    let elevated = crate::win::is_elevated();
    if !json {
        println!(
            "wintop {} --check (elevated: {})",
            env!("CARGO_PKG_VERSION"),
            if elevated { "yes" } else { "no" }
        );
    }
    let nvml = crate::collect::shared_nvml();
    let mut fast = FastCollectors::new(nvml.clone());
    let mut slow = SlowCollectors::new(nvml);
    let _ = fast.collect();
    std::thread::sleep(WARMUP);
    let f = fast.collect();
    // Com as abas Processos e GPU "visíveis": prova também as fontes delas (com aquecimento).
    let s = slow
        .collect(
            &std::sync::atomic::AtomicBool::new(false),
            crate::model::View {
                procs: true,
                gpu: true,
            },
            0,
        )
        .unwrap_or_else(|| Slow::failed("stopped"));
    // Sem iniciar o RTSS nem criar o pipe: o --check só diagnostica.
    // Só lê: o --check não cria nem grava o config.
    let cfg = crate::config::load_home(false);
    let fps = RtssCollector::new(false, false)
        .with_ignored(cfg.config.rtss_ignored.clone())
        .sample();

    let mut r = Report::new(std::io::stdout(), json);
    fast_report(&mut r, &f);
    slow_report(&mut r, &s);
    fps_report(&mut r, &fps);
    r.section("Configuration");
    let path = crate::config::default_path().map_or_else(
        || "no AppData folder".to_string(),
        |p| p.display().to_string(),
    );
    if cfg.warnings.is_empty() {
        r.line("config.toml", &Ok(()), |()| {
            format!(
                "{path} (theme {}, interval {} ms, debug {})",
                cfg.config.theme,
                cfg.config.interval_ms,
                if cfg.config.debug { "on" } else { "off" }
            )
        });
    } else {
        for w in &cfg.warnings {
            r.missing("config.toml", &Missing::Fail(w.clone()));
        }
    }
    own_report(&mut r, &f);
    security_report(&mut r);
    let (fast_ms, slow_ms) = (f.took.as_secs_f64() * 1e3, s.took.as_secs_f64() * 1e3);
    if json {
        println!("{}", r.to_json(elevated, fast_ms, slow_ms));
    } else {
        println!("\nFast collection: {fast_ms:.1} ms; slow: {slow_ms:.1} ms");
        println!(
            "Result: {} OK, {} neutral, {} failed",
            r.ok, r.neutral, r.failed
        );
    }
    i32::from(r.failed > 0)
}

fn fast_report<W: Write>(r: &mut Report<W>, f: &Fast) {
    r.section("CPU");
    match &f.cpu {
        Err(e) => r.missing("collector", e),
        Ok(c) => {
            r.line("Name", &c.name, Clone::clone);
            r.line("Topology", &c.topology, |t| {
                format!(
                    "{} socket(s), {} cores, {} logical; L1 {}, L2 {}, L3 {}",
                    t.sockets,
                    t.cores,
                    t.logical,
                    bytes(t.l1),
                    bytes(t.l2),
                    bytes(t.l3)
                )
            });
            r.line("Base speed", &c.base_mhz, |m| format!("{m} MHz"));
            r.line("Virtualization", &c.virtualization, |v| {
                if *v { "enabled" } else { "disabled" }.into()
            });
            r.line("Total usage", &c.usage, |v| pct(*v));
            r.line("Usage per logical processor", &c.per_logical, |v| {
                let list: Vec<String> = v.iter().map(|x| format!("{x:.0}")).collect();
                format!("{} logical: {}", v.len(), list.join(" "))
            });
            r.line("Kernel share", &c.kernel, |v| pct(*v));
            r.line("Kernel per logical processor", &c.kernel_per_logical, |v| {
                format!("{} logical", v.len())
            });
            r.line("Current speed", &c.freq_mhz, |v| format!("{v:.0} MHz"));
            r.line("Speed per logical", &c.freq_per_logical, |f| {
                if f.is_empty() {
                    "0 logical".into()
                } else {
                    let (lo, hi) = f
                        .iter()
                        .fold((f64::MAX, 0.0_f64), |(a, b), v| (a.min(*v), b.max(*v)));
                    format!("{} logical, {lo:.0}-{hi:.0} MHz (peak {hi:.0})", f.len())
                }
            });
            r.line("Processes / threads / handles", &c.counts, |n| {
                format!("{} / {} / {}", n.processes, n.threads, n.handles)
            });
            let up = c.uptime.as_secs();
            r.line("Up time", &Ok(()), |()| {
                format!(
                    "{}d {:02}:{:02}:{:02}",
                    up / 86400,
                    up / 3600 % 24,
                    up / 60 % 60,
                    up % 60
                )
            });
        }
    }

    r.section("CPU sensors");
    match &f.cpu_sensors {
        Err(e) => r.missing("PawnIO", e),
        Ok(s) => {
            r.line("Tctl", &s.tctl, |v| format!("{} °C", num(*v, 1)));
            r.line("CCD temperatures", &s.ccds, |v| {
                v.iter()
                    .map(|(n, t)| format!("CCD{n} {} °C", num(*t, 1)))
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            r.line("Package power", &s.package_power, |v| {
                format!("{} W", num(*v, 1))
            });
        }
    }

    r.section("Memory");
    match &f.memory {
        Err(e) => r.missing("collector", e),
        Ok(m) => {
            let used_total = m.used.clone().and_then(|u| Ok((u, m.total.clone()?)));
            r.line("In use / total", &used_total, |(u, t)| {
                format!(
                    "{} / {} ({})",
                    bytes(*u),
                    bytes(*t),
                    pct(*u as f64 / (*t).max(1) as f64 * 100.0)
                )
            });
            r.line("Hardware", &m.hardware, |hw| {
                let mut parts = vec![hw.kind.to_string()];
                if let Some(spd) = hw.speed_mt {
                    parts.push(format!("{spd} MT/s"));
                }
                let slots = match hw.form_factor {
                    Some(ff) => format!("{}/{} slots ({ff})", hw.slots_used, hw.slots_total),
                    None => format!("{}/{} slots", hw.slots_used, hw.slots_total),
                };
                parts.push(slots);
                parts.join(", ")
            });
            r.line("Hardware reserved", &m.hardware_reserved, |v| bytes(*v));
            r.line("Compressed", &m.compressed, |c| {
                if c.compressed > 0 && c.uncompressed > c.compressed {
                    let ratio = c.uncompressed as f64 / c.compressed as f64;
                    format!(
                        "{} (~{} uncompressed, {}x)",
                        bytes(c.compressed),
                        bytes(c.uncompressed),
                        num(ratio, 1)
                    )
                } else {
                    bytes(c.compressed)
                }
            });
            r.line("Committed / limit", &m.commit, |(c, l)| {
                format!("{} / {}", bytes(*c), bytes(*l))
            });
            r.line("Commit peak (boot)", &m.commit_peak, |v| bytes(*v));
            r.line("Cached", &m.cached, |v| bytes(*v));
            r.line("Paged pool", &m.paged_pool, |v| bytes(*v));
            r.line("Non-paged pool", &m.nonpaged_pool, |v| bytes(*v));
            r.line("Page file usage", &m.pagefile_usage, |(u, t)| {
                format!(
                    "{} / {} ({})",
                    bytes(*u),
                    bytes(*t),
                    pct(*u as f64 / (*t).max(1) as f64 * 100.0)
                )
            });
            r.line("Paging (in / out)", &m.paging, |(i, o)| {
                format!("{} / {} pages/s", num(*i, 0), num(*o, 0))
            });
            r.line("Bad RAM pages", &m.bad_pages, |n| {
                if *n > 0 {
                    format!("{n} (hardware fault)")
                } else {
                    "0".into()
                }
            });
        }
    }

    r.section("Disks");
    match &f.disks {
        Err(e) => r.missing("collector", e),
        Ok(list) => {
            for d in list {
                let mut tags = d.letters.join(" ");
                if d.system {
                    tags.push_str(" system");
                }
                if d.pagefile {
                    tags.push_str(" page-file");
                }
                r.subject(&format!("Disk {} {}", d.number, tags.trim()));
                r.line("  Model", &d.model, Clone::clone);
                r.line("  Bus", &d.bus, ToString::to_string);
                r.line("  Type", &d.ssd, |s| if *s { "SSD" } else { "HDD" }.into());
                r.line("  Capacity", &d.capacity, |v| bytes(*v));
                r.line("  Storage Spaces member", &d.pool_member, |m| {
                    if *m { "yes" } else { "no" }.into()
                });
                r.line("  Active time", &d.active, |v| pct(*v));
                r.line("  Read", &d.read, |v| format!("{}/s", bytes(*v as u64)));
                r.line("  Write", &d.write, |v| format!("{}/s", bytes(*v as u64)));
                r.line("  Average response time", &d.response, |v| {
                    format!("{} ms", num(v * 1000.0, 2))
                });
                r.line("  IOPS", &d.iops, |v| num(*v, 0));
                r.line("  Queue depth", &d.queue, ToString::to_string);
            }
        }
    }

    r.section("Network");
    match &f.nics {
        Err(e) => r.missing("collector", e),
        Ok(list) => {
            for n in list {
                r.subject(&format!("{} ({})", n.alias, n.description));
                r.line("  Link", &n.link, |v| bits(*v as f64));
                r.line("  Send", &n.send, |v| bits(*v));
                r.line("  Receive", &n.recv, |v| bits(*v));
                r.line("  Packet errors", &n.errors, |(rx, tx)| {
                    format!("in {rx}, out {tx}")
                });
            }
        }
    }

    r.section("GPU");
    match &f.gpus {
        Err(e) => r.missing("collector", e),
        Ok(list) => {
            for g in list {
                r.subject(&format!("GPU {}", g.index));
                r.line("  Name", &g.name, Clone::clone);
                r.line("  Driver", &g.driver, Clone::clone);
                r.line("  VBIOS", &g.vbios, Clone::clone);
                r.line("  Usage", &g.usage, |v| format!("{v}%"));
                r.line("  Memory activity", &g.mem_activity, |v| format!("{v}%"));
                r.line("  Dedicated VRAM (WDDM)", &g.dedicated, |(u, t)| {
                    format!("{} / {}", bytes(*u), bytes(*t))
                });
                r.line("  Shared memory", &g.shared, |(u, t)| {
                    format!("{} / {}", bytes(*u), bytes(*t))
                });
                r.line("  Physical VRAM (NVML)", &g.physical, |[u, f, res, t]| {
                    format!(
                        "used {}, free {}, reserved {}, total {}",
                        bytes(*u),
                        bytes(*f),
                        bytes(*res),
                        bytes(*t)
                    )
                });
                r.line("  Temperature", &g.temp, |v| format!("{v} °C"));
                r.line("  Slowdown limit", &g.temp_slowdown, |v| format!("{v} °C"));
                let clk = |(c, m): &(u32, u32)| {
                    if *m > 0 {
                        format!("{c} / {m} MHz")
                    } else {
                        format!("{c} MHz (max not reported)")
                    }
                };
                r.line("  Graphics clock", &g.clock_graphics, clk);
                r.line("  SM clock", &g.clock_sm, clk);
                r.line("  Memory clock", &g.clock_mem, clk);
                r.line("  Video clock", &g.clock_video, clk);
                r.line("  Power / limit", &g.power, |(p, l)| {
                    if *l > 0.0 {
                        format!("{} / {} W", num(*p, 1), num(*l, 0))
                    } else {
                        format!("{} W (limit not reported)", num(*p, 1))
                    }
                });
                match &g.fans {
                    Err(e) => r.missing("  Fans", e),
                    Ok(fans) => {
                        for (i, fan) in fans.iter().enumerate() {
                            r.line(&format!("  Fan {i} (%)"), &fan.percent, |v| format!("{v}%"));
                            r.line(&format!("  Fan {i} (RPM)"), &fan.rpm, |v| {
                                format!("{v} RPM")
                            });
                        }
                    }
                }
                r.line(
                    "  PCIe link (gen, width)",
                    &g.pcie_link,
                    |(generation, w)| format!("Gen{generation} x{w}"),
                );
                r.line("  Encoder", &g.encoder, |v| format!("{v}%"));
                r.line("  Decoder", &g.decoder, |v| format!("{v}%"));
                r.line("  Other engine (busiest)", &g.other_engine, |(t, v)| {
                    format!("{} {v}%", crate::win::printable(t))
                });
                r.line("  P-state", &g.pstate, |v| format!("P{v}"));
                r.line("  Throttle reasons", &g.throttle, |v| {
                    if v.is_empty() {
                        "none".into()
                    } else {
                        v.join(", ")
                    }
                });
            }
        }
    }
}

fn slow_report<W: Write>(r: &mut Report<W>, s: &Slow) {
    r.section("Slow thread");
    match &s.gpus {
        Err(e) => r.missing("GPU PCIe", e),
        Ok(list) => {
            for g in list {
                r.line(&format!("GPU {} PCIe TX", g.index), &g.pcie_tx, |v| {
                    format!("{}/s", bytes(*v))
                });
                r.line(&format!("GPU {} PCIe RX", g.index), &g.pcie_rx, |v| {
                    format!("{}/s", bytes(*v))
                });
                let fmt_vram = |v: &crate::model::VramBreakdown| {
                    let parts: Vec<String> = v
                        .segments
                        .iter()
                        .map(|(k, b)| format!("{} {}", k.label(), bytes(*b)))
                        .collect();
                    format!("{} ({} processes)", parts.join(", "), v.processes.len())
                };
                r.line(&format!("GPU {} VRAM by kind", g.index), &g.vram, fmt_vram);
                r.line(
                    &format!("GPU {} shared by kind", g.index),
                    &g.shared,
                    fmt_vram,
                );
                if let Some(p) = &g.procs {
                    r.line(&format!("GPU {} processes", g.index), p, |list| {
                        let busy = list
                            .iter()
                            .filter(|p| p.engines.as_ref().is_ok_and(|e| !e.is_empty()))
                            .count();
                        format!("{} processes, {busy} with engine samples", list.len())
                    });
                    // As engines são uma fonte própria (PDH): falha delas conta como falha.
                    if let Ok(list) = p {
                        let engines: crate::model::Metric<()> = list
                            .iter()
                            .find_map(|x| x.engines.as_ref().err())
                            .map_or(Ok(()), |e| Err(e.clone()));
                        r.line(&format!("GPU {} engines", g.index), &engines, |()| {
                            "per-process engine counters".into()
                        });
                    }
                }
            }
        }
    }
    if let Some(p) = &s.procs {
        r.line("Processes", p, |list| {
            let cpu: f64 = list.iter().map(|p| p.cpu).sum();
            format!("{} processes, CPU sum {:.1}%", list.len(), cpu)
        });
    }
    match &s.disk_temps {
        Err(e) => r.missing("Disk temperature", e),
        Ok(list) => {
            for t in list {
                let source = t.source;
                r.line(&format!("Disk {} temperature", t.number), &t.celsius, |c| {
                    format!("{c} °C ({source})")
                });
                r.line(&format!("Disk {} SMART health", t.number), &t.health, |p| {
                    if *p {
                        "FAILURE PREDICTED".into()
                    } else {
                        "OK".into()
                    }
                });
            }
        }
    }
}

fn fps_report<W: Write>(r: &mut Report<W>, f: &Fps) {
    r.section("FPS (RTSS)");
    r.line("RTSS", &f.rtss, Clone::clone);
    r.line("Frametime pipe", &f.pipe, |(n, c)| {
        format!("{n} instances, {c} clients")
    });
    r.line("ETW frames", &f.etw, ToString::to_string);
    match &f.game {
        Err(e) => r.missing("Game", e),
        Ok(g) => {
            let approx = match g.source {
                crate::model::FrameSource::Approx => " (approx.: game opened before wintop)",
                crate::model::FrameSource::Pipe => " (RTSS pipe)",
                crate::model::FrameSource::Etw => " (ETW)",
            };
            r.subject(&format!(
                "Game: {} (PID {}, {}){approx}",
                g.name, g.pid, g.api
            ));
            r.line("  Resolution", &g.resolution, |(x, y)| format!("{x}x{y}"));
            r.line("  FPS", &g.fps, |v| num(*v, 1));
            if let Some(fg) = &g.framegen {
                let mult = fg.ratio_text().map_or(String::new(), |m| format!(" ({m})"));
                r.line("  Real FPS (frame gen)", &Ok(fg.real_fps), |v| {
                    format!("{}{mult}", num(*v, 1))
                });
            }
            r.line(
                "  Frametime avg / min / max",
                &g.frametime,
                |(a, lo, hi)| format!("{} / {} / {} ms", num(*a, 2), num(*lo, 2), num(*hi, 2)),
            );
            r.line("  1% / 0.1% low", &g.lows, |(a, b)| {
                format!("{} / {} FPS", num(*a, 1), num(*b, 1))
            });
            r.line("  GPU active / GPU frame", &g.gpu_time, |(a, b)| {
                format!("{} / {} ms", num(*a, 2), num(*b, 2))
            });
            r.line("  Limited by", &g.bound, ToString::to_string);
        }
    }
}

fn own_report<W: Write>(r: &mut Report<W>, f: &Fast) {
    r.section("Diagnostics");
    match &f.own {
        Err(e) => r.missing("wintop usage", e),
        Ok(o) => {
            r.line("wintop CPU (whole system)", &o.cpu, |v| {
                format!("{}%", num(*v, 3))
            });
            r.line("wintop memory (private WS)", &o.private_ws, |v| bytes(*v));
            r.line("wintop committed memory", &o.commit, |v| bytes(*v));
        }
    }
}

/// O que o wintop elevado executa ou carrega de outros programas, e a cópia instalada: nada
/// pode ser alterado sem elevação (a mesma regra aplicada antes de usar cada um).
fn security_report<W: Write>(r: &mut Report<W>) {
    r.section("Security (files run or loaded elevated)");
    let check = |p: Option<std::path::PathBuf>, missing: &str| -> Metric<String> {
        match p {
            None => crate::model::neutral(missing),
            Some(p) => crate::install::trusted_file(&p)
                .map(|()| format!("{} (protected)", p.display()))
                .or_else(crate::model::fail),
        }
    };
    let rtss = crate::win::reg_sz_32(r"SOFTWARE\Unwinder\RTSS", "InstallPath")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file());
    r.line("RTSS.exe", &check(rtss, "RTSS not installed"), Clone::clone);
    let pawn = crate::collect::pawnio_dll().filter(|p| p.is_file());
    r.line(
        "PawnIOLib.dll",
        &check(pawn, "PawnIO not installed"),
        Clone::clone,
    );
    let installed = crate::install::install_dir()
        .ok()
        .map(|d| d.join("wintop.exe"))
        .filter(|p| p.is_file());
    r.line(
        "Installed copy",
        &check(installed, "not installed (wintop --install)"),
        Clone::clone,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Counts, Cpu, Memory, Topology, fail, neutral};

    #[test]
    fn texto_json_escapado() {
        assert_eq!(json_str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_str("x\ny\u{1}"), r#""x\u000ay\u0001""#);
        assert_eq!(json_str("°C ok"), "\"°C ok\"");
    }

    #[test]
    fn relatorio_trata_memoria_zero_e_freq_vazia() {
        let fast = Fast {
            cpu: Ok(Cpu {
                name: Ok("Test CPU".into()),
                topology: Ok(Topology {
                    sockets: 1,
                    cores: 4,
                    logical: 8,
                    l1: 256 << 10,
                    l2: 2 << 20,
                    l3: 16 << 20,
                }),
                base_mhz: Ok(3500),
                virtualization: Ok(true),
                usage: Ok(12.5),
                per_logical: Ok(vec![]),
                kernel: Ok(2.0),
                kernel_per_logical: Ok(vec![]),
                freq_mhz: Ok(3500.0),
                freq_per_logical: Ok(vec![]),
                counts: Ok(Counts {
                    processes: 10,
                    threads: 20,
                    handles: 30,
                }),
                uptime: Duration::from_secs(65),
            }),
            cpu_sensors: neutral("no PawnIO"),
            memory: Ok(Memory {
                used: Ok(0),
                total: Ok(0),
                commit: Ok((0, 0)),
                commit_peak: Ok(0),
                compressed: Ok(crate::model::CompressedMemory::default()),
                cached: Ok(0),
                modified: Ok(0),
                standby: Ok(0),
                paged_pool: Ok(0),
                nonpaged_pool: Ok(0),
                pagefile_usage: Ok((0, 0)),
                paging: Ok((0.0, 0.0)),
                hardware: Ok(crate::model::MemoryHardware {
                    kind: "DDR4",
                    speed_mt: Some(3600),
                    slots_used: 4,
                    slots_total: 4,
                    form_factor: Some("DIMM"),
                }),
                hardware_reserved: Ok(0),
                bad_pages: Ok(0),
            }),
            disks: fail("pdh error"),
            nics: Ok(vec![]),
            gpus: Ok(vec![]),
            own: fail("own err"),
            took: Duration::from_millis(5),
        };
        let mut r = Report::new(Vec::<u8>::new(), false);
        fast_report(&mut r, &fast);
        own_report(&mut r, &fast);
        let text = String::from_utf8(r.out).expect("utf8");
        assert!(text.contains("0 logical"), "{text}");
        assert!(!text.contains("NaN") && !text.contains("inf"), "{text}");
        assert!(r.entries.is_empty(), "modo texto não acumula entries JSON");
        assert_eq!(r.failed, 2);
        assert_eq!(r.neutral, 1);

        // Mesmo relatório em JSON preenche `entries` sem escrever texto.
        let mut jr = Report::new(Vec::<u8>::new(), true);
        fast_report(&mut jr, &fast);
        assert!(jr.out.is_empty());
        let json = jr.to_json(true, 5.0, 1.0);
        assert!(json.contains(r#""elevated":true"#), "{json}");
        assert!(
            json.contains(r#""name":"Speed per logical","status":"ok","value":"0 logical""#),
            "{json}"
        );
        assert!(
            json.contains(r#""section":"Disks","subject":"","name":"collector","status":"failed""#),
            "{json}"
        );
    }
}
