//! Spike da Etapa 0: inventário do NVML na GPU (o que é suportado e quanto custa cada leitura).
//!
//! Carrega `nvml.dll` por caminho absoluto em System32 obtido pela API (a variável `SystemRoot`
//! é manipulável) e com a busca de DLLs restrita a System32 (processo elevado).
//! `cargo run --example spike_nvml`

#[path = "common/mod.rs"]
mod common;

use std::fmt::Debug;
use std::time::Instant;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{
    Clock, PcieUtilCounter, TemperatureSensor, TemperatureThreshold,
};

fn show<T: Debug>(name: &str, f: impl FnOnce() -> Result<T, nvml_wrapper::error::NvmlError>) {
    let t = Instant::now();
    let r = f();
    let us = t.elapsed().as_micros();
    match r {
        Ok(v) => {
            let mut s = format!("{v:?}");
            if s.len() > 110 {
                s.truncate(s.floor_char_boundary(110));
                s.push_str("...");
            }
            println!("  OK  {name:<30} {us:>6} µs  {s}");
        }
        Err(e) => println!("  --  {name:<30} {us:>6} µs  {e}"),
    }
}

fn main() {
    common::harden_dll_search();
    let Some(sys) = common::system_dir() else {
        return eprintln!("FALHA: GetSystemDirectoryW");
    };
    let lib = sys.join("nvml.dll");
    let t = Instant::now();
    let nvml = match Nvml::builder().lib_path(lib.as_os_str()).init() {
        Ok(n) => n,
        Err(e) => return eprintln!("FALHA: NVML ({}): {e}", lib.display()),
    };
    println!(
        "NVML iniciado em {:.1} ms ({})",
        t.elapsed().as_secs_f64() * 1e3,
        lib.display()
    );
    show("versão do driver", || nvml.sys_driver_version());
    show("versão do NVML", || nvml.sys_nvml_version());
    show("versão CUDA", || nvml.sys_cuda_driver_version());

    let count = nvml.device_count().unwrap_or(0);
    for i in 0..count {
        let Ok(d) = nvml.device_by_index(i) else {
            continue;
        };
        println!("\nGPU {i}:");
        show("nome", || d.name());
        show("PCI", || d.pci_info().map(|p| p.bus_id));
        show("temperatura GPU", || d.temperature(TemperatureSensor::Gpu));
        show("limite de slowdown", || {
            d.temperature_threshold(TemperatureThreshold::Slowdown)
        });
        show("utilização (gpu, memória)", || d.utilization_rates());
        show("memória (NVML)", || d.memory_info());
        show("BAR1", || d.bar1_memory_info());
        show("clock gráfico", || d.clock_info(Clock::Graphics));
        show("clock SM", || d.clock_info(Clock::SM));
        show("clock memória", || d.clock_info(Clock::Memory));
        show("clock vídeo", || d.clock_info(Clock::Video));
        show("clock gráfico máx", || d.max_clock_info(Clock::Graphics));
        show("potência (mW)", || d.power_usage());
        show("limite de potência (mW)", || d.enforced_power_limit());
        show("energia total (mJ)", || d.total_energy_consumption());
        show("nº de fans", || d.num_fans());
        for f in 0..d.num_fans().unwrap_or(0) {
            show(&format!("fan {f} (%)"), || d.fan_speed(f));
            show(&format!("fan {f} (RPM)"), || d.fan_speed_rpm(f));
        }
        show("PCIe geração atual", || d.current_pcie_link_gen());
        show("PCIe largura atual", || d.current_pcie_link_width());
        show("PCIe geração máx", || d.max_pcie_link_gen());
        show("PCIe TX (KB/s)", || {
            d.pcie_throughput(PcieUtilCounter::Send)
        });
        show("PCIe RX (KB/s)", || {
            d.pcie_throughput(PcieUtilCounter::Receive)
        });
        show("encoder", || d.encoder_utilization());
        show("decoder", || d.decoder_utilization());
        show("P-state", || d.performance_state());
        show("motivos de throttling", || d.current_throttle_reasons());
        show("processos gráficos", || {
            d.running_graphics_processes().map(|p| p.len())
        });
        show("processos compute", || {
            d.running_compute_processes().map(|p| p.len())
        });
        show("VBIOS", || d.vbios_version());
    }
}
