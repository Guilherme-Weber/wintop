# Módulos do PawnIO

`AMDFamily17.bin` é o módulo oficial e assinado do projeto PawnIO.Modules, sem modificações:

- Origem: https://github.com/namazso/PawnIO.Modules (release 0.2.12, `release_0_2_12.zip`)
- Código-fonte: `AMDFamily17.p` no mesmo repositório
- Licença: GNU Lesser General Public License 2.1 (texto em `COPYING-LGPL-2.1`)
- SHA-256: `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91`

O módulo é distribuído como arquivo separado, ao lado do executável, e pode ser substituído por
outra versão compatível. O wintop o usa para ler a temperatura (SMN) e a energia (MSR) de
processadores AMD das famílias 17h a 1Ah. O driver PawnIO é instalado à parte
(https://pawnio.eu, `winget install namazso.PawnIO`).
