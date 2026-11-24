//! Journal réseau du navigateur : requêtes/réponses/erreurs vues par CDP.
//!
//! Réglé léger : les identifiants CDP (`Network.requestId`) sont mappés sur
//! l'index de l'entrée, les entrées finies restent ordonnées d'arrivée et la
//! liste est plafonnée (les plus anciennes tombent). Aucune donnée de corps
//! n'est retenue — URL, méthode, statut et texte d'erreur seulement.

use std::collections::HashMap;
use std::collections::VecDeque;

/// Nombre max d'entrées conservées à l'écran.
const CAP: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    /// `Network.requestId` CDP.
    pub id: String,
    /// URL complète de la requête.
    pub url: String,
    /// GET/POST/…
    pub method: String,
    /// Statut HTTP de la réponse ; `None` tant que rien n'est répondu.
    pub status: Option<u16>,
    /// `errorText` de `Network.loadingFailed` (connexion refusée, DNS,
    /// certificat…) ; `None` = la requête a abouti ou est en cours.
    pub error: Option<String>,
    /// Annulation côté page (`loadingFailed.canceled`) : typiquement
    /// `abort()` d'un fetch ou annulation navigateur — pas une erreur.
    pub canceled: bool,
}

impl NetworkEntry {
    /// Vrai si la requête a échoué : erreur CDP hors annulation (les pages
    /// annulent normalement les fetchs abortés) ou statut >= 400.
    pub fn failed(&self) -> bool {
        !self.canceled && (self.error.is_some() || self.status.is_some_and(|s| s >= 400))
    }

    /// Libellé de statut : code HTTP, « ERR_* » tronqué, ou « — ».
    pub fn status_label(&self) -> String {
        if let Some(status) = self.status {
            return status.to_string();
        }
        if let Some(error) = &self.error {
            let mut short = error.clone();
            if let Some(idx) = short.find("ERR_") {
                short = short[idx..].to_owned();
            }
            short.truncate(28);
            return short;
        }
        if self.canceled {
            return "canceled".into();
        }
        "—".into()
    }

    /// Erreur de certificat (`ERR_CERT_*` / SSL) : signalée distinctement.
    pub fn cert_error(&self) -> bool {
        self.error
            .as_deref()
            .is_some_and(|e| e.contains("ERR_CERT") || e.contains("ERR_SSL") || e.contains("SSL"))
    }
}

/// Journal conservé côté surface. Insertion O(1) amortie ; le mapping
/// `requestId → index` est reconstruit au besoin (les index bougent quand
/// d'anciennes entrées tombent du plafond).
#[derive(Debug, Default)]
pub struct NetworkLog {
    entries: VecDeque<NetworkEntry>,
    by_id: HashMap<String, usize>,
}

impl NetworkLog {
    /// `Network.requestWillBeSent` : nouvelle entrée en cours.
    pub fn started(&mut self, id: &str, url: &str, method: &str) {
        let index = self.entries.len();
        self.entries.push_back(NetworkEntry {
            id: id.to_owned(),
            url: url.to_owned(),
            method: method.to_owned(),
            status: None,
            error: None,
            canceled: false,
        });
        self.by_id.insert(id.to_owned(), index);
        self.prune();
    }

    /// `Network.responseReceived` : statut connu. Ignore les id inconnus
    /// (redirections internes, requêtes démarrées avant l'activation).
    pub fn responded(&mut self, id: &str, status: u16) {
        if let Some(&index) = self.by_id.get(id) {
            self.entries[index].status = Some(status);
        }
    }

    /// `Network.loadingFailed` : texte d'erreur (et annulation éventuelle).
    pub fn failed(&mut self, id: &str, error_text: &str, canceled: bool) {
        if let Some(&index) = self.by_id.get(id) {
            self.entries[index].error = Some(error_text.to_owned());
            self.entries[index].canceled = canceled;
        }
    }

    pub fn entries(&self) -> &VecDeque<NetworkEntry> {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Vide le journal (bouton Clear) et le mapping.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.by_id.clear();
    }

    /// Plafond : les plus anciennes tombent, le mapping est recalculé.
    fn prune(&mut self) {
        while self.entries.len() > CAP {
            self.entries.pop_front();
        }
        self.by_id = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.clone(), index))
            .collect();
    }
}

/// Vrai pour `localhost`, `*.localhost`, `127.*`, `[::1]` et les IP privées
/// (RFC 1918 + 169.254) : la distinction http/https/erreur s'affiche selon
/// l'hôte, pas seulement le schéma.
pub fn is_local_host(url: &str) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let rest = rest.strip_prefix("https://").unwrap_or(rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host.ends_with(".localhost")
        || host.starts_with("127.")
        || host == "::1"
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host
            .strip_prefix("172.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|n| n.parse::<u16>().ok())
            .is_some_and(|n| (16..=31).contains(&n))
        || host.starts_with("169.254.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tracks_request_through_response_and_error() {
        let mut log = NetworkLog::default();
        log.started("r1", "http://localhost:5173/app", "GET");
        log.responded("r1", 200);
        let entries: Vec<_> = log.entries().iter().cloned().collect();
        assert_eq!(entries[0].status, Some(200));
        assert!(!entries[0].failed());
        assert_eq!(entries[0].status_label(), "200");

        log.started("r2", "http://localhost:3000/api", "POST");
        log.failed("r2", "net::ERR_CONNECTION_REFUSED", false);
        let entries: Vec<_> = log.entries().iter().cloned().collect();
        assert!(entries[1].failed());
        assert_eq!(entries[1].status_label(), "ERR_CONNECTION_REFUSED");
    }

    #[test]
    fn log_is_capped_and_ids_stay_resolvable() {
        let mut log = NetworkLog::default();
        for i in 0..(CAP as u32 + 50) {
            log.started(&format!("r{i}"), &format!("http://h/{i}"), "GET");
        }
        assert_eq!(log.entries().len(), CAP);
        // une réponse tardive sur une entrée encore présente marche
        log.responded("r349", 204);
        let last = log.entries().back().unwrap();
        assert_eq!(last.id, "r349");
        assert_eq!(last.status, Some(204));
        // tombée du plafond : réponse ignorée sans panique
        log.responded("r0", 200);
    }

    #[test]
    fn canceled_is_not_an_error() {
        let mut log = NetworkLog::default();
        log.started("r1", "http://h/x", "GET");
        log.failed("r1", "net::ERR_ABORTED", true);
        let entry = log.entries().back().unwrap();
        assert!(entry.canceled);
        assert!(!entry.failed());
        assert_eq!(entry.status_label(), "ERR_ABORTED");
    }

    #[test]
    fn cert_errors_are_flagged() {
        let mut log = NetworkLog::default();
        log.started("r1", "https://self-signed.badssl.com/", "GET");
        log.failed("r1", "net::ERR_CERT_AUTHORITY_INVALID", false);
        assert!(log.entries().back().unwrap().cert_error());
    }

    #[test]
    fn local_hosts_are_classified() {
        assert!(is_local_host("http://localhost:5173/app"));
        assert!(is_local_host("http://127.0.0.1:8080"));
        assert!(is_local_host("http://[::1]:9000"));
        assert!(is_local_host("http://192.168.1.4/dev"));
        assert!(is_local_host("http://10.0.0.2"));
        assert!(is_local_host("http://172.16.5.5"));
        assert!(is_local_host("https://sub.localhost"));
        assert!(!is_local_host("http://localhost.evil.com"));
        assert!(!is_local_host("http://172.32.0.1"));
        assert!(!is_local_host("http://8.8.8.8"));
        assert!(!is_local_host("https://example.com"));
    }
}
