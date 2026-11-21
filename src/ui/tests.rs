//! Testes da interface: snapshots do `insta` sobre um terminal de teste, com dados fixos
//! parecidos com os da máquina de desenvolvimento.

use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::view::{self, State};
use crate::model::{
    Bus, Counts, Cpu, CpuSensors, Disk, DiskTemp, Fan, Fast, Fps, Game, Gpu, GpuSlow, Memory,
    Missing, Nic, OwnUsage, Slow, Topology, fail, neutral,
};

const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

fn cpu() -> Cpu {
    Cpu {
        name: Ok("AMD Ryzen 7 5700X3D 8-Core Processor".into()),
        topology: Ok(Topology {
            sockets: 1,
            cores: 8,
            logical: 16,
            l1: 512 << 10,
            l2: 4 * MB,
            l3: 96 * MB,
        }),
        base_mhz: Ok(3001),
        virtualization: Ok(true),
        usage: Ok(23.6),
        per_logical: Ok((0..16).map(|i| f64::from(i * 6 % 100)).collect()),
        kernel: Ok(5.6),
        freq_mhz: Ok(3812.0),
        counts: Ok(Counts {
            processes: 362,
            threads: 7651,
            handles: 182_004,
        }),
        uptime: Duration::from_secs(5 * 3600 + 8 * 60),
    }
}

fn disk(n: u32, model: &str, letters: &[&str], bus: Bus, ssd: bool, member: bool) -> Disk {
    Disk {
        number: n,
        model: Ok(model.into()),
        bus: Ok(bus),
        ssd: Ok(ssd),
        capacity: Ok(477 * GB),
        pool_member: Ok(member),
        letters: letters.iter().map(ToString::to_string).collect(),
        system: letters.contains(&"C:"),
        pagefile: letters.contains(&"C:"),
        active: Ok(f64::from(n) * 3.0),
        read: Ok(f64::from(n) * 1_000_000.0),
        write: Ok(512_000.0),
        response: Ok(0.0004),
    }
}

fn gpu() -> Gpu {
    let clock = |c| Ok((c, 3090));
    Gpu {
        index: 0,
        name: Ok("NVIDIA GeForce RTX 5070 Ti".into()),
        driver: Ok("621.01".into()),
        vbios: Ok("98.03.58.00.96".into()),
        usage: Ok(65),
        mem_activity: Ok(8),
        dedicated: Ok((11 * GB, 16_303 * MB)),
        shared: Ok((3 * GB, 16 * GB)),
        physical: Ok([11 * GB, 4 * GB, 308 * MB, 16_303 * MB]),
        temp: Ok(45),
        temp_slowdown: Ok(90),
        clock_graphics: clock(2767),
        clock_sm: clock(2767),
        clock_mem: Ok((14_001, 14_001)),
        clock_video: clock(2220),
        power: Ok((65.6, 300.0)),
        fans: Ok(vec![
            Fan {
                percent: Ok(34),
                rpm: Ok(1170),
            },
            Fan {
                percent: Ok(0),
                rpm: neutral("stopped"),
            },
        ]),
        pcie_link: Ok((4, 16)),
        encoder: Ok(73),
        decoder: Ok(10),
        pstate: Ok(0),
        throttle: Ok(vec![]),
    }
}

fn fast(step: u32) -> Fast {
    let mut c = cpu();
    c.usage = Ok(f64::from(step * 7 % 100));
    let mut g = gpu();
    g.usage = Ok(step * 11 % 100);
    Fast {
        cpu: Ok(c),
        cpu_sensors: Ok(CpuSensors {
            tctl: Ok(61.0),
            ccds: Ok(vec![(1, 58.25)]),
            package_power: Ok(57.5),
        }),
        memory: Ok(Memory {
            total: Ok(32 * GB),
            used: Ok(21 * GB),
            commit: Ok((40 * GB, 48 * GB)),
            cached: Ok(7 * GB),
            paged_pool: Ok(GB),
            nonpaged_pool: Ok(889 * MB),
            pagefile_usage: Ok(0.8),
        }),
        disks: Ok(vec![
            disk(0, "Netac SSD 512GB", &["E:"], Bus::Sata, true, false),
            disk(1, "HGST HCC545050A7E380", &[], Bus::Sata, false, true),
            disk(7, "NE-512 Por", &["C:"], Bus::Nvme, true, false),
            disk(
                8,
                "Microsoft Storage Space Device",
                &["F:"],
                Bus::Spaces,
                false,
                false,
            ),
        ]),
        nics: Ok(vec![
            Nic {
                alias: "Ethernet".into(),
                description: "Realtek Gaming GbE Family Controller".into(),
                link: neutral("disconnected"),
                send: Ok(0.0),
                recv: Ok(0.0),
            },
            Nic {
                alias: "X540-AT2 NET".into(),
                description: "Intel(R) Ethernet Controller X540-AT2".into(),
                link: Ok(1_000_000_000),
                send: Ok(f64::from(step) * 1000.0),
                recv: Ok(f64::from(step) * 50_000.0),
            },
        ]),
        gpus: Ok(vec![g]),
        own: Ok(OwnUsage {
            cpu: Ok(0.015),
            private_ws: Ok(25 * MB),
            commit: Ok(33 * MB),
        }),
        took: Duration::from_micros(1800),
    }
}

fn slow() -> Slow {
    let t = |n, c, s| DiskTemp {
        number: n,
        celsius: c,
        source: s,
    };
    Slow {
        gpus: Ok(vec![GpuSlow {
            index: 0,
            pcie_tx: Ok(55 * MB),
            pcie_rx: Ok(250 * MB),
        }]),
        disk_temps: Ok(vec![
            t(0, Ok(39), "SMART 194"),
            t(1, neutral("asleep"), ""),
            t(7, Ok(47), "native"),
            t(8, Ok(40), "pool members"),
        ]),
        took: Duration::from_millis(60),
    }
}

fn game() -> Fps {
    Fps {
        rtss: Ok("shared memory 2.21".into()),
        pipe: Ok((8, 1)),
        game: Ok(Game {
            pid: 4242,
            name: "deadlock.exe".into(),
            api: "D3D11",
            resolution: Ok((2560, 1440)),
            approx: false,
            fps: Ok(143.6),
            frametime: Ok((6.96, 5.1, 11.4)),
            lows: Ok((98.0, 61.0)),
            gpu_time: Ok((5.2, 6.8)),
            bound: Ok("GPU"),
        }),
    }
}

fn render(st: &mut State, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal de teste");
    t.draw(|f| view::draw(f, st)).expect("desenho");
    t.backend().to_string()
}

fn filled() -> State {
    let mut st = State::new(1000);
    for i in 0..120 {
        st.push_fast(fast(i));
        st.push_fps(game());
    }
    st.slow = Some(slow());
    st
}

#[test]
fn geral_com_jogo() {
    insta::assert_snapshot!(render(&mut filled(), 160, 48));
}

#[test]
fn geral_terminal_pequeno() {
    // Não pode entrar em pânico nem sair dos limites num terminal apertado.
    insta::assert_snapshot!(render(&mut filled(), 80, 24));
}

#[test]
fn antes_da_primeira_coleta() {
    insta::assert_snapshot!(render(&mut State::new(1000), 100, 30));
}

#[test]
fn falhas_e_estados_neutros() {
    let mut st = State::new(500);
    let mut f = fast(3);
    f.cpu_sensors = neutral("CPU not supported (not AMD)");
    f.gpus = fail("NVML (C:\\Windows\\System32\\nvml.dll): library not found");
    st.push_fast(f);
    st.push_fps(Fps {
        rtss: Err(Missing::Fail("RTSS not running".into())),
        pipe: Ok((8, 0)),
        game: Err(Missing::Fail("RTSS not running".into())),
    });
    insta::assert_snapshot!(render(&mut st, 140, 40));
}

#[test]
fn terminal_minusculo_nao_quebra() {
    let mut st = filled();
    for (w, h) in [(1, 1), (10, 3), (30, 8), (200, 5)] {
        let _ = render(&mut st, w, h);
    }
}
