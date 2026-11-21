//! wintop: monitor de sistema em terminal para Windows.
//!
//! Etapa 0 (base e spike de viabilidade): o programa ainda não coleta nada. Os spikes de
//! cada fonte ficam em `examples/` (`cargo run --example spike_<fonte>`); os coletores
//! definitivos começam na Etapa 1. Ver `docs/metricas.md` e `docs/spike-etapa0.md`.

fn main() {
    println!(
        "wintop {} - em desenvolvimento (Etapa 0). Veja docs/spike-etapa0.md.",
        env!("CARGO_PKG_VERSION")
    );
}
