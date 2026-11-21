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

use std::path::{Path, PathBuf};

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
}

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
        "# wintop configuration (edit while wintop is closed; it saves changes made in the UI).\n\
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
         [rtss]\n\
         # Apps hooked by RTSS that are never treated as the game (lowercase executable names).\n\
         ignored = [\n{}]\n\
         \n\
         [colors]\n\
         # Override single theme colors with \"#rrggbb\", e.g. border = \"#4b5263\".\n\
         # Names: border, title, text, label, neutral, fail, selected, dim.\n",
        c.interval_ms,
        c.theme,
        c.fold_pool.name(),
        ignored.concat()
    )
}

/// Configuração carregada: os valores, o documento (para regravar mantendo comentários) e
/// os avisos (valores inválidos, arquivo com erro).
pub struct Loaded {
    pub config: Config,
    doc: Option<DocumentMut>,
    path: Option<PathBuf>,
    /// Problemas encontrados (a interface mostra o primeiro).
    pub warnings: Vec<String>,
    /// Gravação liberada (não com o arquivo do usuário em erro de sintaxe).
    writable: bool,
}

/// Caminho do arquivo: `%APPDATA%\wintop\config.toml` (pasta do perfil pelo Known Folder, não
/// pela variável de ambiente).
pub fn default_path() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    // SAFETY: GUID estático; a string devolvida é liberada com CoTaskMemFree.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0.cast()));
        Some(PathBuf::from(s?).join("wintop").join("config.toml"))
    }
}

/// Lê (ou cria) o arquivo em `path`.
pub fn load(path: Option<PathBuf>) -> Loaded {
    let mut l = Loaded {
        config: Config::default(),
        doc: None,
        path: path.clone(),
        warnings: Vec::new(),
        writable: path.is_some(),
    };
    let Some(path) = path else {
        l.warnings
            .push("config: no AppData folder (defaults, not saved)".into());
        return l;
    };
    match std::fs::metadata(&path) {
        Err(_) => {
            // 1ª execução: cria com os padrões.
            let text = template(&l.config);
            if let Err(e) = write_atomic(&path, &text) {
                l.warnings
                    .push(format!("config: could not create {}: {e}", path.display()));
                l.writable = false;
            }
            l.doc = text.parse().ok();
            return l;
        }
        Ok(m) if m.len() > MAX_BYTES => {
            l.warnings.push(format!(
                "config: file larger than {} KB (defaults, not saved)",
                MAX_BYTES >> 10
            ));
            l.writable = false;
            return l;
        }
        Ok(_) => {}
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            l.warnings.push(format!(
                "config: could not read it: {e} (defaults, not saved)"
            ));
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
                "config: syntax error ({first}); defaults, not saved"
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

impl Loaded {
    /// Grava os valores que a interface muda (intervalo, tema, pool), mantendo o resto do
    /// documento. Devolve o erro para a interface avisar.
    pub fn save(&mut self, c: &Config) -> Result<(), String> {
        if !self.writable {
            return Err("config not saved (see the warning)".into());
        }
        let path = self.path.clone().ok_or("no config path")?;
        let doc = self
            .doc
            .get_or_insert_with(|| template(c).parse().unwrap_or_default());
        doc["interval_ms"] = value(c.interval_ms as i64);
        doc["theme"] = value(c.theme.as_str());
        doc["fold_pool"] = value(c.fold_pool.name());
        if doc.get("rtss").and_then(|t| t.get("ignored")).is_none() {
            let mut a = Array::new();
            for n in &c.rtss_ignored {
                a.push(n.as_str());
            }
            doc["rtss"]["ignored"] = value(a);
        }
        write_atomic(&path, &doc.to_string()).map_err(|e| e.to_string())?;
        self.config = c.clone();
        Ok(())
    }
}

/// Grava em `<arquivo>.tmp` na mesma pasta e troca pelo original (um corte de energia no meio
/// deixa o antigo ou o novo, nunca metade).
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wintop_cfg_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("config.toml")
    }

    #[test]
    fn primeira_execucao_cria_com_os_padroes() {
        let p = temp("novo");
        let l = load(Some(p.clone()));
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.config, Config::default());
        let text = std::fs::read_to_string(&p).expect("arquivo criado");
        assert!(
            text.contains("interval_ms = 1000") && text.contains("\"discord.exe\""),
            "{text}"
        );
        // O arquivo criado relido dá os mesmos valores.
        assert_eq!(load(Some(p.clone())).config, Config::default());
        let _ = std::fs::remove_dir_all(p.parent().expect("pasta"));
    }

    #[test]
    fn gravar_preserva_comentarios_e_o_resto() {
        let p = temp("grava");
        std::fs::create_dir_all(p.parent().expect("pasta")).expect("pasta");
        std::fs::write(
            &p,
            "# meu comentário\ninterval_ms = 500\ntheme = \"nord\"\n\n[rtss]\nignored = [\"obs64.exe\"]\n\n[colors]\nborder = \"#112233\"\n",
        )
        .expect("escrita");
        let mut l = load(Some(p.clone()));
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
        let text = std::fs::read_to_string(&p).expect("leitura");
        assert!(text.contains("# meu comentário"), "{text}");
        assert!(
            text.contains("interval_ms = 1500") && text.contains("fold_pool = \"fold\""),
            "{text}"
        );
        assert!(
            text.contains("obs64.exe") && text.contains("#112233"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(p.parent().expect("pasta"));
    }

    #[test]
    fn arquivo_com_erro_abre_com_padroes_e_nao_e_regravado() {
        let p = temp("erro");
        std::fs::create_dir_all(p.parent().expect("pasta")).expect("pasta");
        let broken = "interval_ms = = 3\n";
        std::fs::write(&p, broken).expect("escrita");
        let mut l = load(Some(p.clone()));
        assert_eq!(l.config, Config::default());
        assert!(l.warnings[0].contains("syntax error"), "{:?}", l.warnings);
        assert!(l.save(&Config::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(&p).expect("leitura"),
            broken,
            "edição do usuário intacta"
        );
        let _ = std::fs::remove_dir_all(p.parent().expect("pasta"));
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
