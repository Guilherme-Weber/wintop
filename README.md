# wintop

Monitor de sistema em terminal (TUI) para Windows, escrito em Rust. Reúne numa única tela as
informações que normalmente exigem o Gerenciador de Tarefas e o MSI Afterburner abertos ao mesmo
tempo: uso de CPU por thread, temperatura e potência do processador, memória, discos individuais
(com temperatura e membros de Storage Spaces), rede, GPU e VRAM por processo, e FPS e frametime
de jogos.

## Estado

Versão 1.2.0. O histórico está em
[CHANGELOG.md](CHANGELOG.md), o inventário completo de métricas em
[docs/metricas.md](docs/metricas.md), as diferenças conhecidas em relação ao Gerenciador de
Tarefas em [docs/divergencias.md](docs/divergencias.md) e as provas de viabilidade de cada fonte
em [docs/spike-etapa0.md](docs/spike-etapa0.md).

## Instalação

Baixe o `wintop-<versão>-windows-x64.zip` da página de
[releases](https://github.com/Guilherme-Weber/wintop/releases), extraia e abra o `wintop.exe`.
Na primeira abertura, o wintop pergunta como rodar:

- **Install**: copia para `C:\Program Files\wintop` e cria um atalho no Menu Iniciar que o abre
  sem a janela do UAC (ver [Atalho sem UAC](#atalho-sem-uac)); em seguida, abre a cópia
  instalada. Equivale a `.\wintop.exe --install` num terminal como administrador.
- **Portable**: roda da própria pasta, com o `config.toml` gravado ao lado do executável (a
  presença desse arquivo marca a cópia como portátil e a pergunta não volta). A pasta `modules`
  precisa ficar ao lado do executável.

### Atualização automática

A cada abertura, o wintop consulta a última release publicada no GitHub (uma requisição, numa
thread, sem atrasar a interface; sem rede, nada aparece). Havendo versão nova, a interface
pergunta se deve atualizar; com `y`, baixa o zip, confere a assinatura ed25519 (`.zip.sig` da
release) com a chave pública embutida no executável, troca o `wintop.exe` e os módulos da cópia
em uso (instalada ou portátil) e reabre o programa. Um pacote sem assinatura válida, ou de outra
versão que não a anunciada, é recusado sem alterar nada. A consulta pode ser desligada com
`check_updates = false` no `config.toml`.

O executável não tem assinatura Authenticode: o Windows pode mostrar o aviso do SmartScreen
("Mais informações" > "Executar assim mesmo") e o UAC mostra editor desconhecido. Por isso,
confira o download antes de executá-lo:

```powershell
# hash: igual ao do arquivo .sha256 da release
(Get-FileHash wintop-<versão>-windows-x64.zip -Algorithm SHA256).Hash
# procedência: compilado pelo GitHub Actions a partir deste repositório (GitHub CLI)
gh attestation verify wintop-<versão>-windows-x64.zip -R Guilherme-Weber/wintop
```

O zip é reproduzível a partir do código: `.\scripts\package.ps1` no commit da versão gera o
mesmo pacote (exceto pelas diferenças de versão do compilador MSVC entre máquinas).

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

O arquivo `%APPDATA%\wintop\config.toml` (na cópia portátil, `config.toml` ao lado do
executável) é criado na primeira execução, com comentários:
intervalo de atualização, tema (`default`, `nord`, `gruvbox`, `dracula`, `mono`), recolhimento
dos membros do Storage Space, lista de aplicativos que o RTSS acompanha mas não são jogos, busca
de atualização (`check_updates`) e cores avulsas (`[colors]`, em `#rrggbb`). O idioma da interface é escolhido em `language`: `en-US`
(padrão) ou `pt-BR` (português do Brasil, com vírgula decimal); erros vindos do sistema e dos
drivers aparecem como o Windows os informa. As mudanças feitas na interface (`+`/`-`, `t`, `d`) são
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
- GPU NVIDIA com driver recente (NVML) para todas as métricas da GPU. GPUs AMD e Intel aparecem
  com nome, driver, uso, encoder/decoder e VRAM (DXGI e contadores do Windows); com mais de uma
  GPU, a tecla `g` troca a exibida.
- [RivaTuner Statistics Server](https://www.guru3d.com/download/rtss-rivatuner-statistics-server-download/)
  para identificar o jogo (API, resolução, tempo de GPU). Os frames vêm do ETW (DXGI), com o
  pipe do RTSS de reserva; com DLSS Frame Generation, o FPS real aparece ao lado do apresentado.
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

O pacote de distribuição (o mesmo que a release publica) sai de `.\scripts\package.ps1`, em
`dist\`. Fuzz longo dos parsers e teste de longa duração:

```powershell
$env:WINTOP_FUZZ_ITERS = 2000000; cargo test --profile fuzz --locked fuzz_
.\scripts\soak.ps1 -Hours 8   # terminal elevado, com o wintop aberto
```

## Licença

Distribuído sob a licença MIT (arquivo [LICENSE](LICENSE)). O módulo `modules/AMDFamily17.bin`
pertence ao projeto PawnIO.Modules e é distribuído sob a LGPL-2.1, em arquivo separado
(detalhes em [modules/README.md](modules/README.md)).
