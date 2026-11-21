# wintop

Monitor de sistema em terminal (TUI) para Windows, escrito em Rust. Reúne numa única tela as
informações que normalmente exigem o Gerenciador de Tarefas e o MSI Afterburner abertos ao mesmo
tempo: uso de CPU por thread, temperatura e potência do processador, memória, discos individuais
(com temperatura e membros de Storage Spaces), rede, GPU e VRAM por processo, e FPS e frametime
de jogos.

## Estado

Em desenvolvimento. Concluídas as etapas de base e provas de viabilidade
([docs/spike-etapa0.md](docs/spike-etapa0.md)), coletores nativos, fontes elevadas (PawnIO e
temperatura dos discos), FPS pelo RTSS, motor de coleta e as quatro abas da interface (geral,
processos, GPU por processo e sensores), configuração, temas e o atalho sem UAC. O empacotamento
ainda está por vir. O inventário
completo de métricas está em [docs/metricas.md](docs/metricas.md), e as diferenças conhecidas em
relação ao Gerenciador de Tarefas em [docs/divergencias.md](docs/divergencias.md).

## Uso

```powershell
wintop            # interface
wintop --check    # diagnóstico de todas as fontes, sem interface
wintop --install  # copia para Program Files e cria o atalho sem UAC (tarefa agendada)
wintop --uninstall
wintop --version
```

Na interface: `1` a `4` trocam de aba, `+` e `-` ajustam o intervalo de atualização (passos de
100 ms), `r` zera os mínimos e máximos da sessão, `d` recolhe os membros do Storage Space e `q`
(ou `Esc`, `Ctrl+C`) sai. Nas abas Processos e GPU, as setas (ou `j`/`k`, `PgUp`/`PgDn`,
`Home`/`End`) selecionam; na aba Processos, `←`/`→` mudam a coluna da ordem, `i` inverte e `/`
filtra pelo nome ou PID; na aba GPU, `Enter` lista os runtimes de GPU do processo selecionado
(lendo os módulos carregados; nunca em aplicativos com o hook do RTSS, que podem ser jogos com
anticheat). `t` troca o tema. A interface é em inglês.

## Configuração

O arquivo `%APPDATA%\wintop\config.toml` é criado na primeira execução, com comentários:
intervalo de atualização, tema (`default`, `nord`, `gruvbox`, `dracula`, `mono`), recolhimento
dos membros do Storage Space, lista de aplicativos que o RTSS acompanha mas não são jogos e cores
avulsas (`[colors]`, em `#rrggbb`). As mudanças feitas na interface (`+`/`-`, `t`, `d`) são
gravadas no próprio arquivo, preservando os comentários. Um arquivo com erro de sintaxe não
impede a abertura: o wintop usa os padrões, avisa no rodapé e não o regrava.

## Atalho sem UAC

`wintop --install` copia o executável e a pasta `modules` para `C:\Program Files\wintop`, cria
uma tarefa agendada que executa o wintop com privilégio máximo (somente com o login do usuário,
sem gatilhos) e um atalho no Menu Iniciar que dispara essa tarefa, sem a janela do UAC. Antes de
criar a tarefa, o instalador confere que nenhum usuário sem elevação pode alterar a pasta ou o
executável (a pasta recebe dono Administradores e uma ACL própria); caso contrário, recusa a
instalação. Aberto pelo atalho, o wintop usa o console clássico do Windows (o Agendador de
Tarefas não repassa para o Windows Terminal). `wintop --uninstall` remove a tarefa, o atalho e
a cópia instalada.

Com o atalho instalado, qualquer programa do usuário consegue abrir o wintop elevado. Por isso o
que o wintop lê e grava na pasta do usuário (o `config.toml`) passa por verificações: links,
redirecionamentos e caminhos que não sejam locais são recusados.

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
