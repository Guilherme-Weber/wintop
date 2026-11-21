# Módulos do PawnIO

`AMDFamily17.bin` é o módulo oficial e assinado do projeto PawnIO.Modules, sem modificações:

- Origem: https://github.com/namazso/PawnIO.Modules, release 0.2.12 (`release_0_2_12.zip`), tag
  `0.2.12` (commit `34be4e1f320b8ae69be6f3172bb62f2ace3f36d3`)
- Autor: namazso (Copyright (C) 2025 namazso)
- Licença: GNU Lesser General Public License 2.1 ou posterior (texto em `COPYING-LGPL-2.1`)
- SHA-256 do binário: `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91`

## Código-fonte correspondente

Conforme a seção 4 da LGPL-2.1, o código-fonte do módulo é distribuído junto, na pasta
`source/`, copiado sem alterações da tag `0.2.12`:

- `source/AMDFamily17.p`: o módulo;
- `source/include/`: os arquivos incluídos por ele (`pawnio.inc` e os que este inclui).

O binário é produzido pelo compilador Pawn do projeto PawnIO.Modules a partir desses arquivos.

## Uso

O módulo é distribuído como arquivo separado, ao lado do executável, e pode ser substituído por
outra versão compatível. O wintop o usa para ler a temperatura (SMN) e a energia (MSR) de
processadores AMD. O driver PawnIO é instalado à parte (https://pawnio.eu,
`winget install namazso.PawnIO`).
