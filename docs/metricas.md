# Inventário de métricas

Lista fechada do que o wintop mostra. Regra do projeto: **aberto elevado, toda métrica desta
lista aparece**. Uma métrica sem dado é exibida em vermelho com a causa (falha); só os casos da
coluna "Estado neutro" podem aparecer sem valor ou com valor marcado, e sempre com o texto do
estado.

Origem: captura do setup anterior (Gerenciador de Tarefas + MSI Afterburner), detalhamento de VRAM
do gputaskman e detalhamento de disco do Gerenciador de Tarefas.

Coluna Elev.: **A** = exige administrador (o wintop sempre roda elevado).
Coluna Prova: **Spike** = provado em Rust na Etapa 0 (`docs/spike-etapa0.md`); **PS** = só
conferido pelo PowerShell (mesma API, sem código Rust ainda); **E1**..**E6** = etapa que
implementa e prova.

**Idioma da interface:** inglês americano (números com ponto decimal). Os estados neutros desta
lista aparecem traduzidos na interface (ex.: "desconectada" → "disconnected", "em repouso" →
"asleep", "parado" → "stopped"). Uma opção de português do Brasil pode entrar no futuro.

**Etapa 1 (2026-11-20):** as linhas E1 de CPU, memória, discos, rede e GPU foram provadas pelo
`wintop --check` na máquina de desenvolvimento (135 OK, 3 neutras, 0 falhas).

Todas as séries com histórico guardam **mínimo e máximo da sessão** (tecla para zerar), como o
Afterburner. Discos novos (USB) são detectados em até 30 s; o handle de um disco USB não fica
aberto (não impede a remoção segura). Todas as leituras de temperatura de disco e o PCIe do NVML rodam na **thread lenta**.

## CPU

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome do processador | CPUID (0x80000002-4, se a folha máxima permitir) | | | Spike |
| Uso total (%) | PDH `\Processor Information(_Total)\% Processor Time` (tempo ocupado, sem escalar pela frequência, como o Task Manager atual; o `% Processor Utility` escala pelo turbo), limitado a 0-100 | | | E1 (= Task Manager) |
| Uso por processador lógico (grade dimensionada pela CPU real; 16 = 4×4 na máquina de desenvolvimento) | PDH `\Processor Information(*)\% Processor Time` | | | E1 |
| Parcela de kernel (opcional) | `GetSystemTimes` (total do sistema): `(kernel - idle) / (kernel + user)`; KernelTime inclui IdleTime | | | E1 |
| Frequência atual | PDH `% Processor Performance` × frequência base | | | E1 |
| Frequência base | `CallNtPowerInformation(ProcessorInformation)` | | | E1 |
| Sockets, núcleos, lógicos, L1/L2/L3 | `GetLogicalProcessorInformationEx` | | | E1 |
| Virtualização | `IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)` | | | E1 |
| Processos, threads, handles | `GetPerformanceInfo` | | | E1 |
| Tempo ligado | `GetTickCount64` | | | E1 |
| Temperatura Tctl | PawnIO + `AMDFamily17`, SMN `0x59800` (bit 19 = faixa -49 °C), mutex `Global\Access_PCI` | A | "CPU não suportada" (não AMD ou fora de 17h-1Ah) | Spike (Zen 3; ±0,1 °C do Afterburner) |
| Temperatura por CCD | SMN por família/modelo; validado só Zen 3 Vermeer (19h, 20h-2Fh): `0x59954 + 4·n`, bit 11 = válido | A | "não validado nesta família" | Spike (Zen 3) |
| Potência do pacote (W) | MSR `0xC001029B` (energia, 32 bits com volta) e `0xC0010299` (unidade) | A | "CPU não suportada" | Spike (Zen 3) |

## Memória

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Em uso / total / % | `GlobalMemoryStatusEx` | | | E1 |
| Confirmada / limite | `GetPerformanceInfo` (CommitTotal, CommitLimit) | | | E1 |
| Em cache (Standby + Modified, igual ao Task Manager) | PDH `\Memory\Standby Cache Core Bytes`, `...Normal Priority Bytes`, `...Reserve Bytes`, `\Memory\Modified Page List Bytes` | | | E1 |
| Pool paginado / não paginado | `GetPerformanceInfo` (KernelPaged, KernelNonpaged) | | | E1 |
| Arquivo de paginação (uso) | PDH `\Paging File(_Total)\% Usage` | | | E1 |

## Discos (um painel por disco físico; membros do Storage Space sob o disco virtual)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Fabricante e modelo | `StorageDeviceProperty` (VendorId + ProductId) | | | Spike |
| Barramento (SATA, NVMe, Spaces) | `StorageDeviceProperty.BusType` (11, 17, 16) | | | Spike |
| SSD ou HDD | `StorageDeviceSeekPenaltyProperty` (vale também para o disco virtual) | | | Spike |
| Capacidade (unidade binária, igual ao Task Manager) | `IOCTL_DISK_GET_LENGTH_INFO` (handle com leitura) | A | "em repouso" (HDD parado na descoberta: não é aberto para leitura; nova tentativa a cada 60 s) | Spike |
| Membro de Storage Space | partição GPT "Storage Spaces protective" (`E75CAF8F-...`) por `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` | A | "vários pools: associação pendente" (com mais de um pool); "em repouso" (como a capacidade) | Spike (1 pool, 5 membros) |
| Letras das unidades | `GetLogicalDrives` (só unidades fixas e removíveis) + `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` em `\\.\X:` → números dos discos; relido a cada 30 s; membros do pool não têm letra | | | E1 |
| Disco do sistema / arquivo de paginação | letra do Windows (`GetSystemWindowsDirectoryW`) e dos pagefiles (`ExistingPageFiles` no registro) → disco pelas letras | | | E1 |
| Tempo ativo (%) | `IOCTL_DISK_PERFORMANCE` (handle sem acesso; não acorda o disco): `100 - Δidle/ΔQueryTime`, limitado a 0-100. Mesma fonte do PDH `\PhysicalDisk`, sem os ~54 MB que o provedor do PDH reserva | | | E1 (= PDH na mesma janela) |
| Leitura / escrita (B/s) | `IOCTL_DISK_PERFORMANCE`: Δ`BytesRead` / Δ`BytesWritten` por segundo | | | E1 (= PDH) |
| Tempo médio de resposta | `IOCTL_DISK_PERFORMANCE`: Δ(`ReadTime` + `WriteTime`) / Δ(`ReadCount` + `WriteCount`) | | | E1 (= PDH) |
| Temperatura NVMe (e alguns SATA) | `StorageDeviceTemperatureProperty` (52), primeira entrada diferente de "não reportado" | A | | Spike |
| Temperatura SATA | SMART por `IOCTL_STORAGE_PREDICT_FAILURE`, atributo 194 (senão 190) | A | "em repouso" (HDD parado: `GetDevicePowerState` consultado antes; nada é lido) | Spike (repouso real ainda não testado) |
| Temperatura do Storage Space | máximo dos membros | A | | Spike |

## Rede (uma linha por interface física)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome (alias) e descrição | `GetIfTable2` (`MIB_IF_ROW2.Alias`, `Description`) | | | E1 |
| Velocidade do link | `MIB_IF_ROW2.ReceiveLinkSpeed` | | "desconectada" | E1 |
| Envio / recebimento (bit/s) | delta de `OutOctets` / `InOctets`; filtro: `HardwareInterface`, tipo Ethernet/802.11, não `FilterInterface` | | | E1 |

## GPU (adaptador)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome, driver, VBIOS | NVML | | | Spike |
| Uso (%) | NVML `utilization.gpu` | | | Spike |
| Atividade da memória (%) | NVML `utilization.memory` | | | Spike |
| VRAM dedicada usada / total (igual ao Task Manager) | PDH `\GPU Adapter Memory(*)\Dedicated Usage` da instância `luid_0x<alto>_0x<baixo>_phys_N` (LUID pelo DXGI, casando fabricante/dispositivo/subsistema PCI com o NVML); total do NVML | | "várias GPUs iguais: associação pendente" | E1 |
| VRAM compartilhada | PDH `\GPU Adapter Memory(*)\Shared Usage`; limite pelo DXGI (`SharedSystemMemory`) | | | E1 |
| VRAM física (NVML: usada, livre, reservada) | NVML `memory_info` (v2, com `reserved`) | | | Spike |
| Temperatura e limite de slowdown | NVML | | | Spike |
| Clocks (gráfico, SM, memória, vídeo) e máximo | NVML | | | Spike |
| Potência e limite | NVML | | | Spike |
| Fans (%, RPM), todos os `num_fans` | NVML | | "parado" (0 RPM, modo silencioso) | Spike (2 fans) |
| PCIe: geração, largura, TX/RX | NVML (TX/RX custam ~23 ms cada: thread lenta) | | | Spike |
| Encoder / decoder (%) | NVML | | | Spike |
| P-state, motivos de throttling | NVML | | | Spike |
| Hotspot e temperatura da memória | não expostos pela RTX 5070 Ti / NVML | | fora do escopo | — |

## GPU por processo (aba GPU)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| VRAM local, não local (spill), paginada | PDH `\GPU Process Memory(*)`: `Local Usage`, `Non Local Usage`, `Total Committed` (paginada = max(committed - local - non local, 0)) | | | PS |
| Uso por engine (3D, Copy, Video Encode, Video Decode, Compute) | PDH `\GPU Engine(*)\Utilization Percentage` (query aberta só com a aba visível) | | | E6 |
| RAM e CPU % do processo | `SystemProcessInformation` | | | E6 |
| Runtimes (CUDA, Vulkan, D3D11/12, OpenGL, ONNX) | **sob demanda** (tecla no processo selecionado), módulos carregados; nunca no jogo ativo | A | "protegido" (acesso negado) | E6 |

## FPS (jogo em primeiro plano, via RTSS)

Fonte exata: pipe `\\.\pipe\RTSS_Frametime`, um registro `{dwApp, dwFrametime}` por frame
(`dwApp` = índice da entrada na Shared Memory; `dwFrametime` em µs). O hook do RTSS só conecta ao
pipe no início do jogo (confirmado). Com o jogo aberto antes do wintop, os
valores vêm da Shared Memory (amostras a cada 250 ms) e aparecem marcados como **"aprox. (jogo
aberto antes do wintop)"** até o jogo reiniciar.

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Jogo (nome, PID) | `RTSSSharedMemoryV2`: entrada ativa (período de FPS < 2 s) + API 3D + fora da lista de ignorados; primeiro plano desempata | | "nenhum jogo 3D em foco" | Spike (Deadlock; Spotify ignorado) |
| API 3D | `dwFlags & 0xFFFF` | | | Spike |
| Resolução | `dwResolutionX/Y` (v2.20+) | | | Spike |
| FPS | pipe (frames por janela); Shared Memory (`dwFrames / (dwTime1 - dwTime0)`) | | | Spike (pipe e SHM) |
| Frametime atual, mínimo, máximo | pipe; senão `dwFrameTime` da Shared Memory | | "aprox. (jogo aberto antes do wintop)" | Spike (pipe: 7435 frames do Deadlock) |
| 1% e 0,1% low | **calculado pelo wintop** por jogo, janela deslizante de 60 s; os campos do RTSS só existem no modo benchmark | | "aprox." (amostras da Shared Memory); "aguardando amostras" (< 100) | cálculo testado; janela de 60 s: E3 |
| Tempo de GPU / limitado por CPU ou GPU | `dwGpuActiveRenderTime`, `dwGpuFrameTime` (v2.21+) | | "indisponível neste jogo" (valor 0) | Spike (Deadlock: 3237/38726 µs) |
| RTSS rodando | o wintop inicia o RTSS (caminho em `HKLM\SOFTWARE\WOW6432Node\Unwinder\RTSS`) | A | | E3 |

## Processos (aba)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome, PID, CPU %, memória (working set, privada), threads, handles, I/O de leitura/escrita | `NtQuerySystemInformation(SystemProcessInformation)` (uma chamada) | A (para ver todos) | | E6 |

## Diagnóstico

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Estado de cada fonte (OK, falha e motivo) | coletores | | | E1 (`wintop --check`) |
| Consumo do próprio wintop (CPU, RAM) | `GetProcessTimes`, `GetProcessMemoryInfo` | | | E4 |
