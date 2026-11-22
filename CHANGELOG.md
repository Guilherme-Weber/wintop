# Changelog

Todas as mudanças relevantes do projeto. Formato baseado em
[Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/); versões no
[Versionamento Semântico](https://semver.org/lang/pt-BR/).

## [Não lançado]

### Alterado

- Abas Processos e GPU: a seleção fica na linha (os processos mudam embaixo dela) em vez de
  seguir o processo, que fazia a lista pular a cada reordenação por CPU.

## [1.0.0] - 2026-11-21

Primeira versão.

### Adicionado

- Interface em terminal no estilo do btop, com quatro abas:
  - **Geral:** uso de CPU (total e por núcleo, com a parte do kernel), temperatura (Tctl e CCD)
    e potência do processador, RAM por composição e memória confirmada, discos (atividade,
    leitura e escrita, resposta média, temperatura, membros de Storage Spaces), rede por placa,
    GPU (engine mais usada, temperatura, potência, clocks, PCIe) e VRAM por categoria, FPS,
    frametime e lows (1% e 0,1%) do jogo em foco, com mínimos e máximos da sessão (`r`).
  - **Processos:** lista sem abrir nenhum processo, com ordem, filtro e seleção estável.
  - **GPU:** uso de engine e VRAM por processo; runtimes de GPU sob demanda (`Enter`), nunca em
    aplicativos com o hook do RTSS.
  - **Sensores:** todas as leituras físicas (temperaturas, potências, clocks, ventoinhas) numa
    tabela, com mínimo e máximo.
- Fontes: APIs nativas do Windows (sem WMI), NVML para a GPU NVIDIA, PawnIO para a CPU AMD
  (módulo LGPL em arquivo separado), RTSS para FPS (pipe de frametime exato; modo aproximado
  pela Shared Memory quando o jogo abriu antes do wintop). Temperatura de disco nativa (NVMe) ou
  SMART (SATA), sem acordar HDD em repouso.
- Intervalo de atualização ajustável com `+` e `-` (passos de 100 ms), valendo na hora.
- `wintop --check`: diagnóstico de todas as métricas, sem interface.
- `%APPDATA%\wintop\config.toml`: intervalo, tema (`t`: default, nord, gruvbox, dracula, mono),
  cores avulsas, recolhimento do Storage Space, aplicativos que não são jogos; regravado
  preservando os comentários.
- `wintop --install` / `--uninstall`: cópia para `C:\Program Files\wintop` e atalho no Menu
  Iniciar que abre o wintop elevado sem a janela do UAC (tarefa agendada).
- Instância única por sessão; parada limpa com `q`, `Esc`, Ctrl+C, Ctrl+Break ou fechando a
  janela.
- Consumo medido do próprio wintop (fica abaixo de 0,1% de CPU somando o terminal, ~26 MB).

### Segurança

- Processo elevado endurecido: DLLs só de System32 (inclusive as importações estáticas), exceto
  a `PawnIOLib.dll`, carregada por caminho absoluto da instalação do PawnIO (HKLM); caminhos do
  registro só locais e sem expansão, Shared Memory só leitura e com limites, pipe do
  RTSS com DACL própria e tetos de memória, arquivos do usuário sem seguir links nem
  redirecionamentos, instalação recusada se um usuário sem elevação puder alterar a pasta.
- Anticheat: nenhum processo é aberto para leitura automaticamente; os runtimes de GPU só são
  lidos sob demanda (`Enter`), nunca em processos com o hook do RTSS ou recém-iniciados, e
  acesso negado aparece como "protected".
- Parsers de dados externos cobertos por fuzz (milhões de entradas, com checagem de overflow).
