# Spike: frame generation (DLSS FG) sem o Afterburner

Ferramenta: `examples/spike_framegen.rs` (`cargo run --release --example spike_framegen
[segundos] [pid]`, terminal elevado). Data: 2026-11-22. Máquina: RTX 5070 Ti, driver NVIDIA
atual, RTSS v2.21.

## Ensaios

| Alvo | FG | Resultado |
| --- | --- | --- |
| `fps_probe` (D3D11, 100 FPS, sem Reflex) | não | DXGI, DxgKrnl, eventos do driver NVIDIA e RTSS: 96/s em todos |
| Zenless Zone Zero (D3D12, Reflex, DLSS FG) | desligado (menus) | ~160 FPS: DXGI = marcadores Reflex = RTSS |
| Zenless Zone Zero | ligado (gameplay, MFG 4x) | DXGI ~190/s; marcadores Reflex ~47/s; razão exata 4 |

## Fontes encontradas

- **`PCLStatsTraceLoggingProvider`** (TraceLogging, registrado pelo jogo que usa Reflex):
  - `PCLStatsEvent` (`Marker`, `FrameID`): marcadores do Reflex por frame **real**
    (`SIM_START` = 0 ... `PRESENT_END` = 5, `PING` = 8). Com FG, aparecem também
    `OUT_OF_BAND_PRESENT_START/END` (11/12), um por frame **apresentado** (reais + gerados).
  - `PCLStatsEventV3` (`Marker`, `FrameID`, `Value`): só com FG; `Marker = 22`, um por frame
    real, com **`Value` = multiplicador do FG** (4 no MFG 4x).
  - `PCLStatsInput` (`MsgId`): entrada do usuário (~5/s).
- **`NV-wgf2um-tracelogger`** (TraceLogging do driver; existe mesmo sem Reflex no jogo):
  `PresentCBInfo` (`FrameID`, `FrameNumber`) por frame apresentado; `ReflexLatencyInfo`,
  `RenderLatencyInfo`, `ReflexSleep`, `ReflexState` (`LowLatencyMode`, `FullScreen`). Com FG:
  `AsyncRenderLatencyInfo` (`GPUFrameTimeUs`, `AIFrameTimeUs` = custo da geração, ~1,7 ms).
  `ReflexSimStartInfo` vem duas vezes por frame sem FG (driver + jogo) e uma com FG: não serve
  para contar.
- **DXGI** `Present_Start` (id 42, filtro de PID): todos os frames apresentados, inclusive os
  gerados (o Streamline apresenta cada um).
- **DxgKrnl** `Present/Info`: igual ao DXGI; provider de kernel (sem filtro de PID), muito
  verboso com as keywords usadas aqui.
- **RTSS Shared Memory v2.21**: o FPS do RTSS conta os frames apresentados (~190 com FG);
  `qwSimStartTime`/`qwPresentStartTime` mudam só nos frames reais (~47/s), mas só dá para contar
  lendo a cada 1 ms.

## Custo (CPU, % do sistema inteiro, 90 s com o jogo)

- Consumidor ETW do spike (decodifica tudo pelo TDH, inclui DxgKrnl e providers sem nome):
  0,101%. Um consumidor enxuto (DXGI com filtro de PID + PCL, campos lidos direto) deve ficar
  bem abaixo.
- Leitura do RTSS a 1 ms: 0,041% (polling, perde frames acima de ~500 FPS reais).
- Custo dentro do jogo e do kernel não medido.

## Conclusão

FPS total = DXGI `Present_Start` do PID (ou `OUT_OF_BAND_PRESENT_START` do PCL com FG); FPS real =
`SIM_START` do `PCLStatsEvent`; multiplicador = `Value` do marcador 22 do `PCLStatsEventV3`.
Frame gen exige Reflex, então o PCL existe sempre que há FG. Sem PCL (jogo sem Reflex), não há
FG da NVIDIA e o total basta. O RTSS continua de reserva (jogos sem ETW utilizável, Vulkan/OpenGL
a confirmar).
