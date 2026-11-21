# Divergências diagnosticadas

Toda métrica que não bater com a referência (Gerenciador de Tarefas, Afterburner, HWiNFO,
`nvidia-smi`) ganha uma entrada aqui: o que foi visto, a causa real, como foi verificado e o que
foi feito. Entradas novas no topo.

| Data | Métrica | Observado × referência | Causa | Verificação | Decisão |
| --- | --- | --- | --- | --- | --- |
| 2026-11-20 | Lows no fallback | 1% low ~5,0 FPS (Shared Memory) × 7,4 FPS (pipe) | 4 amostras/s do frametime instantâneo não pegam os piores frames | spike_rtss com o Deadlock (199 amostras × 7435 frames) | manter só como ordem de grandeza, sempre marcado "aprox." |
| 2026-11-20 | Frametime e lows com o jogo já aberto | pipe sem conexão | o hook do RTSS (no jogo) não conectou ao pipe criado depois do jogo; DACL e instâncias descartadas | cliente sem elevação e cliente ocioso + cliente ativo (spike_rtss) | Shared Memory a cada 250 ms, marcado "aprox. (jogo aberto antes do wintop)" |
| 2026-11-20 | Capacidade do disco | 512,1 GB × 477 GB no Task Manager | o Task Manager usa unidade binária (GiB) com o rótulo "GB" | spike_discos | exibir em unidade binária, como o Task Manager |
| 2026-11-20 | 1% / 0,1% low | RTSS devolve 0 | os campos são do período de estatística (modo benchmark, `STATFLAG_RECORD`) | header `RTSSSharedMemory.h` e leitura ao vivo | o wintop calcula a partir do pipe (ou da Shared Memory, aproximado) |
| 2026-11-20 | VRAM total usada | NVML ≠ Task Manager | NVML conta o frame buffer físico; o Task Manager mostra a memória dedicada do WDDM | revisão do Gemini Neon; a confirmar na Etapa 1 | VRAM principal pelo PDH `GPU Adapter Memory`; NVML como "física" |
