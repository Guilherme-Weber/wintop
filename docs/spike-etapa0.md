# Spike de viabilidade (Etapa 0)

Executado em 2026-11-20, elevado, na máquina de desenvolvimento: Ryzen 7 5700X3D (família 19h,
modelo 21h), ASUS TUF GAMING X570-PLUS, RTX 5070 Ti (driver 621.01), Windows 11 Pro for
Workstations, RTSS 7.3.7, PawnIO 2.2.0, PawnIO.Modules 0.2.12.

Cada spike é um programa em `examples/` (`cargo run --example <nome>`). Eles não fazem parte do
wintop; servem para provar cada fonte antes dos coletores definitivos.

## 1. Temperatura e potência da CPU (`spike_pawnio`)

| Item | Resultado |
| --- | --- |
| Acesso ao PawnIO | `PawnIOLib.dll` (2.0.0) carregada por caminho absoluto; módulo `AMDFamily17.bin` carregado em 1,3 ms |
| Sem elevação | `\\?\GLOBALROOT\Device\PawnIO`: acesso negado (esperado) |
| Tctl (SMN `0x59800`) | 53,6 a 54,1 °C; Afterburner no mesmo instante: 53,6 a 54,2 °C (diferença ≤ 0,2 °C) |
| CCD1 (SMN `0x59954`) | 43,0 a 45,8 °C (a 5700X3D tem um CCD; os demais vêm inválidos, bit 11 = 0) |
| Potência do pacote (MSR) | 44 a 50 W (unidade 1,526e-5 J por contagem) |
| Custo por ciclo (9 leituras SMN + 1 MSR) | 240 a 305 µs |
| Carregamento no boot | o PawnIO é um dispositivo PnP (`ROOT\PAWNIO\0000`) e carrega no boot; **pendente confirmar após um reboot** |

Pontos para a implementação:

- O módulo aceita qualquer endereço SMN, mas exige o mutex `Global\Access_PCI` em volta de cada
  leitura (é o mesmo que HWiNFO e LibreHardwareMonitor usam).
- O módulo é LGPL-2.1: vai em arquivo separado (`modules/`), nunca embutido no executável.

## 2. Discos (`spike_discos`)

| Disco | Modelo | Barramento | Tipo | Capacidade | Temperatura | Fonte | Custo |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 0 (E:) | Netac SSD 512GB | SATA | SSD | 512,1 GB | 38 °C | SMART 194 | 37 ms |
| 1 | HGST HCC545050A7E380 | SATA | HDD | 500,1 GB | 35 °C | SMART 194 | 2 ms |
| 2 | ST3500418AS | SATA | HDD | 500,1 GB | 37 °C | SMART 194 | 42 ms |
| 3 | WDC WD10SPZX-75Z10T0 | SATA | HDD | 1000,2 GB | 35 °C | SMART 194 | 16 ms |
| 4 | ST3500413AS | SATA | HDD | 500,1 GB | 39 °C | SMART 194 | 58 ms |
| 5 | ST1000DM010-2EP102 | SATA | HDD | 1000,2 GB | 38 °C | IOCTL nativo | 63 ms |
| 6 (D:) | NV900-1T | NVMe | SSD | 1000,2 GB | 45 °C | IOCTL nativo | 5 ms |
| 7 (C:) | NE-512 | NVMe | SSD | 512,1 GB | 47 °C | IOCTL nativo | 0,3 ms |
| 8 (F:) | Storage Space | Spaces | HDD | 3379,1 GB | sem sensor | membros 1 a 5 | — |

Conferido com `Get-StorageReliabilityCounter` (diferença ≤ 1 °C).

Pontos para a implementação:

- `StorageDeviceTemperatureProperty` só funciona nos NVMe e em um HDD; nos SATA devolve
  `ERROR_INVALID_FUNCTION`, e o SMART cobre esses.
- Leituras de 2 a 63 ms: a temperatura dos discos roda na thread lenta (a cada 60 s), nunca na
  thread rápida.
- A capacidade exige um handle com leitura (com acesso 0 o IOCTL falha).
- `GetDevicePowerState` indica repouso; um HDD em repouso não é consultado (estado "em repouso"),
  para não acordá-lo.

## 3. RTSS (`spike_rtss`)

| Item | Resultado |
| --- | --- |
| Shared Memory | `RTSSSharedMemoryV2`, versão 2.21, view de 5.578.752 bytes |
| Layout | offsets gerados pelo compilador MSVC a partir do header oficial; `sizeof(APP_ENTRY)` = 12416 = `dwAppEntrySize` lido ao vivo |
| Assinatura | comparada como `u32` (`0x52545353`); em bytes aparece "SSTR" |
| Campos v2.20/v2.21 | resolução lida (ex.: 1920x1017); tempos de GPU = 0 nos apps atuais (nenhum jogo) |
| 1% e 0,1% low | 0 em todas as entradas fora do modo benchmark: o wintop calcula os seus |
| Apps acompanhados | 6, nenhum jogo (Claude, Spotify, YourPhone, overlay OpenLogi, ZapFast); "idade" de horas desde o último frame: a seleção por atividade recente os descarta |
| Pipe `RTSS_Frametime` | servidor criado com DACL restrita (SYSTEM, Administradores, usuário), rótulo de integridade média, overlapped, sem clientes remotos, instância exclusiva; **nenhuma conexão em 15 s sem jogo aberto** |

Teste com jogo (Deadlock, 2026-11-20):

- A Shared Memory acompanha o jogo: API D3D10 (`0x6`), 2560x1440, 33,5 FPS, idade 47 ms, e os
  campos v2.21 vêm preenchidos (GPU ativa 3237 µs, frame de GPU 38726 µs). Os lows continuam 0.
- O pipe **não recebeu conexão em 60 s**, com o jogo já aberto antes do servidor.
- Quem abre o pipe é o hook injetado no jogo: a string `RTSS_Frametime` existe só em
  `RTSSHooks.dll` e `RTSSHooks64.dll`, não no `RTSS.exe`.
- A DACL não é a causa: um cliente sem elevação (`runas /trustlevel:0x20000`, token de usuário
  básico, integridade média) conectou e escreveu 3 registros, recebidos pelo servidor.
- Hipótese restante: o hook só tenta conectar quando o jogo inicia. **Pendente:** iniciar o spike
  e só então abrir o jogo; confirmar também a unidade de `dwFrametime` e o significado de `dwApp`.

```powershell
cargo run --example spike_rtss 90    # abrir o jogo DEPOIS de "Pipe criado"
```

Consequência para a Etapa 3: o pipe tem de existir antes do jogo abrir (o wintop aberto antes do
jogo). Com o jogo já aberto, o FPS vem da Shared Memory até o próximo início do jogo, e a
interface mostra a fonte usada.

## 4. NVML (`spike_nvml`)

Todos os 29 campos testados são suportados na RTX 5070 Ti (driver 621.01, NVML 13.621.01):
nome, PCI, temperatura e limite de slowdown, utilização (GPU e memória), memória, BAR1, quatro
clocks e o máximo, potência e limite, energia total, 2 fans (% e RPM; 0 em repouso), PCIe
(geração 4, x16, TX/RX), encoder, decoder, P-state, motivos de throttling, processos gráficos e
compute, VBIOS.

- Inicialização: 17 ms (`nvml.dll` por caminho absoluto em System32).
- Leituras comuns: 0 a 1,6 ms. PCIe TX/RX: ~23 ms cada (amostragem interna do NVML) → thread lenta.
- A VRAM por processo continua indisponível no NVML sob WDDM (`[N/A]`): vem do PDH.

## Conclusão

Todas as fontes estão provadas, exceto o pipe de frametime, que depende de um jogo aberto.
Pendências antes da Etapa 3 (RTSS): o teste do pipe com jogo e a confirmação do PawnIO após
um reboot.
