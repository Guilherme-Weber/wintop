# Divergências diagnosticadas

Toda métrica que não bater com a referência (Gerenciador de Tarefas, Afterburner, HWiNFO,
`nvidia-smi`) ganha uma entrada aqui: o que foi visto, a causa real, como foi verificado e o que
foi feito. Entradas novas no topo.

| Data | Métrica | Observado × referência | Causa | Verificação | Decisão |
| --- | --- | --- | --- | --- | --- |
| 2026-11-20 | 1% / 0,1% low | RTSS devolve 0 | os campos são do período de estatística (modo benchmark, `STATFLAG_RECORD`) | header `RTSSSharedMemory.h` e leitura ao vivo | o wintop calcula a partir do pipe de frametime |
| 2026-11-20 | VRAM total usada | NVML ≠ Task Manager | NVML conta o frame buffer físico; o Task Manager mostra a memória dedicada do WDDM | revisão do Gemini Neon; a confirmar na Etapa 3 | VRAM principal pelo PDH `GPU Adapter Memory`; NVML como "física" |
