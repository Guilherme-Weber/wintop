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
        kernel_per_logical: Ok((0..16).map(|i| f64::from(i * 2 % 30)).collect()),
        kernel: Ok(5.6),
        freq_mhz: Ok(3812.0),
        freq_per_logical: Ok((0..16).map(|i| 3600.0 + f64::from(i) * 60.0).collect()),
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
        iops: Ok(f64::from(n) * 40.0),
        queue: Ok(n % 3),
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

pub(super) fn fast(step: u32) -> Fast {
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
            standby: Ok(6 * GB),
            modified: Ok(GB),
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
                errors: Ok((0, 0)),
            },
            Nic {
                alias: "X540-AT2 NET".into(),
                description: "Intel(R) Ethernet Controller X540-AT2".into(),
                link: Ok(1_000_000_000),
                send: Ok(f64::from(step) * 1000.0),
                recv: Ok(f64::from(step) * 50_000.0),
                errors: Ok((0, 0)),
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
    let model = |n| match n {
        0 => "Netac SSD 512GB",
        1 => "HGST HCC545050A7E380",
        7 => "NE-512 Por",
        _ => "Microsoft Storage Space Device",
    };
    let t = |n, c, s| DiskTemp {
        number: n,
        model: Some(model(n).to_string()),
        celsius: c,
        source: s,
        health: Ok(false),
    };
    Slow {
        gpus: Ok(vec![GpuSlow {
            index: 0,
            pcie_tx: Ok(55 * MB),
            pcie_rx: Ok(250 * MB),
            vram: Ok(crate::model::VramBreakdown {
                total: 16_303 * MB,
                used: 11 * GB,
                segments: vec![
                    (crate::model::VramKind::App, 2 * GB),
                    (crate::model::VramKind::Compositor, 859 * MB),
                    (crate::model::VramKind::Browser, 638 * MB),
                    (crate::model::VramKind::System, 105 * MB),
                    (crate::model::VramKind::Kernel, 4 * MB),
                    (crate::model::VramKind::Free, 5 * GB),
                ],
                processes: vec![],
            }),
            procs: None,
        }]),
        disk_temps: Ok(vec![
            t(0, Ok(39), "SMART 194"),
            t(1, neutral("asleep"), ""),
            t(7, Ok(47), "native"),
            t(8, Ok(40), "pool members"),
        ]),
        procs: None,
        took: Duration::from_millis(60),
    }
}

pub(super) fn game() -> Fps {
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
            peak_ms: Some(11.4),
        }),
        hooked: vec![4242],
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
fn geral_conhost_120x30() {
    // Tamanho padrão do console clássico, onde o atalho sem UAC abre o wintop.
    let mut st = filled();
    st.clock = "10:33:28".into();
    insta::assert_snapshot!(render(&mut st, 120, 30));
}

#[test]
fn geral_1080p() {
    // Tamanho da janela do usuário: Windows Terminal maximizado num monitor 1080p.
    let mut st = filled();
    st.clock = "10:33:28".into();
    insta::assert_snapshot!(render(&mut st, 210, 52));
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
        hooked: vec![],
    });
    insta::assert_snapshot!(render(&mut st, 140, 40));
}

/// As bordas de cada caixa ficam intactas: nada desenhado por cima delas.
fn bordas_intactas(st: &mut State, w: u16, h: u16) {
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal de teste");
    t.draw(|f| view::draw(f, st)).expect("desenho");
    let buf = t.backend().buffer();
    let at = |x: u16, y: u16| buf[(x, y)].symbol().to_string();
    let screen = || {
        (0..h)
            .map(|y| (0..w).map(|x| at(x, y)).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    for y in 0..h {
        for x in 0..w {
            let s = at(x, y);
            assert!(!s.chars().any(char::is_control), "({x},{y}) {s:?}");
            // Laterais intactas: abaixo de cada canto de cima, só `│` até o canto de baixo do
            // mesmo lado (um texto por cima da borda quebraria a coluna).
            let bottom = match s.as_str() {
                "╭" => "╰",
                "╮" => "╯",
                _ => continue,
            };
            let mut yy = y + 1;
            while yy < h && at(x, yy) == "│" {
                yy += 1;
            }
            assert!(
                yy < h && at(x, yy) == bottom,
                "borda quebrada abaixo de ({x},{y}) em {w}x{h}: {:?}\n{}",
                if yy < h { at(x, yy) } else { "fim".into() },
                screen()
            );
        }
    }
}

#[test]
fn larguras_estreitas_sem_vazamento() {
    let mut st = filled();
    // A borda direita da caixa da CPU continua sendo borda em toda a altura (a linha 0 é a
    // barra de abas; a caixa começa na 1).
    let mut t = Terminal::new(TestBackend::new(60, 24)).expect("terminal de teste");
    t.draw(|f| view::draw(f, &mut st)).expect("desenho");
    let buf = t.backend().buffer();
    let top = 1u16;
    let cpu_right = (0..60u16)
        .find(|&x| buf[(x, top)].symbol() == "╮")
        .expect("canto da caixa da CPU");
    let bottom = (top + 1..24u16)
        .find(|&y| buf[(cpu_right, y)].symbol() == "╯")
        .expect("canto inferior da caixa da CPU");
    for y in top + 1..bottom {
        assert_eq!(buf[(cpu_right, y)].symbol(), "│", "linha {y}");
    }
    bordas_intactas(&mut st, 60, 24);
}

#[test]
fn terminal_minusculo_nao_quebra() {
    let mut st = filled();
    // Inclui larguras em que a grade de lógicos, as barras e a rede não cabem (achado da
    // revisão: escrita fora do buffer e por cima das caixas vizinhas).
    for (w, h) in [
        (3, 0),
        (1, 1),
        (10, 3),
        (100, 30),
        (120, 40),
        (140, 40),
        (30, 8),
        (200, 5),
        (30, 20),
        (40, 20),
        (45, 30),
        (60, 24),
        (68, 30),
        (79, 40),
        (300, 100),
    ] {
        let _ = render(&mut st, w, h);
    }
}

#[test]
fn falha_de_rede_nao_vira_desconectada() {
    let mut st = State::new(1000);
    let mut f = fast(5);
    if let Ok(list) = &mut f.nics {
        list[1].link = fail("link speed unknown");
    }
    st.push_fast(f);
    let out = render(&mut st, 160, 48);
    assert!(out.contains("! link speed unknown"), "{out}");
    assert!(!out.contains("disconnected: Ethernet, X540"), "{out}");
}

#[test]
fn temperatura_de_outro_disco_nao_aparece() {
    let mut st = filled();
    // Outro disco ocupou o número 7 (hot-plug): a temperatura lida do modelo antigo não vale.
    if let Some(s) = &mut st.slow
        && let Ok(list) = &mut s.disk_temps
    {
        for t in list.iter_mut().filter(|t| t.number == 7) {
            t.model = Some("Outro disco".into());
        }
    }
    let out = render(&mut st, 160, 48);
    assert!(!out.contains("47 °C"), "{out}");
    assert!(out.contains("39 °C"), "{out}");
}

#[test]
fn falha_do_pipe_com_jogo_aparece() {
    let mut st = State::new(1000);
    st.push_fast(fast(2));
    let mut s = game();
    s.pipe = fail("frametime pipe (another server running?): busy");
    if let Ok(g) = &mut s.game {
        g.approx = true;
    }
    st.push_fps(s);
    let out = render(&mut st, 160, 48);
    assert!(out.contains("approx. (frametime pipe failed)"), "{out}");
    assert!(out.contains("! frametime pipe (another server"), "{out}");
    assert!(!out.contains("game opened before wintop"), "{out}");
}

#[test]
fn troca_de_pid_do_jogo_nao_zera_o_historico() {
    // Jogos trocam de executável (launcher, tela de carregamento): só o `r` zera.
    let mut st = State::new(1000);
    let with = |pid: u32, fps: f64| {
        let mut s = game();
        if let Ok(g) = &mut s.game {
            g.pid = pid;
            g.fps = Ok(fps);
        }
        s
    };
    st.push_fps(with(1, 100.0));
    st.push_fast(fast(0));
    st.push_fps(with(2, 200.0));
    st.push_fast(fast(1));
    st.slow = Some(slow());
    let tela = render(&mut st, 160, 48);
    assert!(tela.contains("FPS 200  min 100 · max 200"), "{tela}");
    st.reset_session();
    let tela = render(&mut st, 160, 48);
    assert!(!tela.contains("min 100"), "{tela}");
}

#[test]
fn linhas_de_detalhe_nao_cortam_nada() {
    // As linhas quebram em vez de cortar (o "free" da VRAM era o que sumia).
    for (w, h) in [(160, 48), (210, 52), (240, 64)] {
        let out = render(&mut filled(), w, h);
        for want in [
            "free 5.0G",
            "kernel 4.0M",
            "VBIOS 98.03.58.00.96",
            "(NVML)",
            "video 2220/3090 MHz",
            "182004 handles",
            "frame 6.80 ms",
            "1000 ms tick",
        ] {
            assert!(out.contains(want), "{w}x{h} sem {want:?}:\n{out}");
        }
    }
}

#[test]
fn falha_de_temperatura_mostra_a_causa_inteira() {
    let mut st = filled();
    let cause = "SMART: IOCTL 0x2D1100: error 0x1F";
    if let Some(s) = &mut st.slow
        && let Ok(list) = &mut s.disk_temps
    {
        for t in list.iter_mut().filter(|t| t.number == 0) {
            t.celsius = Err(Missing::Fail(cause.into()));
        }
    }
    let out = render(&mut st, 160, 48);
    assert!(out.contains("! failed"), "{out}");
    assert!(out.contains(&format!("! E: temperature: {cause}")), "{out}");
}

#[test]
fn sem_jogo_fica_idle_com_os_graficos() {
    let mut st = filled();
    st.push_fps(Fps {
        rtss: Ok("shared memory 2.21".into()),
        pipe: Ok((8, 1)),
        game: Err(Missing::Neutral("no 3D game in focus".into())),
        hooked: vec![],
    });
    st.push_fast(fast(200));
    let out = render(&mut st, 160, 48);
    assert!(out.contains("game · idle"), "{out}");
    assert!(out.contains("FPS idle"), "{out}");
    // O histórico continua: mínimo/máximo e o gráfico do último jogo.
    assert!(out.contains("min 144 · max 144"), "{out}");
    assert!(out.contains("no 3D game in focus"), "{out}");
}

#[test]
fn causa_longa_de_falha_quebra_sem_cortar() {
    // Causa que cabe sozinha na linha, mas não junto do rótulo (achado da revisão).
    let mut st = filled();
    let cause = "SMART: no temperature sensor (native or SMART 194/190)";
    if let Some(s) = &mut st.slow
        && let Ok(list) = &mut s.disk_temps
    {
        for t in list.iter_mut().filter(|t| t.number == 0) {
            t.celsius = Err(Missing::Fail(cause.into()));
        }
    }
    for (w, h) in [(140, 48), (160, 48), (210, 52)] {
        let out = render(&mut st, w, h);
        let flat: String = out.split('\n').collect::<Vec<_>>().join(" ");
        assert!(flat.contains("194/190)"), "{w}x{h}:\n{out}");
        assert!(flat.contains("! E: temperature: SMART:"), "{out}");
    }
}

// --- Abas Processos, GPU e Sensores ---

fn proc(pid: u32, name: &str, cpu: f64, mem_mb: u64) -> crate::model::Proc {
    crate::model::Proc {
        pid,
        parent: 4,
        name: name.into(),
        create: u64::from(pid) * 1000,
        cpu,
        mem: mem_mb * MB,
        commit: mem_mb * 2 * MB,
        threads: pid % 40 + 1,
        handles: pid % 900 + 10,
        io_read: f64::from(pid % 7) * 100_000.0,
        io_write: 0.0,
    }
}

fn proc_list() -> Vec<crate::model::Proc> {
    vec![
        proc(4, "System", 0.4, 0),
        proc(4242, "deadlock.exe", 31.2, 6100),
        proc(1784, "dwm.exe", 2.1, 180),
        proc(9120, "firefox.exe", 4.7, 1450),
        proc(2244, "Code.exe", 1.3, 820),
        proc(
            15000,
            "um nome de processo bem comprido que não cabe.exe",
            0.0,
            3,
        ),
    ]
}

fn gpu_list() -> Vec<crate::model::GpuProc> {
    let p = |pid, name: &str, local, engines: Vec<(&str, f64)>| crate::model::GpuProc {
        pid,
        name: name.into(),
        local,
        non_local: local / 20,
        committed: local + local / 10,
        engines: Ok(engines
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()),
    };
    vec![
        p(1784, "dwm.exe", 546 * MB, vec![("3D", 3.0), ("Copy", 0.4)]),
        p(
            4242,
            "deadlock.exe",
            7 * GB,
            vec![("3D", 92.5), ("Copy", 4.0), ("VideoDecode", 0.0)],
        ),
        p(9120, "firefox.exe", 855 * MB, vec![("VideoDecode", 12.0)]),
        p(7000, "ollama.exe", 0, vec![("Compute_0", 0.0)]),
    ]
}

fn tab_state(tab: view::Tab) -> State {
    let mut st = filled();
    st.set_tab(tab);
    let mut s = slow();
    s.procs = Some(Ok(proc_list()));
    if let Ok(g) = &mut s.gpus {
        g[0].procs = Some(Ok(gpu_list()));
    }
    st.push_slow(s);
    st
}

#[test]
fn aba_processos() {
    let mut st = tab_state(view::Tab::Processes);
    insta::assert_snapshot!(render(&mut st, 140, 16));
}

#[test]
fn aba_gpu() {
    let mut st = tab_state(view::Tab::Gpu);
    st.runtimes
        .insert(1784, ("dwm.exe".into(), Ok(vec!["D3D11", "D3D12"])));
    insta::assert_snapshot!(render(&mut st, 160, 20));
}

#[test]
fn aba_sensores() {
    let mut st = tab_state(view::Tab::Sensors);
    insta::assert_snapshot!(render(&mut st, 160, 30));
}

#[test]
fn sensores_em_colunas_quando_nao_cabem() {
    let mut st = tab_state(view::Tab::Sensors);
    let out = render(&mut st, 160, 16);
    // Os grupos vão para a coluna ao lado em vez de sumir.
    for g in ["CPU", "GPU 0", "Disks", "Tctl", "Fan 2 RPM"] {
        assert!(out.contains(g), "{g}\n{out}");
    }
}

#[test]
fn abas_novas_sem_vazamento_em_qualquer_tamanho() {
    for tab in [view::Tab::Processes, view::Tab::Gpu, view::Tab::Sensors] {
        let mut st = tab_state(tab);
        for (w, h) in [
            (1, 1),
            (10, 3),
            (30, 8),
            (45, 12),
            (60, 24),
            (100, 30),
            (300, 100),
        ] {
            let _ = render(&mut st, w, h);
        }
        bordas_intactas(&mut st, 60, 24);
    }
}

#[test]
fn ordem_e_filtro_dos_processos() {
    let mut st = tab_state(view::Tab::Processes);
    let out = render(&mut st, 140, 16);
    let pos = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s}\n{out}"));
    // CPU decrescente por padrão.
    assert!(pos("deadlock.exe") < pos("firefox.exe"));
    assert!(pos("firefox.exe") < pos("dwm.exe"));
    // Por nome (←), crescente.
    st.proc_sort = view::ProcSort::Name;
    let out = render(&mut st, 140, 16);
    let pos = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s}\n{out}"));
    assert!(pos("Code.exe") < pos("deadlock.exe"));
    // Filtro por parte do nome, sem diferenciar maiúsculas.
    st.filter = "FIRE".into();
    let out = render(&mut st, 140, 16);
    assert!(
        out.contains("firefox.exe") && !out.contains("dwm.exe"),
        "{out}"
    );
    assert!(out.contains("1 of 6"), "{out}");
}

#[test]
fn selecao_fica_na_linha_entre_coletas() {
    let mut st = tab_state(view::Tab::Processes);
    let _ = render(&mut st, 140, 16);
    st.proc_cursor.down(1);
    let _ = render(&mut st, 140, 16);
    // 2º por CPU = firefox.
    assert_eq!(st.proc_cursor.pid(), Some(9120));
    // Nova coleta muda a ordem: a seleção fica na 2ª linha (a lista não pula atrás do
    // processo), agora com quem ocupa essa posição.
    let mut list = proc_list();
    list[3].cpu = 90.0;
    let mut s = slow();
    s.procs = Some(Ok(list.clone()));
    st.push_slow(s);
    let _ = render(&mut st, 140, 16);
    let mut by_cpu = list;
    by_cpu.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    assert_eq!(st.proc_cursor.pid(), Some(by_cpu[1].pid));
    assert_ne!(st.proc_cursor.pid(), Some(9120));
    st.proc_cursor.end();
    let _ = render(&mut st, 140, 16);
    assert_eq!(st.proc_cursor.pid(), Some(15000));
}

#[test]
fn troca_de_aba_descarta_a_lista_velha() {
    let mut st = tab_state(view::Tab::Processes);
    st.set_tab(view::Tab::Overview);
    st.set_tab(view::Tab::Processes);
    let out = render(&mut st, 140, 16);
    assert!(out.contains("waiting for the first collection"), "{out}");
}

#[test]
fn protegidos_incluem_jogo_e_apps_com_hook() {
    let mut st = filled();
    let mut f = game();
    f.hooked = vec![1784, 4242];
    st.push_fps(f);
    let p = st.protected_pids().expect("RTSS rodando");
    assert!(p.contains(&1784) && p.contains(&4242), "{p:?}");
}

#[test]
fn sem_lista_do_rtss_nenhum_processo_e_aberto() {
    let mut st = tab_state(view::Tab::Gpu);
    // RTSS fechado: não há como saber qual processo é jogo.
    st.push_fps(Fps {
        rtss: fail("RTSS not running"),
        pipe: Ok((8, 0)),
        game: fail("RTSS not running"),
        hooked: vec![],
    });
    assert_eq!(st.protected_pids(), None);
    let _ = render(&mut st, 160, 20);
    assert!(
        st.request_runtimes().is_none(),
        "nenhuma leitura sem a lista do RTSS"
    );
    let (_, r) = st.runtimes.values().next().expect("resultado");
    assert!(
        matches!(r, Err(Missing::Neutral(s)) if s.contains("waiting for RTSS")),
        "{r:?}"
    );
}

#[test]
fn enter_usa_a_selecao_pendente() {
    let mut st = tab_state(view::Tab::Gpu);
    let _ = render(&mut st, 160, 20);
    // ↓ e Enter no mesmo lote de teclas (sem desenho no meio): vale o 2º da lista.
    st.gpu_cursor.down(1);
    let job = st.request_runtimes().expect("pedido de leitura");
    assert_eq!(job.pid, 9120);
    assert_eq!(
        job.create,
        Some(9120 * 1000),
        "identidade pelo instante de criação"
    );
    // 2º por uso de GPU = firefox (12% no decoder); ele não tem o hook no fixture.
    assert!(st.runtimes.contains_key(&9120), "{:?}", st.runtimes.keys());
}

#[test]
fn causa_longa_nos_sensores_quebra_sem_cortar() {
    let mut st = filled();
    let mut f = fast(3);
    f.cpu_sensors = fail(
        "PawnIO: module AMDFamily17.bin could not be loaded from the modules folder next to wintop.exe (error 0x80070002)",
    );
    st.push_fast(f);
    st.set_tab(view::Tab::Sensors);
    let out = render(&mut st, 100, 40);
    let flat: String = out
        .lines()
        .map(|l| l.trim_matches('"').trim_matches('│').trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(flat.contains("(error 0x80070002)"), "{out}");
    assert!(!out.contains('…'), "{out}");
}

#[test]
fn rodape_nao_corta_atalho_no_meio() {
    for (tab, w) in [
        (view::Tab::Processes, 140),
        (view::Tab::Overview, 120),
        (view::Tab::Gpu, 100),
    ] {
        let mut st = tab_state(tab);
        let out = render(&mut st, w, 30);
        let footer = out.lines().last().expect("rodapé");
        assert!(!footer.contains("interwintop"), "{footer}");
        // Entre a ajuda e o estado há pelo menos 2 espaços.
        let i = footer.find("wintop ").expect("estado");
        assert!(footer[..i].ends_with("  "), "{footer}");
    }
}

#[test]
fn sensores_em_tela_larga_nao_cortam_nomes() {
    let mut st = tab_state(view::Tab::Sensors);
    for w in [200, 216, 230, 247, 300] {
        let out = render(&mut st, w, 60);
        assert!(
            out.contains("Disk 8 Microsoft Storage Space Device"),
            "{w}
{out}"
        );
    }
}

#[test]
fn sensores_com_texto_longo_e_causas_medias_nao_somem() {
    let mut st = filled();
    let mut f = fast(5);
    if let Ok(g) = &mut f.gpus {
        g[0].throttle = Ok(vec!["power cap", "HW slowdown", "SW thermal", "sync boost"]);
        g[0].temp_slowdown = fail("not supported by this GPU in NVML (0x3)");
    }
    st.push_fast(f);
    st.set_tab(view::Tab::Sensors);
    for (w, h) in [(158, 30), (120, 24), (90, 40), (200, 20), (60, 60)] {
        let out = render(&mut st, w, h);
        let flat: String = out
            .lines()
            .map(|l| l.trim_matches('"').trim_matches('│').trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!out.contains('…'), "{w}x{h}\n{out}");
        let complete = flat.contains("sync boost") && flat.contains("(0x3)");
        // Sem altura para tudo, o aviso diz quantos ficaram de fora.
        assert!(
            complete || out.contains("more (taller terminal)"),
            "{w}x{h}\n{out}"
        );
    }
}

#[test]
fn disco_removido_some_da_aba_sensores() {
    let mut st = tab_state(view::Tab::Sensors);
    let mut f = fast(7);
    if let Ok(d) = &mut f.disks {
        d.retain(|d| d.number != 7);
    }
    st.push_fast(f);
    let out = render(&mut st, 160, 40);
    assert!(!out.contains("Disk 7"), "{out}");
    assert!(out.contains("Disk 0"), "{out}");
}

// --- Invariante: toda falha contada no rodapé aparece em vermelho, inteira, em alguma aba ---

/// Uma alteração que põe uma falha com a causa `tag` numa métrica.
type Inject = Box<dyn Fn(&mut Fast, &mut Slow, &mut Fps, &str)>;

fn injections() -> Vec<(&'static str, Inject)> {
    fn f<T>(tag: &str) -> crate::model::Metric<T> {
        fail(format!("cause {tag} of the {}", tag.replace("TAG", "END")))
    }
    let cpu = |g: fn(&mut Cpu, &str)| -> Inject {
        Box::new(move |fa, _, _, t| {
            if let Ok(c) = &mut fa.cpu {
                g(c, t);
            }
        })
    };
    let gpu = |g: fn(&mut Gpu, &str)| -> Inject {
        Box::new(move |fa, _, _, t| {
            if let Ok(l) = &mut fa.gpus {
                g(&mut l[0], t);
            }
        })
    };
    let mem = |g: fn(&mut Memory, &str)| -> Inject {
        Box::new(move |fa, _, _, t| {
            if let Ok(m) = &mut fa.memory {
                g(m, t);
            }
        })
    };
    let disk = |g: fn(&mut Disk, &str)| -> Inject {
        Box::new(move |fa, _, _, t| {
            if let Ok(l) = &mut fa.disks {
                g(&mut l[0], t);
            }
        })
    };
    let game = |g: fn(&mut Game, &str)| -> Inject {
        Box::new(move |_, _, fp, t| {
            if let Ok(x) = &mut fp.game {
                g(x, t);
            }
        })
    };
    vec![
        ("cpu.name", cpu(|c, t| c.name = f(t))),
        ("cpu.topology", cpu(|c, t| c.topology = f(t))),
        ("cpu.usage", cpu(|c, t| c.usage = f(t))),
        ("cpu.kernel", cpu(|c, t| c.kernel = f(t))),
        (
            "cpu.usage+kernel",
            cpu(|c, t| {
                c.usage = f(t);
                c.kernel = f(t);
            }),
        ),
        ("cpu.per_logical", cpu(|c, t| c.per_logical = f(t))),
        (
            "cpu.freq_per_logical",
            cpu(|c, t| c.freq_per_logical = f(t)),
        ),
        (
            "sensors.ccds",
            Box::new(|fa, _, _, t| {
                if let Ok(s) = &mut fa.cpu_sensors {
                    s.ccds = f(t);
                }
            }),
        ),
        ("cpu.base", cpu(|c, t| c.base_mhz = f(t))),
        ("cpu.virt", cpu(|c, t| c.virtualization = f(t))),
        ("cpu.freq", cpu(|c, t| c.freq_mhz = f(t))),
        ("cpu.counts", cpu(|c, t| c.counts = f(t))),
        ("cpu.kernel_per", cpu(|c, t| c.kernel_per_logical = f(t))),
        (
            "sensors.tctl",
            Box::new(|fa, _, _, t| {
                if let Ok(s) = &mut fa.cpu_sensors {
                    s.tctl = f(t);
                }
            }),
        ),
        (
            "sensors.power",
            Box::new(|fa, _, _, t| {
                if let Ok(s) = &mut fa.cpu_sensors {
                    s.package_power = f(t);
                }
            }),
        ),
        ("mem.commit", mem(|m, t| m.commit = f(t))),
        ("mem.used", mem(|m, t| m.used = f(t))),
        ("mem.total", mem(|m, t| m.total = f(t))),
        ("mem.standby", mem(|m, t| m.standby = f(t))),
        ("mem.nonpaged", mem(|m, t| m.nonpaged_pool = f(t))),
        ("mem.paged", mem(|m, t| m.paged_pool = f(t))),
        ("mem.pagefile", mem(|m, t| m.pagefile_usage = f(t))),
        ("disk.model", disk(|d, t| d.model = f(t))),
        ("disk.capacity", disk(|d, t| d.capacity = f(t))),
        (
            "disk.io",
            disk(|d, t| {
                d.active = f(t);
                d.read = f(t);
                d.write = f(t);
                d.response = f(t);
                d.iops = f(t);
                d.queue = f(t);
            }),
        ),
        (
            "nic.send",
            Box::new(|fa, _, _, t| {
                if let Ok(l) = &mut fa.nics {
                    l[1].send = f(t);
                }
            }),
        ),
        (
            "nic.link",
            Box::new(|fa, _, _, t| {
                if let Ok(l) = &mut fa.nics {
                    l[0].link = f(t);
                }
            }),
        ),
        (
            "nic.errors",
            Box::new(|fa, _, _, t| {
                if let Ok(l) = &mut fa.nics {
                    l[1].errors = f(t);
                }
            }),
        ),
        (
            "nic.recv",
            Box::new(|fa, _, _, t| {
                if let Ok(l) = &mut fa.nics {
                    l[0].recv = f(t);
                }
            }),
        ),
        ("gpu.name", gpu(|g, t| g.name = f(t))),
        ("gpu.usage", gpu(|g, t| g.usage = f(t))),
        ("gpu.dedicated", gpu(|g, t| g.dedicated = f(t))),
        ("gpu.shared", gpu(|g, t| g.shared = f(t))),
        ("gpu.temp", gpu(|g, t| g.temp = f(t))),
        ("gpu.power", gpu(|g, t| g.power = f(t))),
        ("gpu.clock_graphics", gpu(|g, t| g.clock_graphics = f(t))),
        ("gpu.driver", gpu(|g, t| g.driver = f(t))),
        ("gpu.temp_slowdown", gpu(|g, t| g.temp_slowdown = f(t))),
        ("gpu.encoder", gpu(|g, t| g.encoder = f(t))),
        ("gpu.clock_mem", gpu(|g, t| g.clock_mem = f(t))),
        ("gpu.fans", gpu(|g, t| g.fans = f(t))),
        ("gpu.throttle", gpu(|g, t| g.throttle = f(t))),
        (
            "own.ws",
            Box::new(|fa, _, _, t| {
                if let Ok(o) = &mut fa.own {
                    o.private_ws = f(t);
                }
            }),
        ),
        (
            "own.cpu",
            Box::new(|fa, _, _, t| {
                if let Ok(o) = &mut fa.own {
                    o.cpu = f(t);
                }
            }),
        ),
        (
            "slow.vram",
            Box::new(|_, sl, _, t| {
                if let Ok(l) = &mut sl.gpus {
                    l[0].vram = f(t);
                }
            }),
        ),
        (
            "slow.pcie",
            Box::new(|_, sl, _, t| {
                if let Ok(l) = &mut sl.gpus {
                    l[0].pcie_tx = f(t);
                }
            }),
        ),
        (
            "slow.disktemp",
            Box::new(|_, sl, _, t| {
                if let Ok(l) = &mut sl.disk_temps {
                    l[0].celsius = f(t);
                }
            }),
        ),
        (
            "slow.diskhealth",
            Box::new(|_, sl, _, t| {
                if let Ok(l) = &mut sl.disk_temps {
                    l[0].health = f(t);
                }
            }),
        ),
        (
            "slow.disktemp.all",
            Box::new(|_, sl, _, t| sl.disk_temps = f(t)),
        ),
        ("game.resolution", game(|g, t| g.resolution = f(t))),
        ("game.fps", game(|g, t| g.fps = f(t))),
        ("game.frametime", game(|g, t| g.frametime = f(t))),
        ("game.gpu_time", game(|g, t| g.gpu_time = f(t))),
        ("game.lows", game(|g, t| g.lows = f(t))),
        ("game.bound", game(|g, t| g.bound = f(t))),
        ("rtss.pipe", Box::new(|_, _, fp, t| fp.pipe = f(t))),
    ]
}

/// As marcas do começo e do fim da causa aparecem em vermelho na tela (a causa pode estar
/// quebrada em linhas dentro de uma caixa: o fim visível prova que nada foi cortado).
fn red_text(st: &mut State, tags: &[String], tab: view::Tab, (w, h): (u16, u16)) -> bool {
    st.set_tab(tab);
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal de teste");
    t.draw(|fr| view::draw(fr, st)).expect("desenho");
    let buf = t.backend().buffer();
    let red_in_row = |y: u16, tag: &str| {
        let row: Vec<(String, ratatui::style::Color)> = (0..w)
            .map(|x| (buf[(x, y)].symbol().to_string(), buf[(x, y)].fg))
            .collect();
        let chars: Vec<char> = tag.chars().collect();
        (0..=row.len().saturating_sub(chars.len())).any(|i| {
            chars.iter().enumerate().all(|(k, c)| {
                row[i + k].0 == c.to_string() && row[i + k].1 == super::theme::th().fail
            })
        })
    };
    tags.iter().all(|tag| (0..h).any(|y| red_in_row(y, tag)))
}

#[test]
fn toda_falha_contada_aparece_em_vermelho() {
    let mut problems = Vec::new();
    for (i, (name, inject)) in injections().into_iter().enumerate() {
        let tag = format!("TAG{i:02}");
        let mut fa = fast(3);
        let mut sl = slow();
        let mut fp = game();
        inject(&mut fa, &mut sl, &mut fp, &tag);
        let mut st = State::new(1000);
        st.push_fps(fp);
        st.push_fast(fa);
        st.push_slow(sl);
        let counted = view::failing_sources(&st);
        let tags = [tag.clone(), tag.replace("TAG", "END")];
        if counted == 0 {
            problems.push(format!("{name}: falha não contada no rodapé"));
        }
        // Em cada tamanho comum de terminal, a causa aparece inteira em alguma aba.
        for size in [(200, 60), (160, 48), (120, 40)] {
            let seen = [
                view::Tab::Overview,
                view::Tab::Sensors,
                view::Tab::Gpu,
                view::Tab::Processes,
            ]
            .into_iter()
            .any(|t| red_text(&mut st, &tags, t, size));
            if !seen {
                problems.push(format!(
                    "{name}: causa não aparece inteira em vermelho em {}x{}",
                    size.0, size.1
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn aviso_do_config_aparece_inteiro_sem_esconder_o_estado() {
    let mut st = filled();
    st.notice = Some(
        r"config: syntax error (TOML parse error at line 3, column 14 in C:\Users\someone\AppData\Roaming\wintop\config.toml); defaults, not saved".into(),
    );
    for w in [80, 120, 200] {
        let out = render(&mut st, w, 40);
        let flat: String = out
            .lines()
            .map(|l| l.trim_matches('"').trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(flat.contains("defaults, not saved"), "{w}\n{out}");
        let footer = out.lines().last().expect("rodapé");
        assert!(footer.contains("sources: all OK"), "{w}\n{footer}");
    }
}

#[test]
fn aba_gpu_ordena_por_coluna_e_filtra() {
    use super::view::tabs::{GpuSort, sorted_gpu};
    let list = gpu_list();
    let pids = |v: Vec<&crate::model::GpuProc>| v.iter().map(|p| p.pid).collect::<Vec<_>>();
    // Padrão: GPU% decrescente.
    assert_eq!(
        pids(sorted_gpu(&list, GpuSort::default(), false, "")),
        [4242, 9120, 1784, 7000]
    );
    // VRAM decrescente; invertida, crescente.
    assert_eq!(
        pids(sorted_gpu(&list, GpuSort::Vram, false, "")),
        [4242, 9120, 1784, 7000]
    );
    assert_eq!(
        pids(sorted_gpu(&list, GpuSort::Vram, true, "")),
        [7000, 1784, 9120, 4242]
    );
    // Nome crescente e filtro por nome ou PID.
    assert_eq!(
        pids(sorted_gpu(&list, GpuSort::Name, false, "")),
        [4242, 1784, 9120, 7000]
    );
    assert_eq!(pids(sorted_gpu(&list, GpuSort::Gpu, false, "FIRE")), [9120]);
    assert_eq!(pids(sorted_gpu(&list, GpuSort::Gpu, false, "1784")), [1784]);
    // ←→ percorre todas as colunas e volta.
    let mut s = GpuSort::default();
    for _ in 0..11 {
        s = s.next();
    }
    assert_eq!(s, GpuSort::default());
}

#[test]
fn ajuda_e_pausa_aparecem() {
    let mut st = filled();
    st.help = true;
    let out = render(&mut st, 160, 48);
    assert!(
        out.contains("any key closes") && out.contains("pause the screen"),
        "{out}"
    );
    // A ajuda inteira cabe num terminal de 120x30 (o conhost do atalho).
    let small = render(&mut st, 120, 30);
    assert!(small.contains("gray = expected state"), "{small}");
    st.help = false;
    st.paused = true;
    let out = render(&mut st, 160, 48);
    assert!(
        out.lines().next().is_some_and(|l| l.contains("PAUSED")),
        "{out}"
    );
}

#[test]
fn discos_com_iops_e_fila_em_tela_larga() {
    let mut st = filled();
    let out = render(&mut st, 320, 80);
    assert!(out.contains("iops") && out.contains("  q  temp"), "{out}");
    // Em 160 colunas, sem IOPS e fila (o resto não encolhe por causa deles).
    let out = render(&mut st, 160, 48);
    assert!(!out.contains("iops"), "{out}");
}

#[test]
fn falha_prevista_do_smart_aparece_em_vermelho() {
    let mut st = filled();
    if let Some(Ok(l)) = st.slow.as_mut().map(|s| &mut s.disk_temps) {
        l[0].health = Ok(true);
    }
    let out = render(&mut st, 200, 60);
    assert!(out.contains("the drive predicts a failure"), "{out}");
    st.set_tab(view::Tab::Sensors);
    let out = render(&mut st, 200, 60);
    assert!(out.contains("FAILURE PREDICTED"), "{out}");
}
