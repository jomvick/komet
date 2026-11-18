//! Dialogue « Clear Storage » : modèle de sélection et extraction des
//! origines connues pour `Storage.clearDataForOrigin`.
//!
//! « Global » = tous les sites du profil : il n'existe pas de commande CDP
//! « toutes origines », on énumère donc les origines vues (historique,
//! favoris, page courante) et on les passe une à une.

/// Catégories cochables du dialogue. Le défaut reflète la spec :
/// cookies et cache cochés, données de sites et historique non.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearSelection {
    pub cookies: bool,
    pub cache: bool,
    pub site_data: bool,
    pub history: bool,
}

impl ClearSelection {
    pub const DEFAULT: Self = Self {
        cookies: true,
        cache: true,
        site_data: false,
        history: false,
    };

    /// `true` si au moins une case est cochée (le bouton « Clear »
    /// reste inerte sinon).
    pub fn any(&self) -> bool {
        self.cookies || self.cache || self.site_data || self.history
    }

    /// `true` si du travail CDP est requis (l'historique seul vit dans
    /// les fichiers JSONL, pas dans Chrome).
    pub fn needs_cdp(&self) -> bool {
        self.cookies || self.cache || self.site_data
    }
}

/// `scheme://autorité` d'une URL http(s) — l'« origin » attendu par
/// `Storage.clearDataForOrigin` (port conservé, chemin/query/fragment
/// ignorés). `None` pour `about:`/`data:`/fichier ou hôte vide.
pub fn origin_of(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Nombre maximal d'origines envoyées à `Storage.clearDataForOrigin` —
/// au-delà, la liste triée est tronquée (le nettoyage est best-effort,
/// l'essentiel des sites est couvert par les premières entrées).
pub const MAX_ORIGINS: usize = 200;

/// Origines dédupliquées et triées de toutes les URLs connues (historique,
/// favoris, page courante), tronquées à [`MAX_ORIGINS`].
pub fn known_origins<'a>(urls: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut origins: Vec<String> = urls
        .into_iter()
        .filter_map(origin_of)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    origins.truncate(MAX_ORIGINS);
    origins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_of_extrait_scheme_hote_et_port() {
        assert_eq!(
            origin_of("https://example.com/path?q=1#frag"),
            Some("https://example.com".to_string())
        );
        assert_eq!(
            origin_of("http://localhost:3000/x"),
            Some("http://localhost:3000".to_string())
        );
        assert_eq!(
            origin_of("https://[::1]:8443/a"),
            Some("https://[::1]:8443".to_string())
        );
    }

    #[test]
    fn origin_of_rejette_les_non_http() {
        assert_eq!(origin_of("about:blank"), None);
        assert_eq!(origin_of("data:text/html,x"), None);
        assert_eq!(origin_of("file:///etc/passwd"), None);
        assert_eq!(origin_of("chrome://settings"), None);
        assert_eq!(origin_of(""), None);
        assert_eq!(origin_of("https://"), None);
        assert_eq!(origin_of("pas une url"), None);
    }

    #[test]
    fn known_origins_dedoublonne_et_trie() {
        let origins = known_origins([
            "https://b.com/x",
            "https://a.com/",
            "https://b.com/y",
            "about:blank",
            "http://a.com:8080/z",
        ]);
        assert_eq!(
            origins,
            vec![
                "http://a.com:8080".to_string(),
                "https://a.com".to_string(),
                "https://b.com".to_string(),
            ]
        );
    }

    #[test]
    fn selection_defaut_et_cas_particuliers() {
        let sel = ClearSelection::DEFAULT;
        assert!(sel.cookies && sel.cache);
        assert!(!sel.site_data && !sel.history);
        assert!(sel.any() && sel.needs_cdp());
        // Historique seul : pas de CDP.
        let history_only = ClearSelection {
            cookies: false,
            cache: false,
            site_data: false,
            history: true,
        };
        assert!(history_only.any() && !history_only.needs_cdp());
        // Tout décoché : rien à faire.
        let none = ClearSelection {
            cookies: false,
            cache: false,
            site_data: false,
            history: false,
        };
        assert!(!none.any() && !none.needs_cdp());
    }
}
