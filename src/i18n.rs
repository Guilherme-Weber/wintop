//! Idioma da interface: inglês americano (padrão) ou português do Brasil (`language` no
//! `config.toml`).
//!
//! O texto em inglês é a chave: o código continua escrevendo a interface em inglês e a tradução
//! acontece na hora de desenhar, nos ajudantes de texto da TUI (`label`, `text`, causas das
//! métricas, títulos). Textos montados com `format!` são traduzidos por partes: separados em
//! " · " e, se ainda não houver tradução, pelo prefixo (`PREFIXES`). Números e nomes passam
//! intactos. Erros do sistema e dos drivers (ex.: "NVML: Unknown Error") ficam como vieram.
//!
//! O idioma é da thread (a TUI desenha numa só): os testes em paralelo não interferem entre si.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    EnUs,
    PtBr,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Self::EnUs => "en-US",
            Self::PtBr => "pt-BR",
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        match s {
            "en-US" => Some(Self::EnUs),
            "pt-BR" => Some(Self::PtBr),
            _ => None,
        }
    }
}

thread_local! {
    static LANG: Cell<Lang> = const { Cell::new(Lang::EnUs) };
}

pub fn set(l: Lang) {
    LANG.with(|c| c.set(l));
}

pub fn get() -> Lang {
    LANG.with(Cell::get)
}

/// Separador decimal do idioma ("1.5" ou "1,5").
pub fn decimal(s: String) -> String {
    if get() == Lang::PtBr {
        s.replace('.', ",")
    } else {
        s
    }
}

/// Tradução de um texto da interface (o próprio texto em inglês, sem tradução).
pub fn tr(s: &str) -> Cow<'_, str> {
    if get() == Lang::EnUs {
        return Cow::Borrowed(s);
    }
    let core = s.trim();
    if core.is_empty() {
        return Cow::Borrowed(s);
    }
    let start = s.len() - s.trim_start().len();
    let (lead, trail) = (&s[..start], &s[start + core.len()..]);
    match tr_core(core) {
        Some(t) => Cow::Owned(format!("{lead}{t}{trail}")),
        None => Cow::Borrowed(s),
    }
}

fn tr_core(core: &str) -> Option<String> {
    let table = table();
    if let Some(t) = table.get(core) {
        return Some((*t).to_string());
    }
    // Partes separadas por " · " (títulos e linhas montadas). Uma chave que também tem " · "
    // ("session min · max (r resets)") vale inteira: a sequência mais longa da tabela primeiro.
    if core.contains(" · ") {
        let parts: Vec<&str> = core.split(" · ").collect();
        let mut out: Vec<String> = Vec::with_capacity(parts.len());
        let mut i = 0;
        while i < parts.len() {
            let whole = (i + 2..=parts.len())
                .rev()
                .find_map(|j| table.get(parts[i..j].join(" · ").as_str()).map(|t| (j, t)));
            if let Some((j, t)) = whole {
                out.push((*t).to_string());
                i = j;
            } else {
                let p = parts[i];
                out.push(tr_core(p.trim()).unwrap_or_else(|| p.to_string()));
                i += 1;
            }
        }
        let joined = out.join(" · ");
        return (joined != core).then_some(joined);
    }
    if let Some(rest) = core
        .strip_prefix("gpu ")
        .and_then(|s| s.strip_suffix(" processes"))
        && rest.contains('/')
    {
        return Some(format!("processos da gpu {rest}"));
    }
    for (en, pt) in PREFIXES {
        // O rótulo pode ser só o prefixo ("limited by "), já sem o espaço do fim.
        if core == en.trim_end() {
            return Some(pt.trim_end().to_string());
        }
        if let Some(rest) = core.strip_prefix(en) {
            let rest = tr_core(rest).unwrap_or_else(|| rest.to_string());
            return Some(format!("{pt}{rest}"));
        }
    }
    for (en, pt) in SUFFIXES {
        if let Some(head) = core.strip_suffix(en) {
            let head = tr_core(head).unwrap_or_else(|| head.to_string());
            return Some(format!("{head}{pt}"));
        }
    }
    None
}

fn table() -> &'static HashMap<&'static str, &'static str> {
    static T: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    T.get_or_init(|| PT_BR.iter().copied().collect())
}

/// Começos de textos montados (o resto é traduzido de novo, se der).
const PREFIXES: &[(&str, &str)] = &[
    ("skipped: ", "pulado: "),
    ("this tab needs ", "esta aba precisa de "),
    ("; the terminal is ", "; o terminal tem "),
    ("min ", "mín "),
    ("max ", "máx "),
    ("gpu processes", "processos da gpu"),
    ("zoom: ", "zoom: "),
    ("collection ", "coleta "),
    ("from row ", "a partir da linha "),
    ("graphs synced to the ", "gráficos no ritmo de "),
    ("limited by ", "limitado pela "),
    ("ended ", "encerrado "),
    ("update failed: ", "falha na atualização: "),
    ("scale ", "escala "),
    ("speed avg ", "clock médio "),
    ("session RAM ", "sessão RAM "),
    ("session VRAM ", "sessão VRAM "),
    ("disconnected: ", "desconectadas: "),
    ("peak ", "pico "),
    ("(max ", "(máx "),
    ("Disk ", "Disco "),
    ("S=sys P=page", "S=sis P=pag"),
];

/// Finais de textos montados ("12 processes").
const SUFFIXES: &[(&str, &str)] = &[
    (" processes", " processos"),
    (" members)", " membros)"),
    (" sources failing", " fontes com falha"),
    (" used", " em uso"),
    (" free", " livre"),
    (" reserved", " reservada"),
    (" commit", " confirmada"),
    (" more (z)", " a mais (z)"),
    (" more (taller terminal)", " a mais (terminal mais alto)"),
    (" more", " a mais"),
    (" ms tick", " ms"),
    (" logical", " lógicos"),
    (" speed", " velocidade"),
    ("[d] fold pool", "[d] recolher pool"),
    ("[d] expand pool", "[d] expandir pool"),
];

/// Português do Brasil. Rótulos curtos alinhados em colunas (3D, encode, VRAM, temp, util,
/// gfx, clk, PCIe...) ficam como estão: são termos técnicos e mudar a largura desalinharia as
/// colunas.
const PT_BR: &[(&str, &str)] = &[
    ("other", "outra"),
    ("other engine", "outra engine"),
    // Abas e caixas.
    ("Overview", "Geral"),
    ("Processes", "Processos"),
    ("Sensors", "Sensores"),
    ("cpu + memory", "cpu + memória"),
    ("disks", "discos"),
    ("game", "jogo"),
    ("memory", "memória"),
    ("processes", "processos"),
    ("sensors", "sensores"),
    ("session", "sessão"),
    ("end process", "encerrar processo"),
    // Atualização.
    ("update", "atualização"),
    ("is available (this is", "está disponível (esta é a"),
    ("). Update now?", "). Atualizar agora?"),
    ("update and restart", "atualizar e reiniciar"),
    ("not now", "agora não"),
    ("Downloading", "Baixando"),
    (
        "Checking the signature, then wintop restarts.",
        "Conferindo a assinatura; depois o wintop reinicia.",
    ),
    ("help", "ajuda"),
    ("net", "rede"),
    ("min", "mín"),
    ("max", "máx"),
    ("· max", "· máx"),
    ("↓ receive  ↑ send", "↓ recebimento  ↑ envio"),
    // Rodapé.
    ("tabs", "abas"),
    ("reset min/max", "zerar mín/máx"),
    ("theme", "tema"),
    ("fold pool", "recolher pool"),
    ("expand pool", "expandir pool"),
    ("next gpu", "próxima gpu"),
    ("pause", "pausar"),
    ("quit", "sair"),
    ("select", "selecionar"),
    ("sort", "ordenar"),
    ("invert", "inverter"),
    ("filter", "filtrar"),
    ("clear filter", "limpar filtro"),
    ("tree", "árvore"),
    ("scroll", "rolar"),
    ("type", "digite"),
    ("to filter", "para filtrar"),
    ("keep", "manter"),
    ("clear", "limpar"),
    ("cancel", "cancelar"),
    ("End", "Encerrar"),
    (
        "? Unsaved work in it is lost.",
        "? O trabalho não salvo nele será perdido.",
    ),
    ("of", "de"),
    ("filter:", "filtro:"),
    ("all OK", "tudo OK"),
    ("sources: all OK", "fontes: tudo OK"),
    ("! failed", "! falhou"),
    ("! 1 source failing", "! 1 fonte com falha"),
    ("[PAUSED · p resumes]", "[PAUSADO · p retoma]"),
    ("[PAUSED]", "[PAUSADO]"),
    ("(z next, esc back)", "(z próxima, esc volta)"),
    ("any key closes", "qualquer tecla fecha"),
    // Ajuda.
    ("p, space", "p, espaço"),
    (
        "switch tabs (Overview, Processes, GPU, Sensors)",
        "trocar de aba (Geral, Processos, GPU, Sensores)",
    ),
    (
        "update interval (100 ms steps, saved to config.toml)",
        "intervalo de atualização (passos de 100 ms, salvo no config.toml)",
    ),
    (
        "pause the screen (collection keeps running)",
        "pausar a tela (a coleta continua)",
    ),
    (
        "Overview: enlarge one box to the whole tab (z next, esc back)",
        "Geral: ampliar uma caixa para a aba inteira (z próxima, esc volta)",
    ),
    (
        "reset session min/max and the game history",
        "zerar mín/máx da sessão e o histórico do jogo",
    ),
    (
        "next GPU, with more than one (Overview, GPU)",
        "próxima GPU, quando houver mais de uma (Geral, GPU)",
    ),
    ("next color theme", "próximo tema de cores"),
    (
        "fold or expand the Storage Space members in the disk box",
        "recolher ou expandir os membros do Storage Space na caixa de discos",
    ),
    (
        "select a row (PgUp/PgDn, Home/End)",
        "selecionar uma linha (PgUp/PgDn, Home/End)",
    ),
    (
        "sort column / invert order (Processes, GPU)",
        "coluna da ordem / inverter (Processos, GPU)",
    ),
    (
        "filter by name or PID / clear the filter",
        "filtrar por nome ou PID / limpar o filtro",
    ),
    (
        "GPU tab: GPU runtimes of the selected process (on demand)",
        "aba GPU: runtimes de GPU do processo selecionado (sob demanda)",
    ),
    (
        "Processes tab: end the selected process (asks to confirm)",
        "aba Processos: encerrar o processo selecionado (pede confirmação)",
    ),
    (
        "Processes tab: tree of parent and child processes",
        "aba Processos: árvore de processos pais e filhos",
    ),
    ("quit (Ctrl+C also works)", "sair (Ctrl+C também funciona)"),
    ("red text", "texto vermelho"),
    (
        "a failed source, with its cause; gray = expected state",
        "uma fonte com falha, com a causa; cinza = estado esperado",
    ),
    (
        "kernel (privileged) share of the CPU time, darker in the graphs",
        "parte do kernel (privilegiada) no tempo de CPU, mais escura nos gráficos",
    ),
    (
        "disk active time / average response time",
        "tempo ativo do disco / tempo médio de resposta",
    ),
    (
        "disk with the system / with a page file",
        "disco com o sistema / com arquivo de paginação",
    ),
    (
        "per frame: ETW or RTSS pipe (exact); approx = shared-memory sampling",
        "por frame: ETW ou pipe do RTSS (exatos); aprox. = memória compartilhada",
    ),
    (
        "frames rendered by the game; the rest are generated (frame gen), darker",
        "frames renderizados pelo jogo; o resto é gerado (frame gen), mais escuro",
    ),
    (
        "frametime; average FPS of the slowest 1% and 0.1% frames",
        "frametime; FPS médio dos 1% e 0,1% frames mais lentos",
    ),
    (
        "AMD control and per-chiplet temperatures (PawnIO)",
        "temperatura de controle e por chiplet da AMD (PawnIO)",
    ),
    // Estados e avisos.
    (
        "waiting for the first collection",
        "aguardando a primeira coleta",
    ),
    (
        "memory: waiting for the first collection",
        "memória: aguardando a primeira coleta",
    ),
    (
        "waiting for the first reading",
        "aguardando a primeira leitura",
    ),
    ("VRAM: waiting", "VRAM: aguardando"),
    ("wait", "aguarde"),
    ("terminal too small", "terminal pequeno demais"),
    (
        "resize the window, or press q to quit",
        "aumente a janela ou tecle q para sair",
    ),
    (
        "no process matches the filter",
        "nenhum processo corresponde ao filtro",
    ),
    (
        "press enter to read the loaded modules (never on RTSS-hooked apps)",
        "tecle enter para ler os módulos carregados (nunca em apps com o hook do RTSS)",
    ),
    ("reading modules...", "lendo os módulos..."),
    (
        "none of the known GPU runtimes",
        "nenhum dos runtimes de GPU conhecidos",
    ),
    (
        "graphs scroll while idle · r resets",
        "gráficos andam no ocioso · r zera",
    ),
    ("lows over 60 s", "lows dos últimos 60 s"),
    ("session min · max (r resets)", "sessão mín · máx (r zera)"),
    ("pipe: exact", "pipe: exato"),
    ("ETW: exact", "ETW: exato"),
    (
        "approx. (game opened before wintop)",
        "aprox. (jogo aberto antes do wintop)",
    ),
    (
        "approx. (frametime pipe failed)",
        "aprox. (pipe de frametime falhou)",
    ),
    ("FAILURE PREDICTED", "FALHA PREVISTA"),
    (
        "the drive predicts a failure, back up its data",
        "o disco prevê uma falha: faça backup dos dados",
    ),
    ("enabled", "ativada"),
    ("disabled", "desativada"),
    ("none", "nenhum"),
    ("idle", "ocioso"),
    ("game · idle", "jogo · ocioso"),
    ("game · RTSS unavailable", "jogo · RTSS indisponível"),
    // Rótulos.
    ("in use", "em uso"),
    ("compressed", "comprimida"),
    ("raw", "real"),
    ("boot peak", "pico boot"),
    ("paging", "paginação"),
    ("hw res", "res. hw"),
    ("hardware reserved", "reservada p/ hardware"),
    ("bad pages", "páginas defeituosas"),
    ("bad RAM pages", "páginas de RAM defeituosas"),
    ("hardware fault", "falha de hardware"),
    ("modified", "modificada"),
    ("free", "livre"),
    ("used", "em uso"),
    ("reserved", "reservada"),
    ("kernel pools", "pools do kernel"),
    ("paged", "paginado"),
    ("non-paged", "não paginado"),
    ("by kind", "por tipo"),
    ("adapter  3D/compute", "adaptador  3D/compute"),
    ("adapter  busiest engine", "adaptador  engine mais usada"),
    ("memory activity", "atividade da memória"),
    ("per core", "por núcleo"),
    ("kernel per core", "kernel por núcleo"),
    ("resolution", "resolução"),
    ("Name", "Nome"),
    ("Value", "Valor"),
    ("Min", "Mín"),
    ("Max", "Máx"),
    ("Temperature", "Temperatura"),
    ("Slowdown limit", "Limite de slowdown"),
    ("Power", "Potência"),
    ("Power limit", "Limite de potência"),
    ("Package power", "Potência do pacote"),
    ("Graphics clock", "Clock gráfico"),
    ("SM clock", "Clock SM"),
    ("Memory clock", "Clock da memória"),
    ("Video clock", "Clock de vídeo"),
    ("Throttle", "Throttling"),
    ("PCIe link", "Link PCIe"),
    ("Disks", "Discos"),
    ("SMART health", "Saúde SMART"),
    ("NVML sensors", "Sensores do NVML"),
    ("Memory", "Memória"),
    ("Shared", "Compart."),
    ("Paged", "Paginada"),
    ("Read", "Leitura"),
    ("Write", "Escrita"),
    ("drive / model", "unidade / modelo"),
    ("drive", "unidade"),
    ("temperature", "temperatura"),
    ("bus", "barramento"),
    ("media type", "tipo de mídia"),
    ("capacity", "capacidade"),
    ("I/O counters", "contadores de E/S"),
    ("(below)", "(abaixo)"),
    ("model", "modelo"),
    // Causas (neutras e falhas) escritas pelo wintop.
    ("no 3D game in focus", "nenhum jogo 3D em foco"),
    (
        "no frames in the last second",
        "nenhum frame no último segundo",
    ),
    ("asleep", "repouso"),
    ("stopped", "parado"),
    ("disconnected", "desconectada"),
    ("protected", "protegido"),
    ("not read", "não lido"),
    ("not read yet", "ainda não lido"),
    (
        "waiting for a second sample",
        "aguardando a segunda amostra",
    ),
    ("waiting for samples", "aguardando amostras"),
    (
        "disk changed: waiting for a new reading",
        "disco trocado: aguardando nova leitura",
    ),
    ("disk removed", "disco removido"),
    ("disk not found", "disco não encontrado"),
    ("see the member disks", "ver os discos membros"),
    ("no member temperature", "sem temperatura dos membros"),
    ("not reported by the drive", "não informado pelo disco"),
    ("not reported by the driver", "não informado pelo driver"),
    ("not reported by firmware", "não informado pelo firmware"),
    ("not supported by Windows", "sem suporte no Windows"),
    ("needs elevation", "requer elevação"),
    ("no page file", "sem arquivo de paginação"),
    ("not supported by NVML", "sem suporte no NVML"),
    ("needs NVML (NVIDIA only)", "requer NVML (só NVIDIA)"),
    (
        "no video encode engine reported",
        "sem engine de codificação de vídeo",
    ),
    (
        "no video decode engine reported",
        "sem engine de decodificação de vídeo",
    ),
    ("no other engine reported", "nenhuma outra engine informada"),
    (
        "driver version not reported by DXGI",
        "versão do driver não informada pelo DXGI",
    ),
    (
        "several identical GPUs: NVML-DXGI matching pending",
        "várias GPUs iguais: associação NVML-DXGI pendente",
    ),
    (
        "NVIDIA GPU not listed by NVML",
        "GPU NVIDIA fora da lista do NVML",
    ),
    (
        "NVML GPU not found in DXGI",
        "GPU do NVML não encontrada no DXGI",
    ),
    (
        "adapter has no PDH instance",
        "adaptador sem instância no PDH",
    ),
    ("no GPU found", "nenhuma GPU encontrada"),
    (
        "no GPU found (NVML and DXGI)",
        "nenhuma GPU encontrada (NVML e DXGI)",
    ),
    ("CPU not supported (not AMD)", "CPU sem suporte (não é AMD)"),
    (
        "not validated on this CPU family",
        "não validado nesta família de CPU",
    ),
    ("no valid CCD sensor", "nenhum sensor de CCD válido"),
    ("RTSS not running", "RTSS fechado"),
    (
        "RTSS not running (started, but no shared memory yet)",
        "RTSS fechado (iniciado, ainda sem memória compartilhada)",
    ),
    ("starting RTSS", "iniciando o RTSS"),
    ("not started in --check", "não iniciado no --check"),
    (
        "resolution not reported by RTSS",
        "resolução não informada pelo RTSS",
    ),
    ("not available in this game", "indisponível neste jogo"),
    ("no frametime", "sem frametime"),
    ("link speed unknown", "velocidade do link desconhecida"),
    ("interface disappeared", "a interface sumiu"),
    (
        "counter went backwards (interface reset)",
        "contador voltou (interface reiniciada)",
    ),
    (
        "counters went backwards (disk reset)",
        "contadores voltaram (disco reiniciado)",
    ),
    (
        "no physical network interface",
        "nenhuma interface de rede física",
    ),
    (
        "no physical disk could be opened",
        "nenhum disco físico pôde ser aberto",
    ),
    (
        "no read access (not elevated?)",
        "sem acesso de leitura (não elevado?)",
    ),
    (
        "no read/write access (not elevated?)",
        "sem acesso de leitura/escrita (não elevado?)",
    ),
    (
        "several pools: association pending",
        "vários pools: associação pendente",
    ),
    (
        "process changed: select it again",
        "o processo mudou: selecione de novo",
    ),
    ("module reader panicked", "o leitor de módulos falhou"),
    (
        "skipped: 3D app hooked by RTSS (anticheat safety)",
        "pulado: app 3D com o hook do RTSS (segurança do anticheat)",
    ),
    (
        "skipped: started less than 5 s ago (anticheat safety)",
        "pulado: iniciado há menos de 5 s (segurança do anticheat)",
    ),
    (
        "skipped: waiting for RTSS to list the hooked apps (anticheat safety)",
        "pulado: aguardando o RTSS listar os apps com hook (segurança do anticheat)",
    ),
    (
        "needs RTSS shared memory 2.20+",
        "requer memória compartilhada do RTSS 2.20+",
    ),
    (
        "needs RTSS shared memory 2.21+",
        "requer memória compartilhada do RTSS 2.21+",
    ),
    ("stopping", "parando"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingles_e_a_chave() {
        set(Lang::EnUs);
        assert_eq!(tr(" fold pool  "), " fold pool  ");
        assert_eq!(decimal("1.5".into()), "1.5");
    }

    #[test]
    fn portugues_mantem_espacos_e_partes() {
        set(Lang::PtBr);
        assert_eq!(tr(" fold pool  "), " recolher pool  ");
        assert_eq!(tr("game · idle"), "jogo · ocioso");
        assert_eq!(tr("session · asleep"), "sessão · repouso");
        assert_eq!(tr("disconnected: Ethernet"), "desconectadas: Ethernet");
        assert_eq!(tr("limited by "), "limitado pela ");
        assert_eq!(tr("total 80s · 16 logical"), "total 80s · 16 lógicos");
        assert_eq!(tr("12 processes"), "12 processos");
        assert_eq!(tr("deadlock.exe"), "deadlock.exe");
        assert_eq!(tr("NVML: Unknown Error"), "NVML: Unknown Error");
        assert_eq!(tr("   "), "   ");
        assert_eq!(decimal("1.5 GB".into()), "1,5 GB");
        // Chave com " · " dentro de um título montado vale inteira.
        assert_eq!(
            tr("from row 5 · session min · max (r resets)"),
            "a partir da linha 5 · sessão mín · máx (r zera)"
        );
        assert_eq!(tr("min 0% · max 99%"), "mín 0% · máx 99%");
        assert_eq!(tr("! 3 sources failing "), "! 3 fontes com falha ");
        assert_eq!(tr("F: Pool (5 members)"), "F: Pool (5 membros)");
        assert_eq!(tr("gpu 1/2 processes"), "processos da gpu 1/2");
        assert_eq!(
            tr("S=sys P=page  [d] fold pool"),
            "S=sis P=pag  [d] recolher pool"
        );
        assert_eq!(tr("p, space"), "p, espaço");
        assert_eq!(tr("adapter  3D/compute "), "adaptador  3D/compute ");
        assert_eq!(tr("memory activity "), "atividade da memória ");
        set(Lang::EnUs);
    }

    /// Entrada que não muda nada só custaria uma alocação por quadro.
    #[test]
    fn sem_entradas_identicas() {
        for (en, pt) in PT_BR.iter().chain(SUFFIXES) {
            assert_ne!(en, pt);
        }
        // Prefixos idênticos só para traduzir o resto ("zoom: disks").
        for (en, pt) in PREFIXES {
            assert!(en != pt || *en == "zoom: ", "{en}");
        }
    }

    /// Toda chave aparece uma vez (uma repetida esconderia a outra tradução).
    #[test]
    fn chaves_unicas() {
        let mut keys: Vec<&str> = PT_BR.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n);
    }

    #[test]
    fn codigos() {
        assert_eq!(Lang::from_code("pt-BR"), Some(Lang::PtBr));
        assert_eq!(Lang::from_code("en-US").map(Lang::code), Some("en-US"));
        assert_eq!(Lang::from_code("pt"), None);
    }
}
