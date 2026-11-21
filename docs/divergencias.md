# Divergências diagnosticadas

Toda métrica que não bater com a referência (Gerenciador de Tarefas, Afterburner, HWiNFO,
`nvidia-smi`) ganha uma entrada aqui: o que foi visto, a causa real, como foi verificado e o que
foi feito. Entradas novas no topo.

| Data | Métrica | Observado × referência | Causa | Verificação | Decisão |
| --- | --- | --- | --- | --- | --- |
| 2026-11-21 | Uso de CPU | wintop ~1,7× o Task Manager (19% × 12% em repouso; 100% × 84% em carga) | o `% Processor Utility` escala pela frequência (turbo 126% da base); o Task Manager atual usa o tempo ocupado padrão | Task Manager lido por UI Automation × PDH e `GetSystemTimes` no mesmo segundo; médias de 20 s depois da troca: 10,4% × 11,0% e 17,1% × 20,2% | trocado para `% Processor Time` (total e por lógico) |
| 2026-11-21 | Utilização da GPU | Task Manager 76-84% × NVML 5% | o Task Manager mostra o maior entre os engines (3D, Copy, Video Encode...); o NVML `utilization.gpu` é só 3D/compute; o encoder do NVML (74-97%) bate com o número do Task Manager | UI Automation + `--check` no mesmo instante, com gravação/stream ativo | manter o NVML e mostrar encoder/decoder; na Etapa 5 decidir se a TUI também mostra o máximo dos três, como o Task Manager |
| 2026-11-21 | VRAM dedicada (total) | 15,9 GB × 16,0 GB no Task Manager | o Task Manager mostra a VRAM física (DXGI 15,62 GiB + "Hardware reserved" 389 MB = 16 GiB); o NVML dá a área usável (16.303 MiB, igual ao `nvidia-smi` e ao registro) | DXGI `DedicatedVideoMemory`, registro `HardwareInformation.qwMemorySize`, `nvidia-smi` | manter o total do NVML (usado bate com o Task Manager: mesmo PDH) |
| 2026-11-21 | Nome do disco virtual | "Msft Storage Space" × "Microsoft Storage Space Device" | o Task Manager usa o nome amigável do PnP; o wintop, fabricante + produto do `StorageDeviceProperty` (nos discos físicos os dois coincidem) | UI Automation | resolvido na Etapa 2: nome amigável do PnP (SetupAPI) |
| 2026-11-21 | Versão do driver da GPU | 621.01 × 32.0.16.2101 | o Task Manager mostra a versão do pacote WDDM; o NVML, a versão NVIDIA (os 5 últimos dígitos) | registro `DriverVersion` | manter a versão NVIDIA (a que o usuário reconhece) |
| 2026-11-20 | Lows no fallback | 1% low ~5,0 FPS (Shared Memory) × 7,4 FPS (pipe) | 4 amostras/s do frametime instantâneo não pegam os piores frames | spike_rtss com o Deadlock (199 amostras × 7435 frames) | manter só como ordem de grandeza, sempre marcado "aprox." |
| 2026-11-20 | Frametime e lows com o jogo já aberto | pipe sem conexão | o hook do RTSS (no jogo) não conectou ao pipe criado depois do jogo; DACL e instâncias descartadas | cliente sem elevação e cliente ocioso + cliente ativo (spike_rtss) | Shared Memory a cada 250 ms, marcado "aprox. (jogo aberto antes do wintop)" |
| 2026-11-20 | Capacidade do disco | 512,1 GB × 477 GB no Task Manager | o Task Manager usa unidade binária (GiB) com o rótulo "GB" | spike_discos | exibir em unidade binária, como o Task Manager |
| 2026-11-20 | 1% / 0,1% low | RTSS devolve 0 | os campos são do período de estatística (modo benchmark, `STATFLAG_RECORD`) | header `RTSSSharedMemory.h` e leitura ao vivo | o wintop calcula a partir do pipe (ou da Shared Memory, aproximado) |
| 2026-11-20 | VRAM total usada | NVML ≠ Task Manager | NVML conta o frame buffer físico; o Task Manager mostra a memória dedicada do WDDM | revisão do Gemini Neon; a confirmar na Etapa 1 | VRAM principal pelo PDH `GPU Adapter Memory`; NVML como "física" |
