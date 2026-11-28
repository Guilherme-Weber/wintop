# Módulos do PawnIO

`AMDFamily17.bin` e `IntelMSR.bin` são os módulos oficiais e assinados do projeto PawnIO.Modules,
sem modificações:

- Origem: https://github.com/namazso/PawnIO.Modules, release 0.2.12 (`release_0_2_12.zip`), tag
  `0.2.12` (commit `34be4e1f320b8ae69be6f3172bb62f2ace3f36d3`)
- Autor: namazso (Copyright (C) 2025 namazso)
- Licença: GNU Lesser General Public License 2.1 ou posterior (texto em `COPYING-LGPL-2.1`)
- SHA-256 de `AMDFamily17.bin`: `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91`
- SHA-256 de `IntelMSR.bin`: `30a5a302283de1e080dfee99e686eca91d7e53e911095cb91a320fb961f1ce12`

## Código-fonte correspondente

Conforme a seção 4 da LGPL-2.1, o código-fonte dos módulos é distribuído junto, na pasta
`source/`, copiado sem alterações da tag `0.2.12`:

- `source/AMDFamily17.p` e `source/IntelMSR.p`: os módulos;
- `source/include/`: os arquivos incluídos por eles (`pawnio.inc` e os que este inclui).

Os binários são produzidos pelo compilador Pawn do projeto PawnIO.Modules a partir desses
arquivos.

## Uso

Os módulos são distribuídos como arquivos separados, ao lado do executável, e podem ser
substituídos por outra versão compatível. O wintop os usa para ler a temperatura e a energia do
processador:

- `AMDFamily17.bin`: processadores AMD (famílias 17h a 1Ah), temperatura pelo SMN e energia pelo
  MSR;
- `IntelMSR.bin`: processadores Intel, temperatura do pacote e de cada núcleo e energia (RAPL),
  só por leitura de MSR (o wintop nunca chama a escrita do módulo).

O driver PawnIO é instalado à parte (https://pawnio.eu, `winget install namazso.PawnIO`).
