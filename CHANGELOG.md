# Changelog

Todas as mudanças relevantes do projeto. Formato baseado em
[Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/); versões no
[Versionamento Semântico](https://semver.org/lang/pt-BR/).

## [Não lançado]

### Alterado

- Barras horizontais (`RAM`, `commit`, `pagefile`, `VRAM` e `shared`) convertidas de blocos sólidos
  (`■`) para braille (`⡇`/`⣿`, com trilha apagada em `dim`), alinhadas ao padrão visual dos
  gráficos e dos termômetros verticais.
- Caixa de rede invertida para mostrar download (`↓ receive`) na metade superior e upload
  (`↑ send`) na metade inferior, mantendo a mesma ordem no cabeçalho e na linha de taxas.

## [1.3.0] - 2026-11-24

### Adicionado

- Modo debug opcional no `config.toml` (`debug = false` por padrão, com recarga ao vivo):
  registra em `wintop-debug.log` (na pasta do config, via `userfile`, com rotação em 2 MiB para
  `.1`) transições de falha e recuperação das fontes, erros transitórios/tolerados (recusas do
  SMART na janela de tolerância, `NaN` de processadores lógicos no PDH, eventos descartados no
  ETW), pânicos isolados de coletores/threads e falhas silenciosas de config ou atualização.
- Termômetro vertical de temperatura à direita das caixas da CPU (`Tctl`) e da GPU (NVML), com
  limite de referência no topo, barra vertical em braille no gradiente do tema e leitura atual
  embaixo (oculto automaticamente em terminais estreitos ou GPUs sem NVML).
- RAM do jogo em foco destacada como categoria `game` na barra segmentada e na legenda da RAM
  (como na VRAM), coletada na thread lenta por `SystemProcessInformation` sem abrir o processo do
  jogo.
- Razão efetiva dinâmica `(Nx)` no frame generation (ex.: `2.5x` em MFG dinâmico), a partir da
  razão `apresentados / reais` no ETW quando diferente do multiplicador nominal.
- Cores semânticas configuráveis em `[colors]` (`accent`, `ok`, `warn`) em todos os 5 temas
  (mantendo o tema `mono` em tons de cinza exceto falhas) para destacar valores principais,
  estados saudáveis (`all OK`, `virt enabled`, `throttle none`, `SMART OK`, link conectado) e
  alertas (`throttle` ativo, temperaturas elevadas, limitado pela GPU).

### Alterado

- Caixa da GPU reorganizada no mesmo padrão da CPU + memória: gráfico, engines e dados da placa
  (`clk`, `temp`, `fans`, `PCIe`/`drv`/`VBIOS`) ficam separados da seção de VRAM (`VRAM`,
  legenda, `shared`, `phys`, `session VRAM`) por um divisor horizontal rotulado `── vram ──`, e os
  mini gráficos das engines (`copy`/3D, `encode`, `decode`, `mem act`) acompanham a largura dos
  núcleos da CPU com altura e espaçamento idênticos entre si (e linha de base sutil em `0%`), num
  balanço `56% / 44%` entre CPU e GPU.
- Gráficos de FPS e frametime continuam avançando no ritmo da UI mesmo no ocioso (sem jogo em
  foco), desenhando linha de base apagada enquanto o histórico anterior sai pela esquerda até `r`.
- Barra de `pagefile` alinhada no mesmo padrão de `RAM` e `commit`, mostrando `usado / total (%)`
  em bytes.
- Cor de `ram_modified` ajustada nos temas coloridos para se distinguir claramente da categoria
  `game` na barra da RAM.

### Corrigido

- SMART de SSDs SATA (`IOCTL_STORAGE_PREDICT_FAILURE` retornando `error 0x1F` /
  `ERROR_GEN_FAILURE` de forma intermitente): leitura de temperatura e `PredictFailure` unificadas
  num único IOCTL (eliminando a 2ª chamada imediata), checagem de estado de energia
  (`GetDevicePowerState`) estendida a SSDs SATA em DevSleep/D3, tentativas rápidas também para
  discos que falharam na descoberta inicial e reserva via `SMART_RCV_DRIVE_DATA` (`0x07C088`).

## [1.2.1] - 2026-11-22

A 1.2.0 não foi publicada (a tag existe, sem release): esta versão inclui tudo o que estava nela.

### Adicionado

- Primeira abertura de uma cópia não instalada: o wintop pergunta se deve instalar (cópia em
  Program Files com o atalho sem UAC, e abre a cópia instalada) ou rodar portátil na pasta, com o
  `config.toml` ao lado do executável. A pasta portátil pode ficar na raiz de um disco ou
  pendrive (`D:\wintop`) e ter qualquer dono; continua sem seguir links.
- Atualização automática: a cada abertura, consulta a última release no GitHub (sem API, pelo
  redirecionamento de `releases/latest`, via WinHTTP) e pergunta se deve atualizar; o zip só é
  usado com a assinatura ed25519 válida (chave pública embutida) e na versão anunciada, e só o
  executável e os módulos saem dele (lidos na memória). Depois da troca, o wintop reabre no
  mesmo console, com o executável novo travado e conferido até o reinício (na cópia portátil,
  a pasta é do usuário). Opção `check_updates` no `config.toml`.
- FPS por ETW (a lógica do PresentMon, enxuta): DXGI `Present_Start` do jogo com filtro de
  PID, frametime exato também para jogos abertos antes do wintop ("ETW: exact"). O pipe do RTSS
  e a Shared Memory ficam de reserva (Vulkan, OpenGL, D3D9 ou ETW indisponível); o RTSS continua
  escolhendo o jogo e dando API, resolução e tempo de GPU.
- Frame generation (DLSS FG): pelos marcadores Reflex do `PCLStatsTraceLoggingProvider`, o
  cabeçalho do gráfico mostra "FPS 190 · real 47 (4x)" e os frames reais aparecem em tom escuro
  dentro do gráfico de FPS. `--check` informa a sessão ETW e o FPS real.
- `fps_probe --fg N` imita um jogo com frame gen Nx (marcadores do Reflex) para os testes.
- Releases com `<zip>.sig` (assinatura ed25519 feita no workflow, conferida antes de anexar).

### Corrigido

- CPU: um processador lógico sem dado válido numa coleta do PDH saía da lista; uso, kernel e
  clock por lógico ficavam com tamanhos diferentes (falha piscando) e a grade de gráficos da CPU
  era recriada, perdendo o histórico. Agora ele mantém a posição com o último valor válido.
- Temperatura e saúde de SSD SATA que recusa a consulta SMART em economia de energia: o último
  valor bom fica por até 10 minutos (antes, ~30 s) e uma consulta recusada não apaga o "OK" da
  saúde; só sem nenhuma leitura boa nesse prazo a falha aparece.

## [1.1.1] - 2026-11-22

### Alterado

- Gradiente do tema padrão: #1E1B4B embaixo, #7E57C2 no meio, #FF8A65 no alto. Atividade
  pequena nos gráficos de uma linha e as porcentagens das tabelas continuam legíveis no fundo.
- Cores do jogo no tema padrão: FPS #108981, frametime #8386F1, picos do frametime #FF8A65
  (cor nova `frametime_spike` em `[colors]`; nos outros temas, a cor de falha).
- Gráfico de FPS com teto na taxa de atualização do monitor principal (relida a cada 10 s);
  acima dela a coluna fica #1E1B4B e abaixo de 30 FPS, #FF8A65 (cores `fps_over` e `fps_low`).
- Caixa da GPU no estilo da caixa da CPU: valor e engine no alto do gráfico com o mínimo e o
  máximo embaixo; ao lado, uma grade de mini gráficos (com o gradiente) com o 3D, ou a engine
  mais usada fora o 3D e o vídeo quando o 3D já está no gráfico principal, mais encoder, decoder
  e atividade da memória; temperatura, potência e clock com o máximo da sessão ao lado do
  valor e a VRAM numa linha "session VRAM" (como a "session RAM").
- A consulta `\GPU Engine(*)` da thread rápida vale para todas as GPUs (antes só sem NVML),
  para a engine fora o 3D e o vídeo (~1 ms por coleta); nova linha no `--check`.
- A caixa do jogo tem a mesma altura antes e depois do primeiro jogo (o layout não pula).
- Rede: os gráficos ocupam a altura toda da caixa, e uma linha de base apagada mostra o eixo
  com a placa parada.

## [1.1.0] - 2026-11-22

### Adicionado

- Várias GPUs: a tecla `g` troca a GPU das abas Geral e GPU; a aba Sensores e o `--check`
  mostram todas.
- GPUs sem NVML (AMD, Intel, ou NVIDIA com o NVML em falha): nome, driver, uso, encoder,
  decoder e VRAM pelo DXGI e pelos contadores do Windows (mesma conta do Gerenciador de
  Tarefas); os campos só do NVML aparecem numa linha com o motivo.
- Idioma da interface: `language = "pt-BR"` no `config.toml` (português do Brasil, com vírgula
  decimal); `en-US` continua o padrão.
- Aba GPU com ordenação por coluna (`←`/`→`, `i`) e filtro (`/`), como a aba Processos.
- Pausa da tela (`p` ou Espaço), com aviso visível; a coleta continua.
- Ajuda na tela (`?`), com as teclas e as siglas.
- Aba Processos: árvore de processos pais e filhos (`e`) e encerrar o processo selecionado
  (`Del`), com confirmação.
- Mouse: clique nas abas, nas linhas e nos cabeçalhos (ordena); rodinha rola as listas (opção
  `mouse` no config).
- Aba Geral: ampliar uma caixa para a aba inteira (`z`); aba Sensores com rolagem quando não
  cabe.
- Discos: IOPS, fila e alerta de falha prevista do SMART (sem acordar HDD em repouso). Rede:
  erros de pacote.
- CPU: clock por núcleo na grade e pico e média na linha de velocidade.
- `config.toml` ao vivo (mudanças valem em até 2 s, inclusive a lista de apps ignorados do
  RTSS), limite de alerta do frametime (`frametime_alert_ms`) e unidade da rede (`net_unit`).
- `--check`: auditoria das permissões das pastas do RTSS, do PawnIO e da cópia instalada, e saída
  `--json`. `--install` atualiza a cópia instalada mesmo com o wintop aberto.
- Console clássico maximizado ao abrir (`start_maximized`) e título na janela; ícone do
  programa.
- CI: pacote em toda execução, fuzz longo semanal e snapshots em 120x30.

### Alterado

- Abas Processos e GPU: a seleção fica na linha (os processos mudam embaixo dela) em vez de
  seguir o processo, que fazia a lista pular a cada reordenação por CPU.
- Instância única num namespace privado de Administradores: um processo sem elevação não
  consegue mais impedir o wintop de abrir.
- Aba Geral: CPU e memória num bloco só, rede compacta, gráficos com o gradiente lavanda,
  violeta e índigo, e várias melhorias de legibilidade (nada cortado em terminais pequenos;
  barra do topo e rodapé encolhem por prioridade).
- Leituras de arrays do PDH reaproveitam o buffer entre as coletas (menos alocações).

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
