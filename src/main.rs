//! wintop: monitor de sistema em terminal para Windows.
//!
//! Etapa 0 (base e spike de viabilidade): o programa ainda não coleta nada. Os spikes de
//! cada fonte ficam em `examples/` (`cargo run --example spike_<fonte>`); os coletores
//! definitivos começam na Etapa 1. Ver `docs/metricas.md` e `docs/spike-etapa0.md`.

mod instance;

fn main() {
    let _instance = match instance::acquire() {
        Ok(guard) => guard,
        Err(instance::Error::AlreadyRunning) => {
            eprintln!("O wintop já está aberto nesta sessão.");
            std::process::exit(1);
        }
        Err(instance::Error::Os(e)) => {
            eprintln!("Falha ao verificar a instância única: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "wintop {} - em desenvolvimento (Etapa 0). Veja docs/spike-etapa0.md.",
        env!("CARGO_PKG_VERSION")
    );
}
