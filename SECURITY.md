# Política de segurança

O wintop roda como administrador e, com o atalho instalado (`wintop --install`), pode ser
aberto elevado por qualquer programa do usuário sem a janela do UAC. Falhas que permitam a um
processo sem elevação obter efeito como administrador por meio do wintop (gravar ou ler onde não
poderia, carregar código, alterar a instalação) são tratadas como prioridade.

## Versões suportadas

Somente a versão mais recente publicada em
[releases](https://github.com/Guilherme-Weber/wintop/releases) recebe correções.

## Como reportar

Não abra uma issue pública. Use o reporte privado do GitHub: aba **Security** do repositório,
botão **Report a vulnerability**. Inclua a versão (`wintop --version`), a versão do Windows, os
passos para reproduzir e o impacto observado.

O recebimento é confirmado em até 7 dias. A correção é publicada numa nova versão, com crédito a
quem reportou, se desejado, depois que a correção estiver disponível.

## Escopo

- O executável `wintop.exe`, o instalador (`--install`/`--uninstall`) e os scripts de
  `scripts/`.
- Fora do escopo: vulnerabilidades do PawnIO, do RTSS, do driver da NVIDIA ou do Windows (reporte
  aos respectivos projetos), e ações que já exigem ser administrador.
