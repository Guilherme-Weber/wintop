# Changelog

Todas as mudanças relevantes do projeto. Formato baseado em
[Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/).

## [Não lançado]

### Adicionado

- Etapa 0: base do projeto (toolchain fixa, lints, CI, manifesto `requireAdministrator`,
  CRT estático), inventário de métricas e provas de viabilidade de cada fonte de dados
  (PawnIO, discos, RTSS, NVML) em `examples/`.
- Instância única por sessão de usuário.
- Código-fonte do módulo `AMDFamily17` (LGPL-2.1) em `modules/source/`.
- Membros de Storage Spaces identificados pela partição GPT de proteção, sem WMI.
- Etapa 1: coletores nativos (CPU, memória, discos por `IOCTL_DISK_PERFORMANCE`, rede, GPU pelo
  NVML) e o diagnóstico `--check`.
- Etapa 2: temperatura e potência da CPU pelo PawnIO; temperatura dos discos (nativa e SMART,
  sem acordar HDD em repouso); nome amigável dos discos.
- Etapa 3: FPS, frametime e lows pelo RTSS, com pipe de frametime exato e modo aproximado pela
  Shared Memory.
- Etapa 4: motor com três threads, consumo do próprio wintop, intervalo ajustável com `+`/`-` e
  parada limpa (Ctrl+C, Ctrl+Break, fechar a janela).
- Etapa 5: aba geral da interface (ratatui): gráficos de uso com tempo de kernel, grade por
  núcleo, VRAM por categoria, RAM por composição, rede espelhada, discos, caixa do jogo,
  mínimos e máximos da sessão (`r`), linhas que quebram em vez de cortar.

### Corrigido (revisão Gemini Neon + Argon)

- Manifesto de administrador só no executável principal (testes e exemplos rodam sem elevação).
- Provas: HDD em repouso não é consultado; pipe do RTSS com várias instâncias e tratamento
  correto de I/O overlapped; leituras de Shared Memory voláteis e com limites checados; idade do
  aplicativo sem underflow; handles com dono (RAII); `nvml.dll` por `GetSystemDirectoryW` e busca
  de DLLs restrita a System32; capacidades em unidade binária.
- `.gitattributes`, permissões mínimas e build de release no CI.
