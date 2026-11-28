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
| Frequência atual (média) | PDH `% Processor Performance` (`_Total`) × frequência base | | | E1 |
| Frequência por lógico e pico | PDH `\Processor Information(*)\% Processor Performance` × frequência base; pico = maior entre os lógicos | | | 1.1 |
| Frequência base | `CallNtPowerInformation(ProcessorInformation)` | | | E1 |
| Sockets, núcleos, lógicos, L1/L2/L3 | `GetLogicalProcessorInformationEx` | | | E1 |
| Virtualização | `IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)` | | | E1 |
| Processos, threads, handles | `GetPerformanceInfo` | | | E1 |
| Tempo ligado | `GetTickCount64` | | | E1 |
| Temperatura Tctl | PawnIO + `AMDFamily17`, SMN `0x59800` (bit 19 = faixa -49 °C), mutex `Global\Access_PCI` | A | "CPU não suportada" (fabricante sem módulo ou AMD fora de 17h-1Ah) | E2 (wintop: ±0,2 °C do Afterburner no spike) |
| Temperatura do pacote (Intel) | PawnIO + `IntelMSR`: `IA32_PACKAGE_THERM_STATUS` (`0x1B1`, bits 22:16 abaixo do TjMax), lido com a thread presa num lógico de cada pacote; principal = pacote mais quente; sem o sensor de pacote (CPUID 6, bit 6) = núcleo mais quente | A | "CPU has no digital thermal sensor" (CPUID 6, bit 0) | 1.4.5 (sem hardware Intel: parsers e afinidade testados) |
| Temperatura por núcleo (Intel) | `IA32_THERM_STATUS` (`0x19C`, bit 31 = válido) com a thread presa no 1º lógico de cada núcleo (`SetThreadGroupAffinity`); TjMax de `MSR_TEMPERATURE_TARGET` (`0x1A2`, bits 23:16; 0 = 100 °C) lido por núcleo na abertura. Caixa da CPU: o mais quente; aba Sensores: todos | A | | 1.4.5 (idem) |
| Potência do pacote (Intel) | RAPL: `MSR_PKG_ENERGY_STATUS` (`0x611`, 32 bits com volta) × unidade de `MSR_RAPL_POWER_UNIT` (`0x606`, bits 12:8), somada por pacote | A | | 1.4.5 (idem) |
| Temperatura por CCD | SMN por família/modelo; validado só Zen 3 Vermeer (19h, 20h-2Fh): `0x59954 + 4·n`, bit 11 = válido | A | "não validado nesta família" | E2 (Zen 3) |
| Potência do pacote (W) | MSR `0xC001029B` (energia, 32 bits com volta) e `0xC0010299` (unidade) | A | "CPU não suportada" | E2 (Zen 3) |

### Outras CPUs (1.1, sem validação em hardware)

Regra do projeto: só entra na tabela `cpu_support` (`src/collect/pawnio.rs`) o que foi conferido
numa CPU real. A máquina de desenvolvimento tem só um Zen 3, então o estado atual é:

| CPU | Tctl e potência | CCD | Referência para validar (LibreHardwareMonitor, AMD PPR) |
| --- | --- | --- | --- |
| Zen 3 Vermeer (19h, 20h-2Fh) | sim | sim | validado no 5700X3D |
| Zen 2 (17h, 31h/71h) | sim (mesmos registradores de 17h-1Ah) | neutro "not validated on this CPU family" | CCD no mesmo `0x59954 + 4·n` do Zen 3 |
| Zen 4 (19h, 60h-7Fh) e Zen 5 (1Ah) | sim | neutro | CCD em `0x59B08 + 4·n` (outra base), bit 11 = válido |
| Intel (1.4.5) | pacote, cada núcleo e potência pelo módulo `IntelMSR` (ver a tabela acima) | — | primeira máquina: Xeon E5-2670 v3 (Haswell-EP) no Windows 10; conferir com o HWiNFO/LHM |

Para validar uma família: rodar `cargo run --example spike_pawnio` numa CPU dela, comparar com o
HWiNFO ou o LibreHardwareMonitor e só então acrescentar a linha em `cpu_support` (com teste).

## Memória

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Em uso / total / % | `GlobalMemoryStatusEx` | | | E1 |
| Comprimida em RAM e tamanho original estimado | `NtQuerySystemInformation(SystemStoreInformation = 109, sub = 22, ver = 3)`: `WorkingSetSize` e estimativa `WorkingSetSize × (TotalDataCompressed / TotalCompressedSize)` | | "disabled" (compressão desligada); "not supported by Windows"; "needs elevation" | 1.4.0 (= `Memory Compression` WorkingSet64) |
| Confirmada / limite / pico desde o boot | `GetPerformanceInfo` (`CommitTotal`, `CommitLimit`, `CommitPeak`) | | | E1 / 1.4.0 |
| Em cache (Standby + Modified, igual ao Task Manager) | `NtQuerySystemInformation(SystemMemoryListInformation = 80)`: soma de `PageCountByPriority[0..8]` (Standby) + `ModifiedPageCount + ModifiedNoWritePageCount` (Modified) × `PageSize` | | "needs elevation"; "not supported by Windows" | 1.4.0 (= PDH `\Memory\Standby Cache *` + `Modified Page List Bytes`) |
| Pool paginado / não paginado | `GetPerformanceInfo` (`KernelPaged`, `KernelNonpaged`) | | | E1 |
| Arquivo de paginação (uso) | `NtQuerySystemInformation(SystemPagefileInformation = 18)`: soma de `TotalInUse` / `TotalSize` × `PageSize` | | "no page file"; "needs elevation" | 1.4.0 (= PDH `\Paging File(_Total)\% Usage`) |
| Paginação real em disco (páginas lidas/gravadas por segundo) | `NtQuerySystemInformation(SystemPerformanceInformation = 2)`: Δ`PageReadCount` / Δ`DirtyPagesWriteCount` por segundo | | "waiting for a second sample" (1ª coleta); "needs elevation" | 1.4.0 (= PDH `\Memory\Pages Input/sec`, `Pages Output/sec`) |
| Hardware de RAM (geração, velocidade, slots, formato) | SMBIOS Type 17 via `GetSystemFirmwareTable('RSMB')` (lido uma vez na abertura) | | "not reported by firmware" | 1.4.0 |
| Reservada para hardware | `GetPhysicallyInstalledSystemMemory` − `ullTotalPhys` (lido uma vez na abertura) | | "not reported by firmware" | 1.4.0 |
| Páginas defeituosas (alerta silencioso se > 0) | `NtQuerySystemInformation(SystemMemoryListInformation = 80)` (`BadPageCount`) | | "needs elevation"; "not supported by Windows" | 1.4.0 |

## Discos (um painel por disco físico; membros do Storage Space sob o disco virtual)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Fabricante e modelo | nome amigável do PnP (SetupAPI, `GUID_DEVINTERFACE_DISK` + `IOCTL_STORAGE_GET_DEVICE_NUMBER`), igual ao Task Manager; sem ele, VendorId + ProductId do `StorageDeviceProperty` | | | E2 (= Task Manager) |
| Barramento (SATA, NVMe, Spaces) | `StorageDeviceProperty.BusType` (11, 17, 16) | | | Spike |
| SSD ou HDD | `StorageDeviceSeekPenaltyProperty` (vale também para o disco virtual) | | "n/a" (USB que não responde à consulta: pendrive, leitor de cartão, ponte USB) | Spike |
| Capacidade (unidade binária, igual ao Task Manager) | `IOCTL_DISK_GET_LENGTH_INFO` (handle com leitura); se recusado (pendrives), `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` (`DiskSize`) e depois `IOCTL_STORAGE_READ_CAPACITY` (`DiskLength`) | A | "em repouso" (HDD parado na descoberta: não é aberto para leitura; nova tentativa a cada 60 s); "empty" (`ERROR_NOT_READY`/`ERROR_NO_MEDIA_IN_DRIVE`: leitor sem cartão) | Spike |
| Membro de Storage Space | partição GPT "Storage Spaces protective" (`E75CAF8F-...`) por `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` | A | "em repouso" (como a capacidade) | Spike (1 pool, 5 membros) |
| Letras das unidades | `GetLogicalDrives` (só unidades fixas e removíveis) + `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` em `\\.\X:` → números dos discos; relido a cada 30 s; membros do pool não têm letra | | | E1 |
| Disco do sistema / arquivo de paginação | letra do Windows (`GetSystemWindowsDirectoryW`) e dos pagefiles (`ExistingPageFiles` no registro) → disco pelas letras | | | E1 |
| Tempo ativo (%) | `IOCTL_DISK_PERFORMANCE` (handle sem acesso; não acorda o disco): `100 - Δidle/ΔQueryTime`, limitado a 0-100. Mesma fonte do PDH `\PhysicalDisk`, sem os ~54 MB que o provedor do PDH reserva | | | E1 (= PDH na mesma janela) |
| Leitura / escrita (B/s) | `IOCTL_DISK_PERFORMANCE`: Δ`BytesRead` / Δ`BytesWritten` por segundo | | | E1 (= PDH) |
| Tempo médio de resposta | `IOCTL_DISK_PERFORMANCE`: Δ(`ReadTime` + `WriteTime`) / Δ(`ReadCount` + `WriteCount`) | | | E1 (= PDH) |
| IOPS e fila | `IOCTL_DISK_PERFORMANCE`: Δ(`ReadCount` + `WriteCount`) por segundo; `QueueDepth` do instante (colunas só com tela larga; sempre no `--check`) | | | 1.1 |
| Temperatura NVMe (e alguns SATA) | `StorageDeviceTemperatureProperty` (52), primeira entrada diferente de "não reportado"; a cada 10 s | A | | E2 (= `Get-StorageReliabilityCounter`) |
| Temperatura SATA | SMART por `IOCTL_STORAGE_PREDICT_FAILURE`, atributo 194 (senão 190), a cada 60 s | A | "em repouso" (HDD parado: `GetDevicePowerState` consultado antes; nada é lido); "n/a" (disco USB sem sensor nativo e com o SMART recusado antes de qualquer leitura boa; vale também para a previsão de falha) | E2 (= `Get-StorageReliabilityCounter`; repouso real ainda não testado) |
| Temperatura do Storage Space | máximo dos membros | A | "vários pools: associação pendente"; "em repouso" (todos os membros parados) | E2 |
| Previsão de falha (SMART) | `PredictFailure` de `IOCTL_STORAGE_PREDICT_FAILURE`, a cada 60 s, só com o disco acordado; falha prevista = aviso em vermelho | A | "not reported by the drive" (NVMe sem SMART pelo IOCTL); "see the member disks" (Storage Space) | 1.1 |

## Rede (uma linha por interface física)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome (alias) e descrição | `GetIfTable2` (`MIB_IF_ROW2.Alias`, `Description`) | | | E1 |
| Velocidade do link | `MIB_IF_ROW2.ReceiveLinkSpeed` | | "desconectada" | E1 |
| Envio / recebimento (bit/s) | delta de `OutOctets` / `InOctets`; filtro: `HardwareInterface`, tipo Ethernet/802.11, não `FilterInterface` | | | E1 |
| Erros de pacote (entrada, saída) | `MIB_IF_ROW2.InErrors` / `OutErrors` (desde que a interface subiu; só aparece quando há algum) | | | 1.1 |

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
| Clocks (gráfico, SM, memória, vídeo) e máximo | NVML | | máximo "não informado" (GPUs móveis): só o atual | Spike |
| Potência e limite | NVML | | limite "not reported by the driver" (GPUs móveis) | Spike |
| Fans (%, RPM), todos os `num_fans` | NVML | | "parado" (0 RPM, modo silencioso) | Spike (2 fans) |
| PCIe: geração, largura, TX/RX | NVML (TX/RX custam ~23 ms cada: thread lenta) | | | Spike |
| Encoder / decoder (%) | NVML | | | Spike |
| Engine mais usada fora o 3D e o vídeo (tipo e %: Copy, Compute, OFA...) | PDH `\GPU Engine(*)\Utilization Percentage` do LUID, mesma conta do uso sem NVML (soma dos processos por engine física, teto 100; maior por tipo). Consulta na thread rápida com qualquer GPU (~540 instâncias, ~1 ms por coleta na RTX 5070 Ti) | | "waiting for a second sample" (1ª coleta); "no other engine reported" | 1.1.1 (2026-11-22: Copy 0,04% parado) |
| P-state, motivos de throttling | NVML | | | Spike |
| Hotspot e temperatura da memória | não expostos pela RTX 5070 Ti / NVML | | fora do escopo | — |

### Várias GPUs e GPUs sem NVML (1.1)

A lista de GPUs tem as do NVML, na ordem dele, e depois os adaptadores DXGI de hardware que o NVML
não cobre (fabricante Microsoft, adaptador de software e LUID repetido ficam de fora). A tecla `g`
troca a GPU exibida nas abas Geral e GPU; a aba Sensores e o `--check` mostram todas.

| Métrica (GPU sem NVML) | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome, driver | DXGI `GetDesc1` e `CheckInterfaceSupport` (versão WDDM, ex. 32.0.15.8180) | | | E1.1 (RTX forçada sem NVML) |
| Uso (%) | PDH `\GPU Engine(*)\Utilization Percentage` do LUID: soma dos processos por engine física (teto 100), máximo entre as engines (conta do Task Manager). Consulta aberta na thread rápida (com qualquer GPU desde a 1.1.1) | | "waiting for a second sample" (1ª coleta) | E1.1: 2 pontos de diferença média do NVML com carga, 6 parado (picos de Copy) |
| Encoder / decoder (%) | mesmas engines: `*Encode*` / `*Decode*`; `VideoCodec` (AMD) vale para os dois | | "no video encode/decode engine reported" | E1.1 |
| VRAM dedicada, compartilhada, por categoria, processos | mesmo PDH das GPUs do NVML; total = DXGI `DedicatedVideoMemory` | | | E1.1 (idêntica ao caminho do NVML) |
| Campos só do NVML (física, temperatura, clocks, potência, fans, PCIe, P-state, throttling, VBIOS, atividade da memória) | — | | "needs NVML (NVIDIA only)" (AMD, Intel), numa linha só; NVIDIA com o NVML em falha mostra a falha | E1.1 |

Validação: teste ignorado `sem_nvml_bate_com_o_nvml` (`src/collect/gpu.rs`) força o NVML em falha
e compara os dois caminhos lado a lado na RTX 5070 Ti. Não há GPU AMD ou Intel na máquina de
desenvolvimento: o caminho delas é o mesmo, mas os nomes das engines de vídeo ("VideoCodec",
"VideoEncode") seguem a documentação do WDDM, sem prova em hardware.

## GPU por processo (aba GPU)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| VRAM local, não local (spill), paginada | PDH `\GPU Process Memory(*)`: `Local Usage`, `Non Local Usage`, `Total Committed` (paginada = max(committed - local - non local, 0)) | | | E6 (fps_probe: 256 MB alocados → 270 MB locais) |
| Uso por engine (3D, Copy, Video Encode, Video Decode, Compute) | PDH `\GPU Engine(*)\Utilization Percentage` (consulta aberta só com a aba visível; por processo e tipo, o maior entre as engines do tipo; coluna "GPU%" = engine mais usada, como o Task Manager) | | "waiting for a second sample" (aba recém-aberta) | E6 (fps_probe: engine 3D do PID do probe; ~800 instâncias em 2,4 ms) |
| RAM e CPU % do processo | `SystemProcessInformation` | | | E6 |
| Runtimes (CUDA, Vulkan, D3D9/10/11/12, OpenGL, OpenCL, ONNX, DirectML, NVENC/NVDEC) | **sob demanda** (Enter no processo selecionado), módulos carregados pelo Toolhelp; nunca no jogo ativo nem em PID com o hook do RTSS | A | "protected" (acesso negado); "skipped: 3D app hooked by RTSS" | E6 |

## FPS (jogo em primeiro plano, via ETW e RTSS)

O RTSS escolhe o jogo (app 3D em primeiro plano) e dá API, resolução e tempo de GPU. Os frames
vêm, em ordem de preferência:

1. **ETW** (sessão em tempo real `wintop-fps-s<sessão>`, lógica do PresentMon): DXGI
   `Present_Start` do PID do jogo (filtro de PID no provider), frametime = intervalo entre dois
   Present da mesma swap chain (a principal é a de mais frames no último segundo). Exato também
   com o jogo aberto antes do wintop; D3D10/11/12. Flush de 100 ms
   (`EVENT_TRACE_USE_MS_FLUSH_TIMER`): as contas usam "agora - 250 ms". Marcado "ETW: exact".
   Marcadores Reflex do `PCLStatsTraceLoggingProvider` (`0d216f06-82a6-4d49-bc4f-8f38ae56efab`,
   `pclstats.h` do Streamline): `SIMULATION_START` (0) por frame real e `NUM_PRESENTS_IN_BATCH`
   (22, `Value` = multiplicador). Ligar o provider faz o jogo emitir o ping de latência (tecla
   F13-F15 ou mensagem para a própria janela a cada 100-300 ms), como com o overlay da NVIDIA.
2. Pipe do RTSS (jogos sem frames por ETW: Vulkan, OpenGL, D3D9), marcado "pipe: exact".
3. Shared Memory do RTSS, aproximado.

Pipe do RTSS: `\\.\pipe\RTSS_Frametime`, um registro `{dwApp, dwFrametime}` por frame
(`dwApp` = índice da entrada na Shared Memory; `dwFrametime` em µs). O hook do RTSS só conecta ao
pipe no início do jogo (confirmado). Com o jogo aberto antes do wintop, os
valores vêm da Shared Memory (amostras a cada 250 ms) e aparecem marcados como **"aprox. (jogo
aberto antes do wintop)"** até o jogo reiniciar.

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Jogo (nome, PID) | `RTSSSharedMemoryV2`: entrada ativa (período de FPS < 2 s) + API 3D + fora da lista de ignorados; primeiro plano desempata | | "nenhum jogo 3D em foco" | E3 (sem jogo: "no 3D game in focus"; pipe com jogo: pendente) |
| API 3D | `dwFlags & 0xFFFF` | | | Spike |
| Resolução | `dwResolutionX/Y` (v2.20+) | | | Spike |
| FPS | pipe (frames por janela); Shared Memory (`dwFrames / (dwTime1 - dwTime0)`) | | | Spike (pipe e SHM) |
| Frametime atual, mínimo, máximo | pipe; senão `dwFrameTime` da Shared Memory | | "aprox. (jogo aberto antes do wintop)"; "sem frames no último segundo" (travamento ou tela de carregamento) | Spike (pipe: 7435 frames do Deadlock); E6 (fps_probe, frametime pelo pipe: 10-50 ms programados = 8,5-50,3 ms medidos, diferença do jitter do próprio probe); congelamento de 1,5 s: FPS cai já nas amostras de 250 ms (68 → 46 → 20) e depois "sem frames no último segundo"; probe aberto antes: "aprox.", FPS 92,14 × 92,59 |
| 1% e 0,1% low | **calculado pelo wintop** por jogo, janela deslizante de 60 s; os campos do RTSS só existem no modo benchmark | | "aprox." (amostras da Shared Memory); "aguardando amostras" (< 100) | E3 (janela de 60 s, recalculada a cada 1 s); E6 (fps_probe: 1% low 19,96 e 0,1% low 19,90 × 20,00 esperado; FPS 92,62 × 92,59) |
| FPS real e multiplicador (frame gen) | ETW: `SIMULATION_START` do PCL no último segundo; multiplicador = `Value` do marcador 22 visto nos últimos 2 s (≥ 2); sem multiplicador, apresentados/reais ≥ 1,6 | A | sem frame gen (jogo sem Reflex ou FG desligado): não aparece | Spike (ZZZ, MFG 4x: DXGI ~190/s, SIM_START ~47/s, `Value` 4); 1.2 (fps_probe `--fg 4` com o GUID e os campos reais: FPS 92,10 × 92,59, real 22,91 × 23,15, multiplicador 4) |
| Sessão ETW dos frames | `StartTrace` + `EnableTraceEx2` (DXGI e PCL, filtro de PID) | A | "not started in --check" | 1.2 (fps_probe aberto antes do wintop: "ETW: exact", FPS 92,75 × 92,59; entrega mediana ~160 ms) |
| Tempo de GPU / limitado por CPU ou GPU | `dwGpuActiveRenderTime`, `dwGpuFrameTime` (v2.21+) | | "indisponível neste jogo" (valor 0) | Spike (Deadlock: 3237/38726 µs) |
| RTSS rodando | o wintop inicia o RTSS (caminho em `HKLM\SOFTWARE\WOW6432Node\Unwinder\RTSS`) | A | | E3 (só no modo normal; o `--check` não inicia o RTSS nem cria o pipe) |

## Processos (aba)

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Nome, PID, CPU %, memória (working set privado, como a coluna "Memory" do Task Manager; commit privado), threads, handles, I/O de leitura/escrita por segundo | `NtQuerySystemInformation(SystemProcessInformation)` (uma chamada, sem abrir nenhum processo; thread lenta, só com a aba Processos ou GPU visível); CPU = Δ(kernel + user) ÷ Δ tempo do sistema | | "waiting for the first collection" (aba recém-aberta) | E6 (carga conhecida: 1 lógico ocupado = 6,25% × 6,25% esperado; 64 MB gravados = 64,0 MB; 512 MB alocados = +512 MB de working set privado) |

## Sensores (aba)

Todas as leituras físicas já coletadas para a aba geral, numa tabela com valor atual, mínimo e
máximo da sessão (`r` zera): CPU (Tctl, CCDs, potência do pacote, clock), GPU (temperatura,
limite de slowdown, potência e limite, clocks, fans em % e RPM, P-state, throttling, link PCIe) e
temperatura de cada disco. Temperaturas coloridas conforme a proximidade do limite (como na aba Geral). Mesmas fontes e estados neutros das
seções acima; nenhuma fonte nova. Prova: E6 (aba conferida em tela cheia).

## Diagnóstico

| Métrica | Fonte | Elev. | Estado neutro | Prova |
| --- | --- | --- | --- | --- |
| Estado de cada fonte (OK, falha e motivo) | coletores | | | E1 (`wintop --check`) |
| Consumo do próprio wintop (CPU, RAM) | `GetProcessTimes` ÷ tempo total do sistema (`GetSystemTimes`); `GetProcessMemoryInfo` (`PROCESS_MEMORY_COUNTERS_EX2`: working set privado = coluna "Memory" do Task Manager, e memória confirmada; antes do Windows 11 22H2 o working set privado vem da lista de processos) | | | E4 (= contadores do Windows: 0,015% × 0,014% em 120 s; 25,8 MB × 25,8 MB) |
