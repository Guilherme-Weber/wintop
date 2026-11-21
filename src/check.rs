//! `wintop --check`: coleta de aquecimento, espera ~300 ms, coleta de novo (contadores de
//! taxa) e imprime o estado de cada métrica. Código de saída 1 se alguma métrica falhou.

use std::time::Duration;

use crate::collect::{FastCollectors, RtssCollector, SlowCollectors};
use crate::fmt::{bits, bytes, num, pct};
use crate::model::{Fast, Fps, Metric, Missing, Slow};

const WARMUP: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Report {
    ok: u32,
    neutral: u32,
    failed: u32,
}

impl Report {
    fn section(title: &str) {
        println!("\n{title}");
    }

    fn line<T>(&mut self, label: &str, m: &Metric<T>, show: impl FnOnce(&T) -> String) {
        let (tag, text) = match m {
            Ok(v) => {
                self.ok += 1;
                ("OK    ", show(v))
            }
            Err(Missing::Neutral(s)) => {
                self.neutral += 1;
                ("N/A   ", s.clone())
            }
            Err(Missing::Fail(s)) => {
                self.failed += 1;
                ("FAIL  ", s.clone())
            }
        };
        println!("  {tag}  {label:<34} {text}");
    }
}

pub fn run() -> i32 {
    println!(
        "wintop {} --check (elevated: {})",
        env!("CARGO_PKG_VERSION"),
        if crate::win::is_elevated() {
            "yes"
        } else {
            "no"
        }
    );
    let nvml = crate::collect::shared_nvml();
    let mut fast = FastCollectors::new(nvml.clone());
    let mut slow = SlowCollectors::new(nvml);
    let _ = fast.collect();
    std::thread::sleep(WARMUP);
    let f = fast.collect();
    let s = slow.collect(&std::sync::atomic::AtomicBool::new(false));
    // Sem iniciar o RTSS nem criar o pipe: o --check só diagnostica.
    let fps = RtssCollector::new(false, false).sample();

    let mut r = Report::default();
    fast_report(&mut r, &f);
    slow_report(&mut r, &s);
    fps_report(&mut r, &fps);
    own_report(&mut r, &f);
    println!(
        "\nFast collection: {:.1} ms; slow: {:.1} ms",
        f.took.as_secs_f64() * 1e3,
        s.took.as_secs_f64() * 1e3
    );
    println!(
        "Result: {} OK, {} neutral, {} failed",
        r.ok, r.neutral, r.failed
    );
    i32::from(r.failed > 0)
}

fn fast_report(r: &mut Report, f: &Fast) {
    Report::section("CPU");
    match &f.cpu {
        Err(e) => r.line("collector", &Err::<(), _>(e.clone()), |()| String::new()),
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
            r.line("Current speed", &c.freq_mhz, |v| format!("{v:.0} MHz"));
            r.line("Processes / threads / handles", &c.counts, |n| {
                format!("{} / {} / {}", n.processes, n.threads, n.handles)
            });
            let up = c.uptime.as_secs();
            println!(
                "  OK      {:<34} {}d {:02}:{:02}:{:02}",
                "Up time",
                up / 86400,
                up / 3600 % 24,
                up / 60 % 60,
                up % 60
            );
            r.ok += 1;
        }
    }

    Report::section("CPU sensors");
    match &f.cpu_sensors {
        Err(e) => r.line("PawnIO", &Err::<(), _>(e.clone()), |()| String::new()),
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

    Report::section("Memory");
    match &f.memory {
        Err(e) => r.line("collector", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(m) => {
            let used_total = m.used.clone().and_then(|u| Ok((u, m.total.clone()?)));
            r.line("In use / total", &used_total, |(u, t)| {
                format!(
                    "{} / {} ({})",
                    bytes(*u),
                    bytes(*t),
                    pct(*u as f64 / *t as f64 * 100.0)
                )
            });
            r.line("Committed / limit", &m.commit, |(c, l)| {
                format!("{} / {}", bytes(*c), bytes(*l))
            });
            r.line("Cached", &m.cached, |v| bytes(*v));
            r.line("Paged pool", &m.paged_pool, |v| bytes(*v));
            r.line("Non-paged pool", &m.nonpaged_pool, |v| bytes(*v));
            r.line("Page file usage", &m.pagefile_usage, |v| pct(*v));
        }
    }

    Report::section("Disks");
    match &f.disks {
        Err(e) => r.line("collector", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(list) => {
            for d in list {
                let mut tags = d.letters.join(" ");
                if d.system {
                    tags.push_str(" system");
                }
                if d.pagefile {
                    tags.push_str(" page-file");
                }
                println!("  Disk {} {}", d.number, tags.trim());
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
            }
        }
    }

    Report::section("Network");
    match &f.nics {
        Err(e) => r.line("collector", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(list) => {
            for n in list {
                println!("  {} ({})", n.alias, n.description);
                r.line("  Link", &n.link, |v| bits(*v as f64));
                r.line("  Send", &n.send, |v| bits(*v));
                r.line("  Receive", &n.recv, |v| bits(*v));
            }
        }
    }

    Report::section("GPU");
    match &f.gpus {
        Err(e) => r.line("collector", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(list) => {
            for g in list {
                println!("  GPU {}", g.index);
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
                let clk = |(c, m): &(u32, u32)| format!("{c} / {m} MHz");
                r.line("  Graphics clock", &g.clock_graphics, clk);
                r.line("  SM clock", &g.clock_sm, clk);
                r.line("  Memory clock", &g.clock_mem, clk);
                r.line("  Video clock", &g.clock_video, clk);
                r.line("  Power / limit", &g.power, |(p, l)| {
                    format!("{} / {} W", num(*p, 1), num(*l, 0))
                });
                match &g.fans {
                    Err(e) => r.line("  Fans", &Err::<(), _>(e.clone()), |()| String::new()),
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

fn slow_report(r: &mut Report, s: &Slow) {
    Report::section("Slow thread");
    match &s.gpus {
        Err(e) => r.line("GPU PCIe", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(list) => {
            for g in list {
                r.line(&format!("GPU {} PCIe TX", g.index), &g.pcie_tx, |v| {
                    format!("{}/s", bytes(*v))
                });
                r.line(&format!("GPU {} PCIe RX", g.index), &g.pcie_rx, |v| {
                    format!("{}/s", bytes(*v))
                });
            }
        }
    }
    match &s.disk_temps {
        Err(e) => r.line("Disk temperature", &Err::<(), _>(e.clone()), |()| {
            String::new()
        }),
        Ok(list) => {
            for t in list {
                let source = t.source;
                r.line(&format!("Disk {} temperature", t.number), &t.celsius, |c| {
                    format!("{c} °C ({source})")
                });
            }
        }
    }
}

fn fps_report(r: &mut Report, f: &Fps) {
    Report::section("FPS (RTSS)");
    r.line("RTSS", &f.rtss, Clone::clone);
    r.line("Frametime pipe", &f.pipe, |(n, c)| {
        format!("{n} instances, {c} clients")
    });
    match &f.game {
        Err(e) => r.line("Game", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(g) => {
            let approx = if g.approx {
                " (approx.: game opened before wintop)"
            } else {
                ""
            };
            println!("  Game: {} (PID {}, {}){approx}", g.name, g.pid, g.api);
            r.line("  Resolution", &g.resolution, |(x, y)| format!("{x}x{y}"));
            r.line("  FPS", &g.fps, |v| num(*v, 1));
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

fn own_report(r: &mut Report, f: &Fast) {
    Report::section("Diagnostics");
    match &f.own {
        Err(e) => r.line("wintop usage", &Err::<(), _>(e.clone()), |()| String::new()),
        Ok(o) => {
            r.line("wintop CPU (whole system)", &o.cpu, |v| {
                format!("{}%", num(*v, 3))
            });
            r.line("wintop memory (private WS)", &o.private_ws, |v| bytes(*v));
            r.line("wintop committed memory", &o.commit, |v| bytes(*v));
        }
    }
}
