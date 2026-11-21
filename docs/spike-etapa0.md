# Spike de viabilidade (Etapa 0)

Executado em 2026-11-20, elevado, na máquina de desenvolvimento: Ryzen 7 5700X3D (família 19h,
modelo 21h), ASUS TUF GAMING X570-PLUS, RTX 5070 Ti (driver 621.01), Windows 11 Pro for
Workstations, RTSS 7.3.7, PawnIO 2.2.0, PawnIO.Modules 0.2.12. Reexecutado após as correções da
revisão Neon + Argon.

Cada spike é um programa em `examples/` (`cargo run --example <nome>`, terminal elevado). Eles
não fazem parte do wintop; servem para provar cada fonte antes dos coletores definitivos.

**Escopo do que foi provado em Rust:** PawnIO, discos, NVML e RTSS. PDH (CPU, discos, GPU por
processo), `GetIfTable2` e `SystemProcessInformation` só foram conferidos pelo PowerShell (mesmas
APIs) e são provados na Etapa 1.

## 1. Temperatura e potência da CPU (`spike_pawnio`)

| Item | Resultado |
| --- | --- |
| Acesso ao PawnIO | `PawnIOLib.dll` (2.0.0) pelo caminho de `InstallLocation` (sem fallback fixo); módulo `AMDFamily17.bin` carregado em 1,5 ms |
| Sem elevação | `\\?\GLOBALROOT\Device\PawnIO`: acesso negado (esperado) |
| Tctl (SMN `0x59800`) | 1ª execução: 53,6-54,1 °C × Afterburner 53,6-54,2 °C; 2ª: 58,8-60,0 °C × 58,9-60,0 °C (diferença ≤ 0,2 °C) |
| CCD1 (SMN `0x59954`) | 43-54 °C (a 5700X3D tem um CCD) |
| Potência do pacote (MSR) | 44 a 53 W (unidade 1,526e-5 J por contagem) |
| Custo por ciclo (Tctl + 2 CCD + 1 MSR) | 107 a 305 µs |
| Carregamento no boot | o PawnIO é um dispositivo PnP (`ROOT\PAWNIO\0000`); **pendente confirmar após um reboot** |

Pontos para a implementação:

- Validado só no Zen 3 Vermeer (19h, modelo 21h). Tctl e potência usam o mesmo caminho em 17h-1Ah;
  o offset dos CCDs muda entre gerações e fica numa tabela por família/modelo (só entra o que for
  conferido numa CPU real).
- Vendor `AuthenticAMD` checado; folha estendida do CPUID checada antes de ler o nome.
- Mutex `Global\Access_PCI` em volta de cada leitura SMN (o mesmo de HWiNFO e LibreHardwareMonitor),
  criado com DACL que deixa monitores sem elevação usá-lo.
- `pawnio_execute` com sucesso mas sem valor escrito é erro, não "0".
- O módulo é LGPL-2.1: vai em arquivo separado (`modules/`), com o código-fonte em `modules/source/`.

## 2. Discos (`spike_discos`)

| Disco | Modelo | Barramento | Tipo | Capacidade | Pool | Temperatura | Fonte | Custo |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 (E:) | Netac SSD 512GB | SATA | SSD | 476,9 GB | | 38-39 °C | SMART 194 | 2-37 ms |
| 1 | HGST HCC545050A7E380 | SATA | HDD | 465,8 GB | membro | 35-36 °C | SMART 194 | 2-64 ms |
| 2 | ST3500418AS | SATA | HDD | 465,8 GB | membro | 37-38 °C | SMART 194 | 35-42 ms |
| 3 | WDC WD10SPZX-75Z10T0 | SATA | HDD | 931,5 GB | membro | 35-36 °C | SMART 194 | 16-156 ms |
| 4 | ST3500413AS | SATA | HDD | 465,8 GB | membro | 39-41 °C | SMART 194 | 49-58 ms |
| 5 | ST1000DM010-2EP102 | SATA | HDD | 931,5 GB | membro | 38-39 °C | IOCTL nativo | 38-63 ms |
| 6 (D:) | NV900-1T | NVMe | SSD | 931,5 GB | | 45 °C | IOCTL nativo | 5 ms |
| 7 (C:) | NE-512 | NVMe | SSD | 476,9 GB | | 47-48 °C | IOCTL nativo | 0,3 ms |
| 8 (F:) | Msft Storage Space | Spaces | HDD | 3147,0 GB | virtual | 41 °C | máximo dos membros 1-5 | — |

Conferido com `Get-StorageReliabilityCounter` (diferença ≤ 1 °C). Capacidades em unidade binária,
como o Gerenciador de Tarefas.

Pontos para a implementação:

- `StorageDeviceTemperatureProperty` responde nos NVMe e em um HDD; nos demais SATA devolve
  `ERROR_INVALID_FUNCTION` e o SMART cobre. Entradas com o sentinela "não reportado" (-32768) são
  ignoradas.
- **Toda** leitura de temperatura de disco (nativa ou SMART) leva até 156 ms: roda na thread lenta.
- Membros do Storage Space: identificados sem WMI pela partição GPT "Storage Spaces protective"
  (`E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D`). Com um pool, o disco virtual mostra o máximo dos
  membros; com vários, a associação por pool fica pendente (exige o ID do pool).
- `GetDevicePowerState` é consultado antes; HDD em repouso não é lido (estado "em repouso").
  Nenhum HDD estava parado nos testes: o comportamento real com disco parado ainda não foi visto.
- Capacidade e layout exigem um handle com leitura (com acesso 0 o IOCTL falha).

## 3. RTSS (`spike_rtss`)

| Item | Resultado |
| --- | --- |
| Shared Memory | `RTSSSharedMemoryV2`, versão 2.21, view de 5.578.752 bytes |
| Layout | offsets do compilador MSVC a partir do header oficial; `sizeof(APP_ENTRY)` = 12416 = `dwAppEntrySize`; campos v2.16/v2.20/v2.21 lidos só se versão e tamanho cobrirem |
| Assinatura | comparada como `u32` (`0x52545353`); `0xDEAD` = RTSS encerrando (reconectar) |
| Com jogo (Deadlock) | API D3D10 (`0x6`), 2560x1440, 33,5 FPS, GPU ativa 3237 µs / frame de GPU 38726 µs; lows do RTSS = 0 (só no modo benchmark) |
| Seleção do jogo | ativo (período < 2 s) + API 3D + lista de ignorados; com o Spotify (OpenGL, 100 FPS) em primeiro plano, ele é ignorado |
| Pipe, DACL | cliente sem elevação (`runas /trustlevel:0x20000`, integridade média) conecta e escreve com a DACL `GRGW` para o usuário |
| Pipe, instâncias | 8 instâncias: um cliente ocioso segura uma e outro cliente conecta e entrega os frames por outra |
| Pipe, jogo aberto antes | nenhuma conexão em 60 s (o hook só conecta no início do jogo) |

Teste com o jogo aberto **depois** do servidor (Deadlock, 90 s):

| Item | Resultado |
| --- | --- |
| Conexão | o hook conecta quando o jogo inicia: `deadlock.exe` (PID 2768) conectou logo ao abrir; com o jogo aberto antes do servidor, não conecta |
| Outros clientes | o overlay da Steam (`gameoverlayui64.exe`) também conectou, sem mandar frames: o servidor precisa de várias instâncias |
| `dwApp` | **índice da entrada** no array de apps da Shared Memory (6 = 7ª entrada = `deadlock.exe`), não o PID |
| `dwFrametime` | microssegundos (232 frames/s × 4,32 ms médio ≈ 1,0 s) |
| Volume | 7435 frames em 90 s (até 232 frames/s), sem perda nem bytes descartados |
| Lows exatos (90 s, com carregamento) | 1% low 7,4 FPS, 0,1% low 1,3 FPS (máximo de 1486 ms no carregamento) |
| Fallback (Shared Memory, 199 amostras) | 1% low ~5,0 FPS: dá a ordem de grandeza, não os piores frames |
| v2.21 | GPU ativa 3094 µs / frame de GPU 3122 µs com frametime de 4,47 ms |

Fallback aprovado pelo usuário: com o jogo aberto antes do wintop, frametime e lows vêm de
amostras da Shared Memory a cada 250 ms, marcados como "aprox."; o spike já faz essa amostragem
e calcula os lows aproximados ao lado dos exatos.

## 4. NVML (`spike_nvml`)

33 consultas (3 do sistema e 30 da GPU), todas suportadas na RTX 5070 Ti (driver 621.01, NVML
13.621.01): nome, PCI, temperatura e limite de slowdown, utilização (GPU e memória), memória v2
(com `reserved`), BAR1, quatro clocks e o máximo, potência e limite, energia total, fans (os 2,
% e RPM), PCIe (geração 4, x16, TX/RX), encoder, decoder, P-state, motivos de throttling,
processos gráficos e compute, VBIOS.

- `nvml.dll` pelo caminho de `GetSystemDirectoryW` (não pela variável `SystemRoot`) e busca de
  DLLs restrita a System32.
- Inicialização: 17 ms. Leituras comuns: 0 a 1,6 ms. PCIe TX/RX: ~23 ms cada → thread lenta.
- A VRAM por processo continua indisponível no NVML sob WDDM (`[N/A]`): vem do PDH.

## Conclusão

Provados em Rust: PawnIO (Zen 3), temperatura e inventário dos 9 discos com os membros do pool,
NVML, a Shared Memory do RTSS e o pipe de frametime (jogo aberto depois do servidor).
Pendentes: o PawnIO após um reboot e um HDD realmente em repouso. PDH, rede e processos são
provados na Etapa 1.
