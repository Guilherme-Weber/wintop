# wintop

Monitor de sistema em terminal (TUI) para Windows, escrito em Rust. Reúne numa única tela as
informações que normalmente exigem o Gerenciador de Tarefas e o MSI Afterburner abertos ao mesmo
tempo: uso de CPU por thread, temperatura e potência do processador, memória, discos individuais
(com temperatura e membros de Storage Spaces), rede, GPU e VRAM por processo, e FPS e frametime
de jogos.

## Estado

Em desenvolvimento. Concluída a Etapa 0 (base do projeto e prova de viabilidade de cada fonte de
dados, descrita em [docs/spike-etapa0.md](docs/spike-etapa0.md)). O inventário completo de
métricas está em [docs/metricas.md](docs/metricas.md).

## Requisitos

- Windows 10 ou 11, 64 bits.
- Execução como administrador (o executável solicita elevação).
- GPU NVIDIA com driver recente (NVML).
- [RivaTuner Statistics Server](https://www.guru3d.com/download/rtss-rivatuner-statistics-server-download/)
  para FPS e frametime.
- [PawnIO](https://pawnio.eu) para temperatura e potência de processadores AMD
  (`winget install namazso.PawnIO`).

## Compilação

A versão do Rust é fixada em `rust-toolchain.toml`.

```powershell
cargo build --release
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Os testes rodam num terminal comum (o manifesto de administrador vai só no executável
principal). Os programas de prova de cada fonte ficam em `examples/` e exigem um terminal
elevado, pelo acesso a hardware que fazem:

```powershell
cargo run --example spike_pawnio
cargo run --example spike_discos
cargo run --example spike_nvml
cargo run --example spike_rtss 60
```

## Licença

Distribuído sob a licença MIT (arquivo [LICENSE](LICENSE)). O módulo `modules/AMDFamily17.bin`
pertence ao projeto PawnIO.Modules e é distribuído sob a LGPL-2.1, em arquivo separado
(detalhes em [modules/README.md](modules/README.md)).
