# Inventário de métricas

Lista fechada do que o wintop mostra. Regra do projeto: **aberto elevado, toda métrica desta
lista aparece**. Uma métrica sem dado é exibida em vermelho com a causa (falha); só os casos da
coluna "Estado neutro" podem aparecer sem valor, e sempre com o texto do estado.

Origem: captura do setup anterior (Gerenciador de Tarefas + MSI Afterburner), detalhamento de VRAM
do gputaskman e detalhamento de disco do Gerenciador de Tarefas.

Legenda da coluna Elev.: **A** = exige administrador (o wintop sempre roda elevado).
Coluna Spike: resultado da Etapa 0 (`docs/spike-etapa0.md`); "E1".."E6" = etapa que implementa.

Todas as séries com histórico guardam **mínimo e máximo da sessão** (tecla para zerar), como o
Afterburner.

## CPU

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Nome do processador | CPUID (0x80000002-4) | | | OK |
| Uso total (%) | PDH `\Processor Information(_Total)\% Processor Utility`, limitado a 0-100 | | | E1 |
| Uso por thread lógico (16, grade 4×4, 60 s) | PDH `\Processor Information(*)\% Processor Utility` | | | E1 |
| Parcela de kernel (opcional) | `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`; KernelTime inclui IdleTime | | | E1 |
| Frequência atual | PDH `% Processor Performance` × frequência base | | | E1 |
| Frequência base | `CallNtPowerInformation(ProcessorInformation)` | | | E1 |
| Sockets, núcleos, lógicos, L1/L2/L3 | `GetLogicalProcessorInformationEx` | | | E1 |
| Virtualização | `IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)` | | | E1 |
| Processos, threads, handles | `GetPerformanceInfo` | | | E1 |
| Tempo ligado | `GetTickCount64` | | | E1 |
| Temperatura Tctl | PawnIO + `AMDFamily17`, SMN `0x59800` (bit 19 = faixa -49 °C), mutex `Global\Access_PCI` | A | | **OK** (±0,2 °C do Afterburner) |
| Temperatura por CCD | SMN `0x59954 + 4·n`, bit 11 = válido | A | | **OK** (CCD1) |
| Potência do pacote (W) | MSR `0xC001029B` (energia, 32 bits com volta) e `0xC0010299` (unidade) | A | | **OK** |

## Memória

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Em uso / total / % | `GlobalMemoryStatusEx` | | | E1 |
| Confirmada / limite | `GetPerformanceInfo` (CommitTotal, CommitLimit) | | | E1 |
| Em cache (Standby + Modified, igual ao Task Manager) | PDH `\Memory\Standby Cache Core Bytes`, `...Normal Priority Bytes`, `...Reserve Bytes`, `\Memory\Modified Page List Bytes` | | | E1 |
| Pool paginado / não paginado | `GetPerformanceInfo` (KernelPaged, KernelNonpaged) | | | E1 |
| Arquivo de paginação (uso) | PDH `\Paging File(_Total)\% Usage` | | | E1 |

## Discos (um painel por disco físico; membros do Storage Space sob o disco virtual)

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Modelo | `IOCTL_STORAGE_QUERY_PROPERTY` / `StorageDeviceProperty` | | | **OK** (9 discos) |
| Barramento (SATA, NVMe, Spaces) | `StorageDeviceProperty.BusType` (11, 17, 16) | | | **OK** |
| SSD ou HDD | `StorageDeviceSeekPenaltyProperty` (vale também para o disco virtual) | | | **OK** |
| Capacidade | `IOCTL_DISK_GET_LENGTH_INFO` (handle com leitura) | A | | **OK** |
| Letras das unidades | nome da instância PDH (`"7 c:"`); membros do pool não têm letra | | | E1 |
| Disco do sistema / arquivo de paginação | volume do Windows e do pagefile → número do disco | | | E1 |
| Tempo ativo (%) | PDH `\PhysicalDisk(*)\% Idle Time` → `100 - idle`, limitado a 0-100 | | | E1 |
| Leitura / escrita (B/s) | PDH `Disk Read Bytes/sec`, `Disk Write Bytes/sec` | | | E1 |
| Tempo médio de resposta | PDH `Avg. Disk sec/Transfer` | | | E1 |
| Temperatura NVMe | `StorageDeviceTemperatureProperty` (52) | A | | **OK** |
| Temperatura SATA (SSD e HDD) | SMART por `IOCTL_STORAGE_PREDICT_FAILURE`, atributo 194 (senão 190) | A | "em repouso" (HDD parado, `GetDevicePowerState`) | **OK** |
| Temperatura do Storage Space | máximo dos membros | A | | **OK** (por agregação) |

## Rede (uma linha por interface física)

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Nome (alias) e descrição | `GetIfTable2` (`MIB_IF_ROW2.Alias`, `Description`) | | | E1 |
| Velocidade do link | `MIB_IF_ROW2.ReceiveLinkSpeed` | | "desconectada" | E1 |
| Envio / recebimento (bit/s) | delta de `OutOctets` / `InOctets`; filtro: `HardwareInterface`, tipo Ethernet/802.11, não `FilterInterface` | | | E1 |

## GPU (adaptador)

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Nome, driver, VBIOS | NVML | | | **OK** |
| Uso (%) | NVML `utilization.gpu` | | | **OK** |
| Atividade da memória (%) | NVML `utilization.memory` | | | **OK** |
| VRAM dedicada usada / total (igual ao Task Manager) | PDH `\GPU Adapter Memory(*)\Dedicated Usage`; total do NVML | | | E3 |
| VRAM compartilhada | PDH `\GPU Adapter Memory(*)\Shared Usage` | | | E3 |
| VRAM física (NVML: usada, livre, reservada) | NVML `memory_info` | | | **OK** |
| Temperatura e limite de slowdown | NVML | | | **OK** |
| Clocks (gráfico, SM, memória, vídeo) e máximo | NVML | | | **OK** |
| Potência e limite | NVML | | | **OK** |
| Fans (%, RPM) | NVML | | "parado" (0 RPM, modo silencioso) | **OK** |
| PCIe: geração, largura, TX/RX | NVML (TX/RX custam ~23 ms cada: thread lenta) | | | **OK** |
| Encoder / decoder (%) | NVML | | | **OK** |
| P-state, motivos de throttling | NVML | | | **OK** |
| Hotspot e temperatura da memória | não expostos pela RTX 5070 Ti / NVML | | fora do escopo | — |

## GPU por processo (aba GPU)

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| VRAM local, não local (spill), paginada | PDH `\GPU Process Memory(*)`: `Local Usage`, `Non Local Usage`, `Total Committed` (paginada = max(committed - local - non local, 0)) | | | E6 |
| Uso por engine (3D, Copy, Video Encode, Video Decode, Compute) | PDH `\GPU Engine(*)\Utilization Percentage` (query aberta só com a aba visível) | | | E6 |
| RAM e CPU % do processo | `SystemProcessInformation` | | | E6 |
| Runtimes (CUDA, Vulkan, D3D11/12, OpenGL, ONNX) | **sob demanda** (tecla no processo selecionado), módulos carregados; nunca no jogo ativo | A | "protegido" (acesso negado) | E6 |

## FPS (jogo em primeiro plano, via RTSS)

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Jogo (nome, PID) | `RTSSSharedMemoryV2`: entrada com atividade recente + API 3D, primeiro plano como desempate, lista de ignorados | | "nenhum jogo 3D em foco" | **OK** (layout v2.21) |
| API 3D | `dwFlags & 0xFFFF` | | | **OK** |
| Resolução | `dwResolutionX/Y` (v2.20) | | | **OK** |
| FPS | pipe `\\.\pipe\RTSS_Frametime` (frames por janela); Shared Memory como reserva | | | pipe: **pendente com jogo** |
| Frametime atual, mínimo, máximo | pipe (por frame) | | | pendente com jogo |
| 1% e 0,1% low | **calculado pelo wintop** (janela de 60 s); os campos do RTSS só existem no modo benchmark | | | lógica OK; dados pendentes |
| Tempo de GPU / limitado por CPU ou GPU | `dwGpuActiveRenderTime`, `dwGpuFrameTime` (v2.21) | | "indisponível neste jogo" (valor 0) | pendente com jogo |
| RTSS rodando | o wintop inicia o RTSS (caminho em `HKLM\SOFTWARE\WOW6432Node\Unwinder\RTSS`) | A | | E3 |

## Processos (aba)

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Nome, PID, CPU %, memória (working set, privada), threads, handles, I/O de leitura/escrita | `NtQuerySystemInformation(SystemProcessInformation)` (uma chamada) | A (para ver todos) | | E6 |

## Diagnóstico

| Métrica | Fonte | Elev. | Estado neutro | Spike |
| --- | --- | --- | --- | --- |
| Estado de cada fonte (OK, falha e motivo) | coletores | | | E1 (`wintop --check`) |
| Consumo do próprio wintop (CPU, RAM) | `GetProcessTimes`, `GetProcessMemoryInfo` | | | E4 |
