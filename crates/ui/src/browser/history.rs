//! Historique et favoris du navigateur.
//!
//! `crates/ui` n'a pas de dépendance SQLite : le stockage est un JSONL
//! append-only (`browser-history.jsonl`) + un JSON (`browser-favorites.json`)
//! sous `{data_dir}/browser/`, compacté au chargement. L'index reste en
//! mémoire (≤ [`MAX_ENTRIES`] entrées — quelques centaines de Ko).
//!
//! Le cœur est pur et testé : règles d'enregistrement (déduplication par URL,
//! fenêtre de rechargement, plafond, rétention), score de « frecency »,
//! classement des suggestions et regroupement par jour.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Plafond d'entrées conservées en mémoire et sur disque.
pub const MAX_ENTRIES: usize = 5000;
/// Un rechargement de la même URL dans cette fenêtre ne recompte pas une
/// visite (mais met à jour `last_visit` et le titre).
pub const RELOAD_WINDOW_MS: i64 = 30_000;
/// Rétention par défaut : 90 jours (`None` = illimité, réglage PR 4).
pub const DEFAULT_RETENTION_DAYS: u32 = 90;
/// Demi-vie du score de frecency : une visite « pèse » moitié moins après
/// 30 jours.
const FRECENCY_HALF_LIFE_MS: f64 = 30.0 * 24.0 * 3600.0 * 1000.0;
/// Les favoris sont boostés dans le classement des suggestions.
const FAVORITE_BOOST: f64 = 3.0;
const SUGGESTION_LIMIT: usize = 6;
const DAY_MS: i64 = 24 * 3600 * 1000;

/// Une URL d'historique : une entrée par URL (fragment `#…` ignoré pour la
/// déduplication, conservé dans `url`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    /// Millisecondes epoch de la dernière visite.
    pub last_visit: i64,
    pub visit_count: u32,
}

/// Favori : la même entrée d'historique + date d'ajout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    pub url: String,
    pub title: String,
    pub added: i64,
}

/// Portée d'un « Clear history… » : comme le navigateur, on efface les
/// visites DE la période, pas celles d'avant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearScope {
    /// Les entrées des 60 dernières minutes.
    Hour,
    /// Tout ce qui a été visité aujourd'hui.
    Today,
    /// Tout l'historique (les favoris survivent).
    All,
}

/// Vide les fichiers de stockage (historique + favoris) sous
/// `{data_dir}/browser/` — « Clear history… » / « Clear browsing data… » de
/// Settings → Browser. Les surfaces déjà ouvertes gardent leur copie en
/// mémoire jusqu'à leur prochaine ouverture (le dialogue Clear Storage de la
/// PR 8 refait le ménage côté CDP).
pub fn clear_storage_files(data_dir: &Path) -> std::io::Result<()> {
    let dir = data_dir.join("browser");
    for name in ["browser-history.jsonl", "browser-favorites.json"] {
        let path = dir.join(name);
        if path.exists() {
            std::fs::write(&path, "")?;
        }
    }
    Ok(())
}

/// Vide UNIQUEMENT le journal d'historique — les favoris survivent.
/// « Clear Storage » coche « History » : les surfaces réécrivent le
/// fichier depuis leur mémoire purgée, ceci garantit le cas où aucune
/// surface n'a de store (data_dir absent).
pub fn clear_history_file(data_dir: &Path) -> std::io::Result<()> {
    let path = data_dir.join("browser").join("browser-history.jsonl");
    if path.exists() {
        std::fs::write(&path, "")?;
    }
    Ok(())
}

/// Regroupement par jour pour le panneau : Aujourd'hui / Hier / date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DayGroup {
    Today,
    Yesterday,
    Date(String),
}

/// Une ligne de suggestion de la barre d'adresse.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// `true` pour la ligne « Search … for "requête" ».
    pub is_search: bool,
    /// Libellé principal (titre de page, ou « Search Engine for "q" »).
    pub title: String,
    /// URL à naviguer, ou le texte de recherche pour `is_search`.
    pub target: String,
    /// Hôte affiché en `text_muted` (`None` pour la ligne recherche).
    pub host: Option<String>,
}

/// Clé de déduplication : URL sans fragment ni slash final superflu.
pub fn history_key(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

/// Vrai pour les URL enregistrables : http(s) seulement.
pub fn is_recordable_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// `visit_count` pondéré par décroissance exponentielle de la récence
/// (demi-vie 30 jours). Plus il y a de visites récentes, plus le score est haut.
pub fn frecency(entry: &HistoryEntry, now_ms: i64) -> f64 {
    let age_ms = (now_ms - entry.last_visit).max(0) as f64;
    entry.visit_count as f64 * 0.5f64.powf(age_ms / FRECENCY_HALF_LIFE_MS)
}

/// La saisie ressemble-t-elle à une URL plutôt qu'à une recherche ?
/// Mêmes règles que `normalize_url` : pas d'espace + un point, un schéma,
/// ou une cible locale (`localhost`, `:3000`, IP).
pub fn looks_like_url(query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return false;
    }
    let lower = q.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return true;
    }
    if lower.starts_with("localhost") || lower.starts_with("127.") {
        return true;
    }
    if let Some(port) = q.strip_prefix(':') {
        return !port.is_empty() && port.chars().all(|c| c.is_ascii_digit());
    }
    q.contains('.')
}

/// Enregistre une visite dans `entries` (dédupliquées par [`history_key`]).
/// Retourne `true` si la liste a changé (nouvelle entrée ou compteur/titre).
///
/// Règles : http(s) seulement ; même URL à moins de [`RELOAD_WINDOW_MS`] →
/// `last_visit`/titre mis à jour sans recompter ; titre vide ignoré ;
/// plafond [`MAX_ENTRIES`] par éviction des plus anciennes.
pub fn record_visit(entries: &mut Vec<HistoryEntry>, url: &str, title: &str, now_ms: i64) -> bool {
    if !is_recordable_url(url) {
        return false;
    }
    let key = history_key(url).to_string();
    if let Some(entry) = entries.iter_mut().find(|e| history_key(&e.url) == key) {
        let mut changed = false;
        if entry.last_visit != now_ms {
            // Fenêtre de rechargement : la visite « compte » pour la
            // récence mais pas pour le compteur.
            if now_ms - entry.last_visit >= RELOAD_WINDOW_MS {
                entry.visit_count = entry.visit_count.saturating_add(1);
            }
            entry.last_visit = now_ms;
            changed = true;
        }
        if !title.trim().is_empty() && entry.title != title {
            entry.title = title.to_string();
            changed = true;
        }
        return changed;
    }
    entries.push(HistoryEntry {
        url: url.to_string(),
        title: title.to_string(),
        last_visit: now_ms,
        visit_count: 1,
    });
    entries.sort_by_key(|e| std::cmp::Reverse(e.last_visit));
    if entries.len() > MAX_ENTRIES {
        entries.truncate(MAX_ENTRIES);
    }
    true
}

/// Met à jour le titre d'une URL déjà connue (événement `Title` tardif).
pub fn update_title(entries: &mut [HistoryEntry], url: &str, title: &str) -> bool {
    if title.trim().is_empty() || !is_recordable_url(url) {
        return false;
    }
    let key = history_key(url);
    match entries.iter_mut().find(|e| history_key(&e.url) == key) {
        Some(entry) if entry.title != title => {
            entry.title = title.to_string();
            true
        }
        _ => false,
    }
}

/// Suggestions pour la barre d'adresse : jusqu'à [`SUGGESTION_LIMIT`] lignes,
/// classées par frecency (favoris boostés). La première ligne est « Search …
/// for "q" » quand la saisie ne ressemble pas à une URL.
pub fn suggestions(
    entries: &[HistoryEntry],
    favorites: &[Favorite],
    query: &str,
    search_label: &str,
    now_ms: i64,
) -> Vec<Suggestion> {
    let q = query.trim().to_lowercase();
    let mut scored: Vec<(f64, &HistoryEntry)> = entries
        .iter()
        .filter(|e| {
            q.is_empty() || e.url.to_lowercase().contains(&q) || e.title.to_lowercase().contains(&q)
        })
        .map(|e| {
            let mut score = frecency(e, now_ms);
            if favorites
                .iter()
                .any(|f| history_key(&f.url) == history_key(&e.url))
            {
                score *= FAVORITE_BOOST;
            }
            (score, e)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut out: Vec<Suggestion> = scored
        .into_iter()
        .take(SUGGESTION_LIMIT)
        .map(|(_, e)| Suggestion {
            is_search: false,
            title: if e.title.trim().is_empty() {
                e.url.clone()
            } else {
                e.title.clone()
            },
            host: host_of_url(&e.url),
            target: e.url.clone(),
        })
        .collect();

    if !q.is_empty() && !looks_like_url(query) {
        out.insert(
            0,
            Suggestion {
                is_search: true,
                title: format!("{search_label} for \"{}\"", query.trim()),
                target: query.trim().to_string(),
                host: None,
            },
        );
        out.truncate(SUGGESTION_LIMIT + 1);
    }
    out
}

/// Regroupe des entrées (récentes d'abord) en Aujourd'hui / Hier / dates.
/// `now_ms` et les timestamps sont en epoch ms ; le jour est calculé en
/// jours calendaires UTC (±12 h d'erreur acceptable pour un panneau).
pub fn group_by_day(entries: &[HistoryEntry], now_ms: i64) -> Vec<(DayGroup, Vec<HistoryEntry>)> {
    let today_start = now_ms.div_euclid(DAY_MS) * DAY_MS;
    let mut groups: Vec<(DayGroup, Vec<HistoryEntry>)> = Vec::new();
    for entry in entries {
        let group = if entry.last_visit >= today_start {
            DayGroup::Today
        } else if entry.last_visit >= today_start - DAY_MS {
            DayGroup::Yesterday
        } else {
            DayGroup::Date(format_date(entry.last_visit))
        };
        match groups.last_mut() {
            Some((last, items)) if *last == group => items.push(entry.clone()),
            _ => groups.push((group, vec![entry.clone()])),
        }
    }
    groups
}

fn format_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn host_of_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.split(':').next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_string())
}

/// Supprime les entrées selon la portée. Retourne le nombre supprimé.
pub fn clear_entries(entries: &mut Vec<HistoryEntry>, scope: ClearScope, now_ms: i64) -> usize {
    let before = entries.len();
    match scope {
        ClearScope::All => entries.clear(),
        ClearScope::Hour => {
            entries.retain(|e| now_ms - e.last_visit >= 3_600_000);
        }
        ClearScope::Today => {
            let today_start = now_ms.div_euclid(DAY_MS) * DAY_MS;
            entries.retain(|e| e.last_visit < today_start);
        }
    }
    before - entries.len()
}

/// Historique + favoris persistés. Toutes les méthodes sont synchrones et
/// bon marché : le fichier n'est réécrit qu'à [`BrowserHistory::flush`].
pub struct BrowserHistory {
    entries: Vec<HistoryEntry>,
    favorites: Vec<Favorite>,
    dir: PathBuf,
    retention_days: Option<u32>,
    dirty: bool,
}

impl BrowserHistory {
    /// Charge (ou crée) le stockage sous `data_dir/browser/`. Un fichier
    /// JSONL corrompu ne bloque pas : les lignes invalides sont ignorées et
    /// le fichier est compacté à la prochaine écriture.
    pub fn load(data_dir: &Path) -> Self {
        let dir = data_dir.join("browser");
        let history_path = dir.join("browser-history.jsonl");
        let mut entries: BTreeMap<String, HistoryEntry> = BTreeMap::new();
        if let Ok(contents) = std::fs::read_to_string(&history_path) {
            for line in contents.lines() {
                if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line)
                    && is_recordable_url(&entry.url)
                {
                    let key = history_key(&entry.url).to_string();
                    match entries.get(&key) {
                        Some(existing) if existing.last_visit >= entry.last_visit => {}
                        _ => {
                            entries.insert(key, entry);
                        }
                    }
                }
            }
        }
        let mut entries: Vec<HistoryEntry> = entries.into_values().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_visit));
        entries.truncate(MAX_ENTRIES);

        let favorites_path = dir.join("browser-favorites.json");
        let favorites = std::fs::read_to_string(&favorites_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Favorite>>(&text).ok())
            .unwrap_or_default();

        let mut history = Self {
            entries,
            favorites,
            dir,
            retention_days: Some(DEFAULT_RETENTION_DAYS),
            dirty: false,
        };
        history.prune_retention(chrono::Utc::now().timestamp_millis());
        history
    }

    /// Enregistre une visite (règles de [`record_visit`]) puis journalise
    /// l'entrée résultante — le JSONL est une trace, le rechargement déduplique
    /// par URL en gardant la ligne au `last_visit` le plus récent.
    pub fn record(&mut self, url: &str, title: &str, now_ms: i64) {
        if !record_visit(&mut self.entries, url, title, now_ms) {
            return;
        }
        self.prune_retention(now_ms);
        let key = history_key(url);
        if let Some(entry) = self.entries.iter().find(|e| history_key(&e.url) == key)
            && let Ok(line) = serde_json::to_value(entry)
        {
            self.append_line(&line);
        }
        self.dirty = true;
    }

    /// Met à jour le titre d'une URL connue (événement `Title` tardif).
    pub fn record_title(&mut self, url: &str, title: &str) -> bool {
        if update_title(&mut self.entries, url, title) {
            self.dirty = true;
            true
        } else {
            false
        }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn favorites(&self) -> &[Favorite] {
        &self.favorites
    }

    pub fn is_favorite(&self, url: &str) -> bool {
        let key = history_key(url);
        self.favorites.iter().any(|f| history_key(&f.url) == key)
    }

    /// Bascule un favori ; retourne le nouvel état. Le titre vient de
    /// l'historique ou de l'appelant.
    pub fn toggle_favorite(&mut self, url: &str, title: &str, now_ms: i64) -> bool {
        let key = history_key(url).to_string();
        if let Some(ix) = self
            .favorites
            .iter()
            .position(|f| history_key(&f.url) == key)
        {
            self.favorites.remove(ix);
            self.save_favorites();
            return false;
        }
        self.favorites.push(Favorite {
            url: url.to_string(),
            title: if title.trim().is_empty() {
                self.entries
                    .iter()
                    .find(|e| history_key(&e.url) == key)
                    .map(|e| e.title.clone())
                    .unwrap_or_else(|| url.to_string())
            } else {
                title.to_string()
            },
            added: now_ms,
        });
        self.favorites
            .sort_by(|a, b| b.added.cmp(&a.added).then(a.url.cmp(&b.url)));
        self.save_favorites();
        true
    }

    /// Supprime une entrée d'historique (✕ du panneau).
    pub fn remove(&mut self, url: &str) {
        let key = history_key(url);
        let before = self.entries.len();
        self.entries.retain(|e| history_key(&e.url) != key);
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    /// « Clear history… » : portée choisie par l'utilisateur.
    pub fn clear(&mut self, scope: ClearScope, now_ms: i64) {
        if clear_entries(&mut self.entries, scope, now_ms) > 0 {
            self.dirty = true;
        }
    }

    /// Tout l'historique, favoris compris — Settings → « Clear history… »
    /// (sa méta annonce « history and favorites »). Le journal est vidé à
    /// la prochaine [`BrowserHistory::flush`].
    pub fn clear_all(&mut self) {
        self.entries.clear();
        self.favorites.clear();
        self.dirty = true;
        self.save_favorites();
    }

    /// Réglage de rétention (`None` = illimité) — PR 4 l'expose en settings.
    pub fn set_retention_days(&mut self, days: Option<u32>) {
        self.retention_days = days;
        self.prune_retention(chrono::Utc::now().timestamp_millis());
        self.dirty = true;
    }

    pub fn suggestions(&self, query: &str, search_label: &str, now_ms: i64) -> Vec<Suggestion> {
        suggestions(&self.entries, &self.favorites, query, search_label, now_ms)
    }

    /// Entrées récentes pour l'état vide (« Recent », 5 max).
    pub fn recent(&self, limit: usize) -> Vec<HistoryEntry> {
        self.entries.iter().take(limit).cloned().collect()
    }

    /// Entrées groupées par jour pour le panneau History.
    pub fn grouped(&self, now_ms: i64) -> Vec<(DayGroup, Vec<HistoryEntry>)> {
        group_by_day(&self.entries, now_ms)
    }

    /// Compacte le JSONL : réécrit le fichier depuis l'index en mémoire.
    /// Appelé au chargement si le journal a dérivé, et après les clears.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let path = self.dir.join("browser-history.jsonl");
        let mut text = String::new();
        for entry in &self.entries {
            if let Ok(line) = serde_json::to_string(entry) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        if std::fs::create_dir_all(&self.dir).is_ok() {
            let tmp = self.dir.join("browser-history.jsonl.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        self.dirty = false;
    }

    fn prune_retention(&mut self, now_ms: i64) {
        let Some(days) = self.retention_days else {
            return;
        };
        let cutoff = now_ms - days as i64 * DAY_MS;
        let before = self.entries.len();
        self.entries.retain(|e| e.last_visit >= cutoff);
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    fn append_line(&self, value: &serde_json::Value) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("browser-history.jsonl"))
        {
            let _ = writeln!(file, "{value}");
        }
    }

    fn save_favorites(&self) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.favorites) {
            let _ = std::fs::write(self.dir.join("browser-favorites.json"), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, title: &str, last_visit: i64, count: u32) -> HistoryEntry {
        HistoryEntry {
            url: url.into(),
            title: title.into(),
            last_visit,
            visit_count: count,
        }
    }

    #[test]
    fn record_visit_dedupes_by_url_ignoring_fragment() {
        let mut entries = Vec::new();
        assert!(record_visit(
            &mut entries,
            "https://a.com/x#one",
            "A",
            1_000
        ));
        assert!(record_visit(
            &mut entries,
            "https://a.com/x#two",
            "A2",
            40_000
        ));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].visit_count, 2);
        assert_eq!(entries[0].title, "A2");
    }

    #[test]
    fn record_visit_rejects_non_http_and_reload_window() {
        let mut entries = Vec::new();
        assert!(!record_visit(&mut entries, "about:blank", "", 0));
        assert!(!record_visit(&mut entries, "file:///etc/passwd", "", 0));
        assert!(record_visit(&mut entries, "https://a.com", "A", 100_000));
        // Même URL 10 s plus tard : last_visit bouge, compteur inchangé.
        assert!(record_visit(&mut entries, "https://a.com", "A", 110_000));
        assert_eq!(entries[0].visit_count, 1);
        assert_eq!(entries[0].last_visit, 110_000);
        // 40 s plus tard : recompte.
        assert!(record_visit(&mut entries, "https://a.com", "A", 150_000));
        assert_eq!(entries[0].visit_count, 2);
    }

    #[test]
    fn frecency_decays_with_age() {
        let fresh = entry("https://a.com", "", 1_000_000, 2);
        let old = entry("https://b.com", "", 1_000_000 - 90 * DAY_MS, 2);
        let now = 1_000_000 + 3_600_000;
        assert!(frecency(&fresh, now) > frecency(&old, now));
        assert!((frecency(&fresh, now) - 2.0).abs() < 0.01);
    }

    #[test]
    fn suggestions_rank_by_frecency_and_boost_favorites() {
        let now = 10 * DAY_MS;
        let entries = vec![
            entry("https://old.com", "Old", now - 80 * DAY_MS, 9),
            entry("https://fav.com", "Fav", now - DAY_MS, 1),
        ];
        let favorites = vec![Favorite {
            url: "https://fav.com".into(),
            title: "Fav".into(),
            added: now,
        }];
        let out = suggestions(&entries, &favorites, "", "Search", now);
        assert_eq!(out[0].target, "https://fav.com");
    }

    #[test]
    fn suggestions_prepend_search_row_for_non_url_queries() {
        let entries = vec![entry("https://a.com", "A", 1, 1)];
        let out = suggestions(&entries, &[], "rust lang", "Search", 2);
        assert!(out[0].is_search);
        assert_eq!(out[0].title, "Search for \"rust lang\"");
        let out = suggestions(&entries, &[], "a.com", "Search", 2);
        assert!(!out.iter().any(|s| s.is_search));
    }

    #[test]
    fn group_by_day_orders_today_yesterday_then_dates() {
        let now = 10 * DAY_MS + 100;
        let entries = vec![
            entry("https://a.com", "", now - 100, 1),
            entry("https://b.com", "", now - DAY_MS - 100, 1),
            entry("https://c.com", "", now - 3 * DAY_MS - 100, 1),
        ];
        let groups = group_by_day(&entries, now);
        assert_eq!(groups[0].0, DayGroup::Today);
        assert_eq!(groups[1].0, DayGroup::Yesterday);
        assert!(matches!(groups[2].0, DayGroup::Date(_)));
    }

    #[test]
    fn clear_entries_respects_scope() {
        // 20 h dans la journée : « now-2h » et « now-10s » sont aujourd'hui.
        let now = 10 * DAY_MS + 20 * 3_600_000;
        let mut entries = vec![
            entry("https://recent.com", "", now - 10_000, 1),
            entry("https://hour.com", "", now - 2 * 3_600_000, 1),
            entry("https://old.com", "", now - 3 * DAY_MS, 1),
        ];
        // « Last hour » n'efface que la visite d'il y a 10 s.
        assert_eq!(clear_entries(&mut entries, ClearScope::Hour, now), 1);
        assert_eq!(entries.len(), 2);
        // « Today » efface les deux visites du jour.
        assert_eq!(clear_entries(&mut entries, ClearScope::Today, now), 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(clear_entries(&mut entries, ClearScope::All, now), 1);
        assert!(entries.is_empty());
    }

    #[test]
    fn jsonl_store_roundtrip_and_favorites() {
        let dir = std::env::temp_dir().join(format!("komet-hist-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut hist = BrowserHistory::load(&dir);
        // Horodatages « maintenant » : la rétention (90 j) élaguerait des
        // dates de 1970 au rechargement.
        let now = chrono::Utc::now().timestamp_millis();
        hist.record("https://a.com", "A", now - 40_000);
        hist.record("https://a.com", "A", now - 1_000);
        hist.record_title("https://a.com", "A new title");
        assert!(!hist.is_favorite("https://a.com"));
        assert!(hist.toggle_favorite("https://a.com", "A", now));
        assert!(hist.is_favorite("https://a.com"));
        hist.flush();

        let hist2 = BrowserHistory::load(&dir);
        assert_eq!(hist2.entries().len(), 1);
        assert_eq!(hist2.entries()[0].title, "A new title");
        assert_eq!(hist2.entries()[0].visit_count, 2);
        assert!(hist2.is_favorite("https://a.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
