//! Téléchargements du navigateur : état des items CDP, dossier de
//! destination et noms de fichiers sûrs.
//!
//! Le chemin d'écriture appartient à Chrome (`Browser.setDownloadBehavior`
//! avec `behavior: "allow"` + `downloadPath`) : Komet ne crée jamais de
//! fichier lui-même, il affiche la progression et ouvre/révèle le résultat.
//! `suggestedFilename` n'est donc utilisé que pour l'affichage et les
//! libellés — jamais pour composer un chemin.

use std::path::{Path, PathBuf};

/// État d'un téléchargement (`Browser.downloadProgress.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    InProgress,
    Completed,
    Canceled,
}

impl DownloadState {
    /// `state` brut de `Browser.downloadProgress`.
    pub fn from_cdp(value: &str) -> Option<Self> {
        match value {
            "inProgress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }
}

/// Un téléchargement suivi, du `downloadWillBegin` au `downloadProgress`
/// terminal. Identité = GUID CDP (stable sur toute la vie de l'item).
#[derive(Debug, Clone)]
pub struct DownloadItem {
    /// GUID CDP, stable du début à la fin.
    pub id: String,
    /// `suggestedFilename` — affichage uniquement, jamais utilisé comme chemin.
    pub filename: String,
    /// URL d'origine (affichage).
    pub url: String,
    pub state: DownloadState,
    pub received: u64,
    /// `totalBytes` : `-1` CDP (taille inconnue) → `None`.
    pub total: Option<u64>,
    /// Chemin final une fois terminé (`downloadProgress.filePath`).
    pub path: Option<PathBuf>,
}

impl DownloadItem {
    /// Progression 0..1 ; `None` si la taille totale est inconnue ou nulle.
    pub fn progress(&self) -> Option<f32> {
        let total = self.total?;
        (total > 0).then(|| self.received.min(total) as f32 / total as f32)
    }

    /// Vrai quand l'item n'évoluera plus (terminé ou annulé).
    pub fn is_terminal(&self) -> bool {
        self.state != DownloadState::InProgress
    }
}

/// Nom de fichier sûr : affichage et libellés uniquement. Les séparateurs et
/// la traversée (`..`, `..\\`) deviennent `_`, les caractères de contrôle sont
/// retirés, la longueur est bornée ; vide → « download ».
pub fn safe_filename(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '|' | '"' | '<' | '>' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    // Les segments « . » / « .. » seuls ou en début de nom : neutralisés.
    if cleaned.starts_with('.') {
        cleaned.insert(0, '_');
    }
    let cleaned = cleaned.trim().trim_matches('.').trim();
    let mut owned: String = cleaned.chars().take(180).collect();
    if owned.is_empty() {
        owned = "download".into();
    }
    owned
}

/// Dossier de téléchargement : réglage utilisateur (créé au besoin), sinon le
/// dossier Téléchargements de l'OS, sinon le répertoire temporaire. Toujours
/// un chemin existant : `Browser.setDownloadBehavior` exige un dossier réel.
pub fn resolve_download_dir(pref: Option<&str>) -> PathBuf {
    if let Some(dir) = pref
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(expand_home)
        .map(PathBuf::from)
        .filter(|dir| std::fs::create_dir_all(dir).is_ok())
    {
        return dir;
    }
    if let Some(dir) = dirs::download_dir()
        && std::fs::create_dir_all(&dir).is_ok()
    {
        return dir;
    }
    let fallback = std::env::temp_dir().join("komet-downloads");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// `~/…` → home (les réglages acceptent le raccourci, comme le dossier des
/// captures dans Settings).
fn expand_home(path: &str) -> String {
    if let Some(home) = path
        .strip_prefix("~/")
        .filter(|_| path != "~")
        .and_then(|_| dirs::home_dir())
    {
        return home.join(&path[2..]).to_string_lossy().into_owned();
    }
    if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home.to_string_lossy().into_owned();
    }
    path.to_owned()
}

/// Mise à jour d'un item dans la liste, par GUID. Crée l'item au premier
/// événement (`DownloadStart`), laisse les suivants le faire évoluer.
#[allow(clippy::too_many_arguments)] // un champ CDP par paramètre, tous optionnels
pub fn apply_event(
    items: &mut Vec<DownloadItem>,
    id: &str,
    filename: &str,
    url: &str,
    state: Option<DownloadState>,
    received: Option<u64>,
    total: Option<u64>,
    path: Option<&Path>,
) {
    let entry = match items.iter_mut().find(|item| item.id == id) {
        Some(item) => item,
        None => {
            items.push(DownloadItem {
                id: id.to_owned(),
                filename: safe_filename(filename),
                url: url.to_owned(),
                state: DownloadState::InProgress,
                received: 0,
                total,
                path: None,
            });
            items.last_mut().expect("just pushed")
        }
    };
    if !filename.is_empty() {
        entry.filename = safe_filename(filename);
    }
    if let Some(state) = state {
        entry.state = state;
    }
    if let Some(received) = received {
        entry.received = received;
    }
    if let Some(total) = total {
        entry.total = Some(total);
    }
    if let Some(path) = path {
        entry.path = Some(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_filename_neutralizes_traversal_and_separators() {
        assert_eq!(safe_filename("../../etc/passwd"), "_.._.._etc_passwd");
        assert_eq!(safe_filename("a/b\\c.txt"), "a_b_c.txt");
        assert_eq!(safe_filename("report: final?.pdf"), "report_ final_.pdf");
        assert_eq!(safe_filename(".."), "_");
        assert_eq!(safe_filename("   "), "download");
        assert_eq!(safe_filename(""), "download");
        // caractères de contrôle neutralisés
        assert_eq!(safe_filename("a\u{7}b.zip"), "a_b.zip");
    }

    #[test]
    fn progress_handles_unknown_total() {
        let mut items = Vec::new();
        apply_event(
            &mut items,
            "g1",
            "file.zip",
            "http://x/",
            None,
            Some(10),
            None,
            None,
        );
        assert_eq!(items[0].progress(), None);
        assert!(!items[0].is_terminal());

        apply_event(
            &mut items,
            "g1",
            "",
            "",
            Some(DownloadState::Completed),
            Some(100),
            Some(100),
            Some(Path::new("/tmp/dl/file.zip")),
        );
        assert_eq!(items[0].progress(), Some(1.0));
        assert!(items[0].is_terminal());
        assert_eq!(
            items[0].path.as_deref(),
            Some(Path::new("/tmp/dl/file.zip"))
        );
        // le nom du premier événement est conservé
        assert_eq!(items[0].filename, "file.zip");
    }

    #[test]
    fn apply_event_creates_on_first_sight_and_updates_after() {
        let mut items = Vec::new();
        apply_event(
            &mut items,
            "g1",
            "doc.pdf",
            "http://x/",
            None,
            None,
            Some(200),
            None,
        );
        apply_event(
            &mut items,
            "g2",
            "other.zip",
            "http://y/",
            None,
            None,
            Some(1),
            None,
        );
        apply_event(&mut items, "g1", "", "", None, Some(50), None, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].received, 50);
        assert_eq!(items[0].total, Some(200));
    }

    #[test]
    fn resolve_download_dir_creates_the_configured_folder() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = dir.path().join("dl");
        let resolved = resolve_download_dir(Some(chosen.to_str().unwrap()));
        assert_eq!(resolved, chosen);
        assert!(chosen.is_dir());
        // défaut = dossier Téléchargements de l'OS (existant ou créé)
        let fallback = resolve_download_dir(None);
        assert!(fallback.is_dir() || fallback.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn resolve_download_dir_expands_tilde() {
        let resolved = resolve_download_dir(Some("~/komet-test-dl"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(resolved, home.join("komet-test-dl"));
            let _ = std::fs::remove_dir(&resolved);
        }
    }
}
