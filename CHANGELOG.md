# Changelog

Todas as mudanças relevantes do projeto. Formato baseado em
[Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/); versões no
[Versionamento Semântico](https://semver.org/lang/pt-BR/).

## [Não lançado]

### Alterado

- CI: o Inno Setup, se não vier na imagem, é baixado na versão 6.7.3 com SHA-256 e assinatura
  conferidos (`scripts/ensure-inno.ps1`), em vez do Chocolatey sem versão fixada.
- `src/install.rs` dividido em `src/install/` (ACL, tarefa e atalho, exe e módulos,
  desinstalação), sem mudança de comportamento.

### Corrigido

- Instalador: a caixa "Abrir o wintop" da tela final dava o erro 740 ("a operação requer
  elevação"), porque o Inno Setup roda essa caixa como o usuário sem elevação; agora o wintop é
  aberto pela tarefa agendada, como o atalho do Menu Iniciar (sem UAC).
- Coluna `free` em 80 colunas: o espaço livre abaixo de 10% substitui a capacidade (antes o
  alerta laranja sumia no terminal pequeno).
- Desinstalação: recusa com o wintop aberto (antes de remover qualquer coisa) e só apaga a
  entrada de "Aplicativos instalados" depois que a pasta saiu; se a pasta falhar, a
  desinstalação pode ser repetida por lá.
- Caixas modais de erro crítico do Windows desligadas (`SetErrorMode`): um cartão removido entre
  duas leituras não abre mais "não há disco na unidade" travando a coleta.
- Atualização automática: o pacote é recusado com entradas `..`, barra invertida, `:` ou nomes
  repetidos, e com um `wintop.exe` que não seja executável (defesa em profundidade; o zip já
  tem a assinatura conferida).
- Memória comprometida de 91 MB (em vez de ~35 MB) com a janela maximizada na abertura
  (`start_maximized`, o padrão): o `ShowWindow` síncrono na janela do conhost fazia o Windows
  comprometer ~54 MB no processo do wintop. Agora a maximização é pedida com
  `ShowWindowAsync`.

## [1.4.6] - 2026-11-27

### Adicionado

- Instalador `wintop-<versão>-setup.exe` (Inno Setup, português e inglês): instala em
  `C:\Program Files\wintop` pelo mesmo `--install` endurecido, instala o PawnIO se faltar
  (release oficial 2.2.0, SHA-256 conferido), oferece a página do RTSS e abre o wintop no fim.
  Gerado pelo `package.ps1` e publicado na release junto do zip, com atestado de procedência.
- `wintop --install` registra o wintop em "Aplicativos instalados"; desinstalar ali (ou
  `wintop --uninstall`) remove tudo, inclusive a pasta quando o comando roda a partir dela.
- Coluna `free` na tabela de discos: espaço livre somado dos volumes do disco, em laranja
  (cor `disk_low`, configurável em `[colors]`) abaixo de 10%; também no `--check`.

## [1.4.5] - 2026-11-27

### Adicionado

- Temperatura e potência de processadores Intel pelo PawnIO (módulo oficial `IntelMSR`, LGPL-2.1,
  em `modules/`): pacote, cada núcleo (lido com a thread presa no núcleo) a partir do TjMax e
  potência do pacote pelo RAPL. A caixa da CPU mostra o pacote e o núcleo mais quente; a aba
  Sensores, todos os núcleos.
- Opção `graph_symbols` (`auto`, `braille`, `block`) no `config.toml`.

### Corrigido

- Gráficos e barras como quadrados vazios no console clássico do Windows 10: a fonte padrão
  (Consolas) não tem braille e o conhost não faz fallback de fonte. Agora o wintop confere a
  fonte na abertura; sem braille, troca para uma fonte monoespaçada instalada que tenha (e
  restaura a original ao sair) ou, sem nenhuma, desenha com blocos.
- Discos USB (pendrives, leitores de cartão, pontes USB) sem falsas falhas: tipo de mídia não
  informado vira "n/a", a capacidade tenta a geometria e o `READ CAPACITY` quando
  `IOCTL_DISK_GET_LENGTH_INFO` é recusado (leitor sem cartão = "empty") e a temperatura de um
  disco USB sem SMART vira "n/a".

### Observação

- A atualização automática a partir da 1.4.4 ou anterior não copia o `IntelMSR.bin` (a lista de
  módulos é a da versão em uso). Em CPUs Intel, reinstalar pelo zip completo
  (`wintop --install`) ou extrair o zip de novo na pasta portátil.

## [1.4.4] - 2026-11-27

### Corrigido

- Alinhamento de buffers em chamadas Win32/NT (`memory`, `instance` e `win`), ordem de `Drop` e
  fechamento seguro do namespace privado de instância única, rejeição de seções file-backed em
  `SharedView` (evita falha SEH `EXCEPTION_IN_PAGE_ERROR`) e proteção `catch_unwind` nos callbacks
  FFI de ETW e console.
- Endurecimento de segurança no instalador, atualizador e leitores de arquivos: leitura segura dos
  módulos PawnIO (sem seguir reparse points, hardlinks ou caminhos não-locais), leitura anti-TOCTOU
  do próprio executável, `VerifiedZip` exigindo verificação ed25519 antes de `unpack`, bloqueio de
  downgrade e allowlist de hosts no atualizador, limpeza de variáveis de ambiente ao iniciar o RTSS
  e remoção de `CREATOR OWNER` na ACL da tarefa agendada.
- Percentual `(69%)` da barra de VRAM não é mais cortado no piso de terminal 80×24; modais de
  confirmação e atualização agora quebram linhas e ajustam altura automaticamente em telas estreitas.
- `Esc` no nível raiz não fecha mais o wintop acidentalmente (mantendo `q` e `Ctrl+C` para sair;
  `Esc` continua fechando modais, filtro e zoom).
- Movimentos simples de mouse (`Moved`) não acordam mais o laço principal em idle, reduzindo
  consumo de CPU; bracketed paste ativado para colar com segurança apenas no filtro (`/`).

### Alterado

- Desempenho e concorrência: eliminação de polling de 20 ms na coleta lenta (`WaitForSingleObject`),
  cálculo de 1%/0,1% lows em O(n) sem alocação no RTSS, `drain` em O(1) no ETW, pré-cálculo único
  por quadro na caixa da GPU (`PreparedGpu`), renderização direta de divisores/linhas e caminho sem
  alocação em `i18n::tr`.
- Usabilidade e acessibilidade: `Tab`/`Shift+Tab` ciclam entre as 4 abas, `End` rola até o fim em
  Sensores, janela de ajuda (`?`) rolável, rodapé indica `p resume` quando pausado, trilha distinta
  (`⣀`) e seleção invertida (`REVERSED`) sob `NO_COLOR`/tema `mono`, e sincronização de quadro
  (`BeginSynchronizedUpdate`/`EndSynchronizedUpdate`) com restauração do título da janela ao sair.
- CI e release: `cargo-audit` fixado por versão, verificação de integridade SHA-256 antes de assinar
  e publicar, `environment: release` no job de publicação e Dependabot semanal para GitHub Actions.

## [1.4.3] - 2026-11-26

### Adicionado

- Atalho `f` na aba Geral para ocultar ou mostrar a caixa de jogo (FPS e frametime), reaproveitando
  a área liberada para expandir os gráficos de CPU/GPU e a caixa de discos; o estado é salvo em
  `show_game` no `config.toml` e o ciclo de zoom (`z`) pula a caixa de jogo quando oculta.

## [1.4.2] - 2026-11-25

### Alterado

- Linha `clk` da GPU agora exibe `MHz` ao lado de cada contador (`gfx`, `SM`, `mem` e `video`) no
  mesmo padrão de potência e temperatura, removendo `(MHz)` do rótulo.
- Divisor `── memory ──` exibe apenas as especificações de hardware da RAM (sem `hw res`, mantido
  no `--check`).
- Cabeçalho da caixa de CPU atualizado para exibir núcleos físicos e threads (`N cores · M threads`
  / `N núcleos · M threads`), movendo a duração total do buffer de histórico (`history 16m40s`) para
  o rodapé ao lado da versão e consumo do wintop.

## [1.4.1] - 2026-11-25

### Adicionado

- Memória compartilhada da GPU (`shared`) dividida por categoria (`game`, `app`, `compositor`,
  `browser`, `system`, `kernel`, `free`) via `\GPU Process Memory(*)\Shared Usage` na mesma
  consulta PDH da VRAM, com barra segmentada em braille, legenda própria na caixa da GPU e entrada
  `GPU N shared by kind` no `--check`.

### Alterado

- Unidades padronizadas no formato completo (`B`/`KB`/`MB`/`GB`/`TB` e `GHz`) nas legendas de RAM
  e VRAM, tabela de discos e grade de clocks por núcleo da CPU (removendo abreviações `G`/`M`/`K`);
  unidade de clock da GPU movida para o cabeçalho `clk (MHz)`.
- Linha `phys` da VRAM posicionada antes de `session VRAM` na mesma linha (`phys ... │ session
  VRAM ...`) para comportar a legenda de `shared` sem consumir espaço extra dos gráficos da GPU.
- Gráfico espelhado de rede revertido para upload (`↑ send`) em cima e download (`↓ receive`)
  embaixo, com a cor de download fortalecida nos temas.
- Instalador (`--install`, `--uninstall` e abertura da cópia instalada) migrado de subprocesso
  `schtasks.exe` + XML temporário em disco para a API COM nativa `ITaskService`.

## [1.4.0] - 2026-11-25

### Adicionado

- Memória comprimida exibida na barra segmentada e na legenda da RAM (`compressed` com tamanho
  original estimado em `raw`, via `NtQuerySystemInformation` classe `109`), com cor configurável
  `ram_compressed` em `[colors]` nos 5 temas.
- Especificações de hardware da RAM no divisor `── memory ──` (geração, velocidade e slots
  ocupados via SMBIOS Type 17, além de memória reservada para hardware), pico de commit desde o
  boot (`boot peak`), paginação real em disco (`paging in/out/s`) e alerta silencioso de páginas
  defeituosas (`bad RAM pages > 0`).
- Versão atual do wintop exibida no rodapé (`wintop v1.4.0`) e suporte à variável de ambiente
  `NO_COLOR`.

### Alterado

- Coleta de memória migrada integralmente de PDH para chamadas diretas `NtQuerySystemInformation`
  (`80`, `18` e `2`), somada a leitura streaming sem alocações no PDH, cache de nomes de
  adaptadores de rede e pré-cálculo de cores fora dos laços de renderização dos widgets.
- Caixa da GPU compactada e sem espaços vazios: `drv` e `VBIOS` removidos da Visão geral
  (mantidos na aba Sensores e no `--check`), `PCIe` posicionado ao lado de `fans`, altura uniforme
  entre o gráfico principal e as engines, e `min · max` na mesma linha da porcentagem tanto em CPU
  quanto em GPU.
- Demarcação e alinhamento visual aprimorados: divisores verticais entre gráfico principal,
  núcleos/engines e termômetro; linhas de base sutis nos gráficos; bordas direitas alinhadas entre
  barras de RAM/commit/pagefile e VRAM/shared; `scale` em linha própria acima do gráfico espelhado
  de rede; hierarquia de cores semânticas refinada na Visão geral, tabelas e aba Sensores.
- Refatoração interna: RAII para `SecurityDescriptor`, `PdhError` tipado, snapshots imutáveis com
  `Arc`, controlador de UI puro com gravação assíncrona do `config.toml` e separação das abas em
  módulos dedicados.

### Corrigido

- Aba Sensores: correspondência de GPUs pelo índice real (`g.index`) em sistemas multi-GPU e
  tratamento consistente de temperaturas SMART em trocas de disco.
- Visão geral em terminais 80×24: prioridade de altura ajustada para exibir o primeiro disco em
  vez de ocultar toda a seção; texto neutro de GPUs sem NVML exibido em largura total abaixo das
  engines em vez de quebrar em colunas estreitas.
- Cor `fps_over` no tema padrão corrigida para `#60A5FA` (alinhada ao cabeçalho e ao gráfico de
  FPS), textos literais de hardware/usuário preservados sem tradução (`raw_text`) e traduções
  `pt-BR` faltantes completadas.

## [1.3.2] - 2026-11-24

### Corrigido

- CPU: erros transitórios de cálculo diferencial do PDH (`PDH 0x800007D6` / `PDH_CALC_NEGATIVE_DENOMINATOR`,
  `0x800007D7` e `0x800007D8`, que podem ocorrer numa coleta isolada durante ajustes de relógio ou
  delta negativo) agora repetem a última leitura válida por até 3 coletas seguidas (registrando em
  `wintop-debug.log`) em vez de piscar falha e zerar a grade de núcleos; se só `_Total` vier `NaN`,
  usa a média dos lógicos válidos.

## [1.3.1] - 2026-11-24

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
