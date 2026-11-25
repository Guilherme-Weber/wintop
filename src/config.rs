//! Configuração em `%APPDATA%\wintop\config.toml` (Etapa 7).
//!
//! - Criada com os valores padrão (e comentários) na 1ª execução.
//! - Gravada quando algo muda pela interface (intervalo `+`/`-`, tema `t`, pool `d`), editando
//!   o documento original: comentários e a formatação do usuário ficam. Gravação atômica
//!   (arquivo temporário + troca).
//! - O wintop roda elevado e o arquivo é do usuário: só dados (números, nomes, cores), nada que
//!   vire caminho executado; tamanho limitado; valor inválido = padrão, com aviso.
//! - Arquivo com erro de sintaxe: o wintop abre com os padrões, avisa e **não grava** por cima
//!   (a edição do usuário não se perde).

use std::path::PathBuf;

use toml_edit::{Array, DocumentMut, Item, Value, value};

use crate::engine::{DEFAULT_INTERVAL_MS, clamp_interval};

/// Teto do arquivo (um config real tem poucas centenas de bytes).
const MAX_BYTES: u64 = 64 << 10;

/// Apps que o RTSS acompanha mas não são jogos (overlays, navegadores, apps Electron).
pub const DEFAULT_RTSS_IGNORED: &[&str] = &[
    "claude.exe",
    "spotify.exe",
    "spotifast.exe",
    "zapfast.exe",
    "discord.exe",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "msedgewebview2.exe",
    "steamwebhelper.exe",
    "gameoverlayui64.exe",
    "openlogi-overlay.exe",
    "yourphoneappproxyhost.exe",
];

/// Recolhimento dos membros do Storage Space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FoldPool {
    /// Recolhe só se não couberem (mais de 12 discos).
    #[default]
    Auto,
    Fold,
    Expand,
}

impl FoldPool {
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fold => "fold",
            Self::Expand => "expand",
        }
    }

    pub fn as_option(self) -> Option<bool> {
        match self {
            Self::Auto => None,
            Self::Fold => Some(true),
            Self::Expand => Some(false),
        }
    }

    pub fn from_option(v: Option<bool>) -> Self {
        match v {
            None => Self::Auto,
            Some(true) => Self::Fold,
            Some(false) => Self::Expand,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub interval_ms: u64,
    pub theme: String,
    pub fold_pool: FoldPool,
    /// Executáveis (minúsculos) que nunca viram "o jogo".
    pub rtss_ignored: Vec<String>,
    /// Cores trocadas sobre o tema: (nome da cor, "#rrggbb").
    pub colors: Vec<(String, String)>,
    /// Frametime (ms) a partir do qual o gráfico e o valor ficam em destaque.
    pub frametime_alert_ms: f64,
    /// Unidade da rede: bits por segundo (como o Task Manager) ou bytes.
    pub net_bytes: bool,
    /// Mouse na interface (clique nas abas, nas linhas e no cabeçalho; rodinha nas listas).
    pub mouse: bool,
    /// Maximizar a janela do console clássico ao abrir (o atalho abre o conhost com 120x30).
    pub start_maximized: bool,
    /// Idioma da interface (en-US padrão; pt-BR).
    pub language: crate::i18n::Lang,
    /// Procurar versão nova no GitHub ao abrir (e oferecer a atualização).
    pub check_updates: bool,
    /// Modo debug: registra todos os erros (inclusive transitórios) em `wintop-debug.log`.
    pub debug: bool,
}

/// Faixa aceita do limite de alerta do frametime (ms).
pub const FRAMETIME_ALERT_RANGE: std::ops::RangeInclusive<f64> = 4.0..=1000.0;
pub const DEFAULT_FRAMETIME_ALERT_MS: f64 = 33.3;

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_INTERVAL_MS,
            theme: "default".into(),
            fold_pool: FoldPool::Auto,
            rtss_ignored: DEFAULT_RTSS_IGNORED
                .iter()
                .map(ToString::to_string)
                .collect(),
            colors: Vec::new(),
            frametime_alert_ms: DEFAULT_FRAMETIME_ALERT_MS,
            net_bytes: false,
            mouse: true,
            start_maximized: true,
            language: crate::i18n::Lang::EnUs,
            check_updates: true,
            debug: false,
        }
    }
}

/// Texto do arquivo criado na 1ª execução.
fn template(c: &Config) -> String {
    let ignored: Vec<String> = c
        .rtss_ignored
        .iter()
        .map(|n| format!("    \"{n}\",\n"))
        .collect();
    format!(
        "# wintop configuration. Edits here apply within a few seconds, even with wintop open;\n\
         # changes made in the UI (keys) are saved here, keeping the comments.\n\
         \n\
         # Refresh interval in milliseconds (100 to 10000, steps of 100). Keys: + and -.\n\
         interval_ms = {}\n\
         \n\
         # Color theme: default, nord, gruvbox, dracula, mono. Key: t.\n\
         theme = \"{}\"\n\
         \n\
         # Storage Space members in the disk box: auto, fold, expand. Key: d.\n\
         fold_pool = \"{}\"\n\
         \n\
         # Frametime (ms) from which the game box highlights it (33.3 = below 30 FPS).\n\
         frametime_alert_ms = {}\n\
         \n\
         # Network rates in \"bits\" per second (like Task Manager) or \"bytes\".\n\
         net_unit = \"{}\"\n\
         \n\
         # Mouse: click tabs, rows and column headers, wheel scrolls the lists. With it on,\n\
         # select text in the terminal with Shift+drag.\n\
         mouse = {}\n\
         \n\
         # Maximize the classic console window on start (the no-UAC shortcut opens it at\n\
         # 120x30). The Windows Terminal is not touched.\n\
         start_maximized = {}\n\
         \n\
         # Interface language: \"en-US\" (default) or \"pt-BR\" (Brazilian Portuguese).\n\
         language = \"{}\"\n\
         \n\
         # Look for a new version on GitHub when wintop opens and offer to install it (the\n\
         # download is checked against the wintop signing key before anything is replaced).\n\
         check_updates = {}\n\
         \n\
         # Debug mode: log all errors (including transient/tolerated ones) to wintop-debug.log\n\
         # next to this file, rotating at 2 MB.\n\
         debug = {}\n\
         \n\
         [rtss]\n\
         # Apps hooked by RTSS that are never treated as the game (lowercase executable names).\n\
         ignored = [\n{}]\n\
         \n\
         [colors]\n\
         # Override single theme colors with \"#rrggbb\", e.g. border = \"#4b5263\".\n\
         # UI: border, title, text, label, accent, ok, warn, neutral, fail, selected, dim.\n\
         # Graphs & bars: net_send, net_recv, fps, frametime, frametime_spike, fps_over,\n\
         #   fps_low, encode, decode, ram_in_use, ram_compressed, ram_modified, ram_standby, game,\n\
         #   vram_app, vram_compositor, vram_browser, vram_system, vram_kernel, low, mid, high.\n",
        c.interval_ms,
        c.theme,
        c.fold_pool.name(),
        c.frametime_alert_ms,
        if c.net_bytes { "bytes" } else { "bits" },
        c.mouse,
        c.start_maximized,
        c.language.code(),
        c.check_updates,
        c.debug,
        ignored.concat()
    )
}

/// Pasta e arquivo dentro de `AppData\Roaming`.
const DIR: &str = "wintop";
const FILE: &str = "config.toml";

/// Onde mora o `config.toml`: subpasta em `AppData\Roaming` ou a própria pasta do exe (portátil).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfigDir {
    /// `<base>\<dir>` (ex.: `AppData\Roaming\wintop`, ou pasta de teste dentro de `Roaming`).
    Roaming { base: PathBuf, dir: String },
    /// Pasta do próprio exe no modo portátil (inclusive raiz de unidade, ex.: `D:\`).
    Portable(PathBuf),
}

impl ConfigDir {
    fn file_path(&self) -> PathBuf {
        match self {
            Self::Roaming { base, dir } => base.join(dir).join(FILE),
            Self::Portable(d) => d.join(FILE),
        }
    }

    fn open(&self, create: bool) -> Result<Option<crate::userfile::UserDir>, String> {
        match self {
            Self::Portable(d) => crate::userfile::UserDir::open_exe_dir(d).map(Some),
            Self::Roaming { base, dir } => crate::userfile::UserDir::open(base, dir, create),
        }
    }
}

/// Configuração carregada: os valores, o documento (para regravar mantendo comentários) e
/// os avisos (valores inválidos, arquivo com erro).
pub struct Loaded {
    pub config: Config,
    doc: Option<DocumentMut>,
    /// Localização do config (`AppData\Roaming\wintop` ou pasta portátil).
    loc: Option<ConfigDir>,
    /// Problemas encontrados ao ler (a interface mostra o primeiro).
    pub warnings: Vec<String>,
    /// Gravação liberada (não com o arquivo do usuário em erro de sintaxe ou ilegível).
    writable: bool,
}

/// `AppData\Roaming` do usuário (Known Folder, não a variável de ambiente).
pub fn roaming() -> Option<PathBuf> {
    crate::win::known_folder(&windows::Win32::UI::Shell::FOLDERID_RoamingAppData).ok()
}

/// Pasta do exe no modo portátil (o config fica nela); definida uma vez, na partida.
static PORTABLE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Modo portátil: o `config.toml` passa a ser `<dir>\config.toml` (`dir` = pasta do exe).
pub fn set_portable(dir: PathBuf) {
    let _ = PORTABLE.set(dir);
}

/// Local do config: pasta do exe no modo portátil, senão `%APPDATA%\wintop`.
fn home() -> Option<ConfigDir> {
    if let Some(d) = PORTABLE.get() {
        return Some(ConfigDir::Portable(d.clone()));
    }
    Some(ConfigDir::Roaming {
        base: roaming()?,
        dir: DIR.to_string(),
    })
}

/// Abre a pasta do config (`%APPDATA%\wintop` ou a pasta do exe no modo portátil) pelo
/// `userfile`. Usada também pelo log de debug.
pub fn open_home_dir(create: bool) -> Result<Option<crate::userfile::UserDir>, String> {
    let Some(loc) = home() else {
        return Err("no AppData folder".into());
    };
    loc.open(create)
}

/// Caminho do arquivo, para exibir: `%APPDATA%\wintop\config.toml` (ou o portátil).
pub fn default_path() -> Option<PathBuf> {
    home().map(|loc| loc.file_path())
}

/// Lê o config do lugar em uso (portátil ou `%APPDATA%`); `create` como em `load`.
pub fn load_home(create: bool) -> Loaded {
    load_at(home(), create)
}

/// Lê o arquivo em `<base>\wintop\config.toml`; `create`: cria com os padrões se faltar (a
/// interface; o `--check` só lê). Todo acesso passa por `userfile` (o wintop roda elevado e a
/// pasta é do usuário: links e redirecionamentos são recusados).
#[cfg(test)]
fn load_in(base: Option<PathBuf>, dir_name: &str, create: bool) -> Loaded {
    load_at(
        base.map(|b| ConfigDir::Roaming {
            base: b,
            dir: dir_name.to_string(),
        }),
        create,
    )
}

fn load_at(loc: Option<ConfigDir>, create: bool) -> Loaded {
    let mut l = Loaded {
        config: Config::default(),
        doc: None,
        loc: loc.clone(),
        warnings: Vec::new(),
        writable: loc.is_some(),
    };
    let Some(loc) = loc else {
        l.warnings
            .push("config: no AppData folder (defaults, not saved)".into());
        return l;
    };
    let dir = match loc.open(create) {
        Ok(Some(d)) => d,
        // Sem a pasta no modo só leitura (a base passou nas checagens): padrões, sem aviso.
        Ok(None) => return l,
        Err(e) => {
            l.warnings
                .push(format!("config: {e} (defaults, not saved)"));
            l.writable = false;
            return l;
        }
    };
    let text = match dir.read(FILE, MAX_BYTES) {
        Ok(Some(t)) => t,
        Ok(None) => {
            // 1ª execução: cria com os padrões (comentados).
            let text = template(&l.config);
            if create && let Err(e) = dir.write(FILE, &text) {
                l.warnings.push(format!("config: could not create it: {e}"));
                l.writable = false;
            }
            l.doc = text.parse().ok();
            return l;
        }
        Err(e) => {
            l.warnings
                .push(format!("config: {e} (defaults, not saved)"));
            l.writable = false;
            return l;
        }
    };
    match text.parse::<DocumentMut>() {
        Ok(doc) => {
            l.config = from_doc(&doc, &mut l.warnings);
            l.doc = Some(doc);
        }
        Err(e) => {
            let first = e.to_string().lines().next().unwrap_or_default().to_string();
            l.warnings.push(format!(
                "config: syntax error ({}); defaults, not saved",
                crate::win::printable(&first)
            ));
            l.writable = false;
        }
    }
    l
}

/// Valores do documento; o que for inválido fica no padrão, com aviso.
fn from_doc(doc: &DocumentMut, warn: &mut Vec<String>) -> Config {
    let mut c = Config::default();
    if let Some(v) = doc.get("interval_ms") {
        match v.as_integer() {
            Some(n) if n > 0 => c.interval_ms = clamp_interval(n as u64),
            _ => warn.push("config: interval_ms must be a positive number".into()),
        }
    }
    if let Some(v) = doc.get("theme") {
        match v.as_str() {
            Some(t) if crate::ui::theme::exists(t) => c.theme = t.to_string(),
            Some(t) => warn.push(format!(
                "config: unknown theme \"{}\"",
                crate::win::printable(t)
            )),
            None => warn.push("config: theme must be text".into()),
        }
    }
    if let Some(v) = doc.get("fold_pool") {
        c.fold_pool = match v.as_str() {
            Some("auto") => FoldPool::Auto,
            Some("fold") => FoldPool::Fold,
            Some("expand") => FoldPool::Expand,
            _ => {
                warn.push("config: fold_pool must be auto, fold or expand".into());
                FoldPool::Auto
            }
        };
    }
    if let Some(v) = doc.get("frametime_alert_ms") {
        let n = v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
        match n {
            Some(ms) if FRAMETIME_ALERT_RANGE.contains(&ms) => c.frametime_alert_ms = ms,
            _ => warn.push("config: frametime_alert_ms must be a number from 4 to 1000".into()),
        }
    }
    if let Some(v) = doc.get("net_unit") {
        match v.as_str() {
            Some("bits") => c.net_bytes = false,
            Some("bytes") => c.net_bytes = true,
            _ => warn.push("config: net_unit must be bits or bytes".into()),
        }
    }
    if let Some(v) = doc.get("language") {
        match v.as_str().and_then(crate::i18n::Lang::from_code) {
            Some(l) => c.language = l,
            None => warn.push("config: language must be en-US or pt-BR".into()),
        }
    }
    if let Some(v) = doc.get("start_maximized") {
        match v.as_bool() {
            Some(b) => c.start_maximized = b,
            None => warn.push("config: start_maximized must be true or false".into()),
        }
    }
    if let Some(v) = doc.get("check_updates") {
        match v.as_bool() {
            Some(b) => c.check_updates = b,
            None => warn.push("config: check_updates must be true or false".into()),
        }
    }
    if let Some(v) = doc.get("debug") {
        match v.as_bool() {
            Some(b) => c.debug = b,
            None => warn.push("config: debug must be true or false".into()),
        }
    }
    if let Some(v) = doc.get("mouse") {
        match v.as_bool() {
            Some(b) => c.mouse = b,
            None => warn.push("config: mouse must be true or false".into()),
        }
    }
    if doc.get("rtss").is_some_and(|t| t.as_table_like().is_none()) {
        warn.push("config: rtss must be a table ([rtss])".into());
    }
    if let Some(list) = doc.get("rtss").and_then(|t| t.get("ignored")) {
        match list.as_array() {
            Some(a) => {
                c.rtss_ignored = a
                    .iter()
                    .filter_map(Value::as_str)
                    .take(256)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty() && s.len() <= 260)
                    .collect();
            }
            None => warn.push("config: rtss.ignored must be a list of names".into()),
        }
    }
    if doc
        .get("colors")
        .is_some_and(|t| t.as_table_like().is_none())
    {
        warn.push("config: colors must be a table ([colors])".into());
    }
    if let Some(t) = doc.get("colors").and_then(Item::as_table_like) {
        for (k, v) in t.iter().take(64) {
            match v.as_str() {
                Some(hex) if crate::ui::theme::parse_hex(hex).is_some() => {
                    if crate::ui::theme::is_color_name(k) {
                        c.colors.push((k.to_string(), hex.to_string()));
                    } else {
                        warn.push(format!(
                            "config: unknown color \"{}\"",
                            crate::win::printable(k)
                        ));
                    }
                }
                _ => warn.push(format!(
                    "config: colors.{} must be \"#rrggbb\"",
                    crate::win::printable(k)
                )),
            }
        }
    }
    c
}

/// Valores que a interface muda, no documento (o resto fica como o usuário deixou).
fn update_doc(doc: &mut DocumentMut, c: &Config) {
    doc["interval_ms"] = value(c.interval_ms as i64);
    doc["theme"] = value(c.theme.as_str());
    doc["fold_pool"] = value(c.fold_pool.name());
    // A lista do RTSS só entra se a tabela não existe (um `rtss` que não é tabela fica como
    // o usuário deixou: indexar nele derrubaria o programa).
    if doc.get("rtss").is_none() {
        let mut a = Array::new();
        for n in &c.rtss_ignored {
            a.push(n.as_str());
        }
        let mut t = toml_edit::Table::new();
        t.insert("ignored", value(a));
        doc.insert("rtss", Item::Table(t));
    }
}

/// Gravação de `config.toml` já serializada em memória, pronta para ir para o disco em outra
/// thread sem travar a interface.
pub struct SaveJob {
    loc: ConfigDir,
    text: String,
}

impl SaveJob {
    pub fn write(&self) -> Result<(), String> {
        self.loc
            .open(true)?
            .ok_or("config folder not created")?
            .write(FILE, &self.text)
    }
}

impl Loaded {
    /// Atualiza o documento em memória e devolve a gravação pronta para o disco.
    pub fn prepare_save(&mut self, c: &Config) -> Result<SaveJob, String> {
        if !self.writable {
            return Err("config not saved (see the warning)".into());
        }
        let loc = self.loc.clone().ok_or("no AppData folder")?;
        let doc = self
            .doc
            .get_or_insert_with(|| template(c).parse().unwrap_or_default());
        update_doc(doc, c);
        let text = doc.to_string();
        self.config = c.clone();
        Ok(SaveJob { loc, text })
    }

    /// Grava os valores que a interface muda (intervalo, tema, pool), mantendo o resto do
    /// documento. Devolve o erro para a interface avisar.
    #[cfg(test)]
    pub fn save(&mut self, c: &Config) -> Result<(), String> {
        let prev = self.config.clone();
        let job = self.prepare_save(c)?;
        if let Err(e) = job.write() {
            self.config = prev;
            return Err(e);
        }
        Ok(())
    }

    /// Gravação liberada (o aviso de leitura explica por que não).
    pub fn writable(&self) -> bool {
        self.writable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pasta de teste dentro da Roaming real, removida automaticamente no `Drop`.
    struct TestDir {
        base: PathBuf,
        dir: String,
        file: PathBuf,
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.base.join(&self.dir));
        }
    }

    /// Pasta de teste dentro da Roaming real (do usuário, como a de verdade), sem encostar no
    /// `wintop\config.toml`.
    fn temp(name: &str) -> TestDir {
        let base = roaming().expect("Roaming");
        let dir = format!("wintop_test_{}_{name}", std::process::id());
        let _ = std::fs::remove_dir_all(base.join(&dir));
        let file = base.join(&dir).join(FILE);
        TestDir { base, dir, file }
    }

    /// Arquivo qualquer: a leitura dá valores válidos (ou os padrões), e a regravação do que a
    /// interface muda continua um TOML válido com esses valores.
    #[test]
    fn fuzz_arquivo_de_configuracao() {
        let full = "interval_ms = 500\ntheme = \"nord\"\nfold_pool = \"fold\"\n\
                    [rtss]\nignored = [\"a.exe\", \"B.EXE \"]\n[colors]\nborder = \"#112233\"\n";
        let odd =
            "rtss = 3\ncolors = { text = \"#zzzzzz\", nope = \"#000000\" }\ninterval_ms = -1\n";
        let tpl = template(&Config::default());
        crate::fuzz::run_text("config", &[full, odd, &tpl], 4096, |text| {
            let Ok(mut doc) = text.parse::<DocumentMut>() else {
                return;
            };
            let mut warn = Vec::new();
            let c = from_doc(&doc, &mut warn);
            use crate::engine::{MAX_INTERVAL_MS, MIN_INTERVAL_MS, STEP_MS};
            assert!((MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&c.interval_ms));
            assert_eq!(c.interval_ms % STEP_MS, 0);
            assert!(crate::ui::theme::exists(&c.theme));
            assert!(c.rtss_ignored.len() <= 256);
            assert!(c.rtss_ignored.iter().all(|n| {
                !n.is_empty() && n.len() <= 260 && *n == n.trim().to_ascii_lowercase()
            }));
            assert!(c.colors.len() <= 64);
            assert!(c.colors.iter().all(|(k, v)| {
                crate::ui::theme::is_color_name(k) && crate::ui::theme::parse_hex(v).is_some()
            }));
            assert!(warn.iter().all(|w| !w.chars().any(char::is_control)));
            update_doc(&mut doc, &c);
            let again: DocumentMut = doc.to_string().parse().expect("regravado inválido");
            let back = from_doc(&again, &mut Vec::new());
            assert_eq!(
                (back.interval_ms, back.theme, back.fold_pool.name()),
                (c.interval_ms, c.theme, c.fold_pool.name())
            );
        });
    }

    #[test]
    fn primeira_execucao_cria_com_os_padroes() {
        let t = temp("novo");
        // Só leitura (--check): não cria nada.
        let l = load_in(Some(t.base.clone()), &t.dir, false);
        assert!(
            l.warnings.is_empty() && !t.file.exists(),
            "{:?}",
            l.warnings
        );
        let l = load_in(Some(t.base.clone()), &t.dir, true);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.config, Config::default());
        let text = std::fs::read_to_string(&t.file).expect("arquivo criado");
        assert!(
            text.contains("interval_ms = 1000") && text.contains("\"discord.exe\""),
            "{text}"
        );
        // O arquivo criado relido dá os mesmos valores.
        assert_eq!(
            load_in(Some(t.base.clone()), &t.dir, true).config,
            Config::default()
        );
    }

    #[test]
    fn gravar_preserva_comentarios_e_o_resto() {
        let t = temp("grava");
        std::fs::create_dir_all(t.file.parent().expect("pasta")).expect("pasta");
        std::fs::write(
            &t.file,
            "# meu comentário\ninterval_ms = 500\ntheme = \"nord\"\n\n[rtss]\nignored = [\"obs64.exe\"]\n\n[colors]\nborder = \"#112233\"\n",
        )
        .expect("escrita");
        let mut l = load_in(Some(t.base.clone()), &t.dir, true);
        assert_eq!(l.config.interval_ms, 500);
        assert_eq!(l.config.theme, "nord");
        assert_eq!(l.config.rtss_ignored, vec!["obs64.exe"]);
        assert_eq!(
            l.config.colors,
            vec![("border".to_string(), "#112233".to_string())]
        );
        let mut c = l.config.clone();
        c.interval_ms = 1500;
        c.fold_pool = FoldPool::Fold;
        l.save(&c).expect("gravação");
        let text = std::fs::read_to_string(&t.file).expect("leitura");
        assert!(text.contains("# meu comentário"), "{text}");
        assert!(
            text.contains("interval_ms = 1500") && text.contains("fold_pool = \"fold\""),
            "{text}"
        );
        assert!(
            text.contains("obs64.exe") && text.contains("#112233"),
            "{text}"
        );
    }

    #[test]
    fn arquivo_com_erro_abre_com_padroes_e_nao_e_regravado() {
        let t = temp("erro");
        std::fs::create_dir_all(t.file.parent().expect("pasta")).expect("pasta");
        let broken = "interval_ms = = 3\n";
        std::fs::write(&t.file, broken).expect("escrita");
        let mut l = load_in(Some(t.base.clone()), &t.dir, true);
        assert_eq!(l.config, Config::default());
        assert!(l.warnings[0].contains("syntax error"), "{:?}", l.warnings);
        assert!(l.save(&Config::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(&t.file).expect("leitura"),
            broken,
            "edição do usuário intacta"
        );
    }

    #[test]
    fn rtss_que_nao_e_tabela_nao_derruba_a_gravacao() {
        let t = temp("rtss");
        std::fs::create_dir_all(t.file.parent().expect("pasta")).expect("pasta");
        std::fs::write(&t.file, "rtss = \"off\"\ncolors = 123\n").expect("escrita");
        let mut l = load_in(Some(t.base.clone()), &t.dir, true);
        assert!(
            l.warnings
                .iter()
                .any(|w| w.contains("rtss must be a table")),
            "{:?}",
            l.warnings
        );
        assert!(
            l.warnings
                .iter()
                .any(|w| w.contains("colors must be a table")),
            "{:?}",
            l.warnings
        );
        let mut c = l.config.clone();
        c.interval_ms = 700;
        l.save(&c).expect("gravação sem pânico");
        let text = std::fs::read_to_string(&t.file).expect("leitura");
        assert!(
            text.contains("rtss = \"off\"") && text.contains("interval_ms = 700"),
            "{text}"
        );
    }

    #[test]
    fn caminho_portatil_na_raiz_do_disco() {
        let loc = ConfigDir::Portable(PathBuf::from(r"D:\"));
        assert_eq!(loc.file_path(), PathBuf::from(r"D:\config.toml"));
    }

    #[test]
    fn valores_invalidos_ficam_no_padrao_com_aviso() {
        let doc: DocumentMut =
            "interval_ms = -5\ntheme = \"\\u001b[31m\"\nfold_pool = 3\n[colors]\nborder = \"red\"\nnope = \"#000000\"\n"
                .parse()
                .expect("toml");
        let mut w = Vec::new();
        let c = from_doc(&doc, &mut w);
        assert_eq!(c.interval_ms, DEFAULT_INTERVAL_MS);
        assert_eq!(c.theme, "default");
        assert_eq!(c.fold_pool, FoldPool::Auto);
        assert!(c.colors.is_empty());
        assert_eq!(w.len(), 5, "{w:?}");
        // Nada de controle chega à tela pelo aviso.
        assert!(w.iter().all(|x| !x.chars().any(char::is_control)), "{w:?}");
        // Intervalo fora dos limites é ajustado (não recusado).
        let doc: DocumentMut = "interval_ms = 99999".parse().expect("toml");
        assert_eq!(from_doc(&doc, &mut Vec::new()).interval_ms, 10_000);
    }
}

#[cfg(test)]
mod tests_1_1 {
    use super::*;

    #[test]
    fn idioma_da_interface() {
        use crate::i18n::Lang;
        let read = |s: &str| {
            let doc: DocumentMut = s.parse().expect("toml");
            let mut warn = Vec::new();
            (from_doc(&doc, &mut warn).language, warn.len())
        };
        assert_eq!(read("language = \"pt-BR\"\n"), (Lang::PtBr, 0));
        assert_eq!(read("language = \"en-US\"\n"), (Lang::EnUs, 0));
        // Código desconhecido: padrão (inglês) e aviso.
        assert_eq!(read("language = \"pt\"\n"), (Lang::EnUs, 1));
        assert_eq!(read("language = 1\n"), (Lang::EnUs, 1));
        // O arquivo da 1ª execução traz a opção e é lido de volta igual.
        let c = Config {
            language: Lang::PtBr,
            ..Config::default()
        };
        assert_eq!(read(&template(&c)), (Lang::PtBr, 0));
    }

    #[test]
    fn alerta_do_frametime_e_unidade_da_rede() {
        let doc: DocumentMut = "frametime_alert_ms = 16.7\nnet_unit = \"bytes\"\n"
            .parse()
            .expect("toml");
        let mut warn = Vec::new();
        let c = from_doc(&doc, &mut warn);
        assert!(warn.is_empty(), "{warn:?}");
        assert!((c.frametime_alert_ms - 16.7).abs() < 1e-9 && c.net_bytes);
        // Inteiro também vale; fora da faixa ou unidade desconhecida: padrão e aviso.
        let doc: DocumentMut = "frametime_alert_ms = 2\nnet_unit = \"kbps\"\n"
            .parse()
            .expect("toml");
        let mut warn = Vec::new();
        let c = from_doc(&doc, &mut warn);
        assert_eq!(warn.len(), 2, "{warn:?}");
        assert_eq!(c.frametime_alert_ms, DEFAULT_FRAMETIME_ALERT_MS);
        assert!(!c.net_bytes);
        // O arquivo criado na 1ª execução traz as duas opções e é lido de volta igual.
        let t = template(&Config::default());
        let back = from_doc(&t.parse().expect("modelo"), &mut Vec::new());
        assert_eq!(back, Config::default());
    }

    #[test]
    fn modo_debug_no_config() {
        let read = |s: &str| {
            let doc: DocumentMut = s.parse().expect("toml");
            let mut warn = Vec::new();
            (from_doc(&doc, &mut warn).debug, warn.len())
        };
        assert_eq!(read(""), (false, 0));
        assert_eq!(read("debug = true\n"), (true, 0));
        assert_eq!(read("debug = false\n"), (false, 0));
        assert_eq!(read("debug = 1\n"), (false, 1));
        let c = Config {
            debug: true,
            ..Config::default()
        };
        assert_eq!(read(&template(&c)), (true, 0));
    }
}
