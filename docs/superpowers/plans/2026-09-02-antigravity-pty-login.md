# Antigravity PTY Login + Keyring Swap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Donner à Antigravity (`agy`) le même niveau de fonctionnalité multi-compte que Codex — Add account (login complet depuis Komet) + Switch instantané entre comptes sauvegardés — malgré l'absence d'un sous-verbe `agy login` headless.

**Contexte (voir `docs/superpowers/plans/2026-08-31-antigravity-agent-accounts.md`) :** l'implémentation précédente supposait à tort que `agy login` existait et se comportait comme `codex login`. `agy --help` confirme qu'aucune sous-commande `login`/`auth` n'existe : les sous-commandes disponibles sont `agent(s)`, `changelog`, `help`, `install`, `mcp`, `mic-serve`, `models`, `plugin(s)`, `update`. L'authentification n'est qu'un écran du TUI bubbletea affiché au tout premier lancement interactif d'`agy` (menu "Select login method: 1. Google OAuth 2. Use a Google Cloud project"), et le token final est stocké dans le **keyring système** (Secret Service / libsecret sur Linux), jamais dans un fichier JSON simple.

**Architecture cible :**
- **Detect/activate** : lire/écrire le keyring Linux (crate `secret-service` ou `secret-tool` en subprocess) au lieu de fichiers `antigravity_home/*.json`.
- **Add account** : piloter `agy` derrière un vrai PTY (crate `portable-pty`), envoyer la sélection de menu + le code d'autorisation comme le ferait un humain au clavier, puis tuer le process et relire le keyring.
- **UI** : Antigravity bascule sur le pattern **PasteCode** (comme Claude), pas Browser/poll (comme Codex), car le code doit être renvoyé manuellement dans le PTY, pas via un callback loopback.

**Tech Stack:** Rust, `portable-pty` (déjà dans le workspace — voir `crates/engine/src/terminals.rs` pour le pattern de référence), `secret-service = { version = "5", features = ["rt-tokio-crypto-rust"] }` (pure Rust, pas de `libdbus-dev` requis, compatible tokio — à ajouter), `serde_json`

## Global Constraints
- Ne jamais casser les flows Claude/Codex existants (fichiers, pas de PTY).
- Le module Linux/libsecret doit être `#[cfg(target_os = "linux")]` et échouer proprement (pas de panic) si aucun daemon Secret Service ne tourne (serveur headless sans session graphique).
- Le process PTY doit être tuable à tout moment (Cancel) sans laisser de zombie ni de secret à moitié écrit.
- Toute donnée sensible transitant en mémoire (code d'autorisation, JSON de credentials lu du keyring) suit les mêmes règles d'hygiène que les slots existants (jamais loggée en clair).

---

### Task 0: Discovery — Localiser le vrai stockage keyring d'Antigravity ✅ RÉSOLUE (2026-09-02, sans login manuel)

**Résultats de la discovery (faite par introspection D-Bus + `strings` du binaire `agy` 1.1.23) :**

1. **Item keyring trouvé** (collection `kdewallet` via `ksecretd`/`org.freedesktop.secrets`) :
   - Attributs : `service="gemini"`, `username="antigravity"` (schéma exact de `github.com/zalando/go-keyring`, présent dans les symboles Go du binaire)
   - Label : `Password for 'antigravity' on 'gemini'`
2. **Format du secret** (JSON exact) :
   ```json
   {"token":{"access_token":"ya29.…","token_type":"Bearer","refresh_token":"1//03…","expiry":"2026-09-02T02:02:09.192730153Z"},"auth_method":"consumer"}
   ```
   → `parse_antigravity_auth()` existant doit gérer la forme `token.access_token` / `token.refresh_token` + `auth_method`.
3. **Quirk ksecretd** : `secret-tool lookup service gemini username antigravity` renvoie **vide** alors que `secret-tool search --all service gemini username antigravity` renvoie le secret. Le module Rust (Task 1) doit donc être validé sur cette machine avec `search`, pas seulement `lookup`.
4. **Mode headless auth découvert** (strings du binaire) : `agy` supporte un flow paste-code en print mode —
   `Headless auth: no valid auth (%s); starting login`, `Print mode: not authenticated, trying silent auth`,
   `Print mode: submitting manually-entered auth code`, `Or, paste the authorization code here and press Enter:`.
   → **Task 2 simplifiée** : pas besoin de détecter/piloter le menu bubbletea « Select login method » ; un PTY sur `agy -p` non-authentifié déclenche directement le headless login (ouverture navigateur + invite paste-code sur /dev/tty). La détection du menu reste un fallback si ce chemin disparaît.
5. **File fallback confirmé** : symboles `codeassistclient.fileTokenStorage` + messages `Keyring SaveToken/LoadToken timed out after %v, falling back to file storage`. Aucun fichier token trouvé sur disque actuellement (keyring répond) — le fallback fichier est une sécurité d'`agy`, pas un chemin à imiter.
6. **OAuth client ID** visible : `884354919052-36trc1jjb3tguiac32ov6cod268c5blh.apps.googleusercontent.com` (utile pour reconnaître l'URL d'autorisation).
7. **Environnement** : daemon Secret Service = `ksecretd` (KDE) sur `org.freedesktop.secrets` ; `kwalletd6` tourne aussi en parallèle (backend KWallet sous le service compat).

- [ ] **Step 2: Extraire le secret trouvé et documenter son format**

```bash
secret-tool lookup service <service-trouve> account <compte-trouve>
```

Noter le JSON exact (clés : access_token/refresh_token/id_token/email ?) — `parse_antigravity_auth()` existe déjà et gère plusieurs formes (JWT, email brut, access_token brut) ; vérifier qu'une des branches existantes matche, sinon l'étendre.

- [ ] **Step 3: Vérifier qu'aucun fichier n'apparaît en parallèle**

```bash
find ~/.gemini ~/.config ~/.local/share -newer /tmp/before.txt -type f 2>/dev/null
```

Si un fichier apparaît malgré tout (cache, session, etc.), noter son chemin — il pourrait servir de signal "login terminé" plus simple à poller que le keyring lui-même.

- [ ] **Step 4: Documenter dans le header du module**

```rust
//! - **Antigravity** — Google OAuth via `agy`. NO headless login subcommand
//!   exists (`agy --help` lists no login/auth verb) — auth only happens via
//!   the interactive bubbletea TUI on first run. Credentials are stored in
//!   the OS keyring (Secret Service/libsecret on Linux, service=`<TROUVE>`),
//!   never in a plain file. Komet drives `agy` behind a PTY to automate the
//!   menu selection + code paste, then reads the keyring directly.
```

- [ ] **Step 5: Commit découverte**

```bash
git add crates/engine/src/agent_accounts.rs
git commit -m "docs: document antigravity keyring storage (no login subcommand)"
```

---

**Service keyring (résolu Task 0) : `service="gemini"`, `username="antigravity"`, secret = JSON `{"token":{access_token,token_type,refresh_token,expiry},"auth_method":"consumer"}`.**

### Task 1: Module keyring Linux (Secret Service / libsecret)

**Files:**
- Create: `crates/engine/src/secret_service_linux.rs` (ou module inline `#[cfg(target_os = "linux")]` dans `agent_accounts.rs`, à la façon du module `keychain` macOS existant)
- Modify: `crates/engine/Cargo.toml` — ajouter `secret-service = { version = "5", features = ["rt-tokio-crypto-rust"] }` (confirmé pure-Rust, pas de dépendance système `libdbus-1-dev`, runtime tokio — cohérent avec le reste du crate)
- Test: `crates/engine/tests/m5c_accounts_uploads_titles.rs` (nouveau test avec mock)

**Interfaces:**
- Consumes: nom de service découvert en Task 0
- Produces: `read_credentials(service: &str) -> Option<serde_json::Value>`, `write_credentials(service: &str, json: &str) -> Result<(), EngineError>`, calqués sur la forme des modules `keychain`/`wincred` existants (même signature `(Option<Value>, Option<String>)` pour les warnings côté read)

- [ ] **Step 1: Write failing test (avec un mock/skip si pas de daemon en CI)**

```rust
#[cfg(target_os = "linux")]
#[tokio::test]
async fn secretservice_roundtrip_or_skip() {
    // Si aucun daemon Secret Service (CI headless), le test doit SKIP
    // proprement, pas paniquer — assert sur une erreur typée "unavailable",
    // pas sur un round-trip réel.
}
```

- [ ] **Step 2: Implémenter le module, calqué sur la structure `keychain` (macOS)**

```rust
#[cfg(target_os = "linux")]
mod secretservice {
    use super::*;

    const EXEC_TIMEOUT: Duration = Duration::from_secs(15);

    pub(super) async fn read_credentials(service: &str) -> (Option<serde_json::Value>, Option<String>) {
        // Option A: crate `secret-service` (async, pas de shell-out)
        // Option B (plus simple, cohérent avec le style `exec()` de keychain):
        //   `secret-tool lookup service <service>` — timeout borné comme macOS,
        //   car un daemon qui demande une confirmation utilisateur peut bloquer indéfiniment
    }

    pub(super) async fn write_credentials(service: &str, account: &str, json: &str) -> Result<(), EngineError> {
        // `secret-tool store --label="Antigravity" service <service> account <account>`
        // (secret-tool store lit le secret depuis stdin)
    }

    pub(super) async fn clear_credentials(service: &str) -> Result<(), EngineError> {
        // `secret-tool clear service <service>` — utile pour un `forget()` propre plus tard
    }
}
```

- [ ] **Step 3: Gérer l'absence de daemon proprement**

Un serveur sans session graphique (SSH pur, pas de `gnome-keyring-daemon`/`kwalletd`) doit renvoyer `(None, None)` — pas de crash, pas de warning bruyant si Antigravity n'est simplement pas utilisé sur cette machine.

- [ ] **Step 4: Commit**

```bash
cargo test -p komet-engine secretservice
git add crates/engine/src/agent_accounts.rs Cargo.toml
git commit -m "feat: linux secret service module for antigravity keyring"
```

---

### Task 2: PTY driver — sélection du menu de login

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs` (remplacer `start_antigravity_login()`)
- Modify: `Cargo.toml` (ajouter `portable-pty`)

**Interfaces:**
- Consumes: format exact du menu TUI (texte + touches attendues) — à capturer manuellement en Task 0 bis si besoin (lancer `agy` dans un terminal, noter le texte pixel-perfect et si la sélection se fait par chiffre+Enter ou flèches+Enter)
> **Task 2 SIMPLIFIÉE post-Task 0** : `agy` a un headless auth en print mode (`Headless auth: starting login` + `Or, paste the authorization code here and press Enter:` sur /dev/tty). Path principal = PTY sur `agy -p` non-authentifié, **pas** de menu bubbletea à piloter. La détection du menu « Select login method » ci-dessous reste un fallback.

- Produces: `LoginFlow::Antigravity { pty_master, writer, output, started_at, exit }`, variant dédiée (PAS de réutilisation de `LoginFlow::Codex`)

- [ ] **Step 1: Ajouter la variante dédiée à l'enum**

```rust
enum LoginFlow {
    Claude { .. },
    Codex { .. },
    Antigravity {
        writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
        output: Arc<Mutex<String>>,
        started_at: Instant,
        exit: Arc<Mutex<Option<()>>>,
        child: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>,
    },
}
```

- [ ] **Step 2: Spawn `agy` (bare, sans argument) derrière un PTY**

Suivre EXACTEMENT le pattern déjà en place dans `crates/engine/src/terminals.rs::open_with_shell` (native_pty_system → openpty → CommandBuilder → spawn_command sur le slave → drop(slave) → try_clone_reader + take_writer + clone_killer → thread bloquant dédié pour la lecture → spawn_blocking pour le wait) plutôt que réinventer un pattern PTY différent dans ce fichier :

```rust
async fn start_antigravity_login(&self) -> Result<AgentLoginStart, EngineError> {
    let pty = portable_pty::native_pty_system();
    let pair = pty
        .openpty(portable_pty::PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| EngineError::Other(format!("could not open a pty: {e}")))?;
    let cmd = portable_pty::CommandBuilder::new("agy");
    let mut child = pair.slave.spawn_command(cmd).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::Other("The `agy` CLI was not found on this device — install it first.".into())
        } else {
            EngineError::Other(format!("Could not start agy: {e}"))
        }
    })?;
    drop(pair.slave); // comme terminals.rs: le master garde la connexion vivante
    let killer = child.clone_killer();
    let reader = pair.master.try_clone_reader()
        .map_err(|e| EngineError::Other(format!("pty reader: {e}")))?;
    let writer = pair.master.take_writer()
        .map_err(|e| EngineError::Other(format!("pty writer: {e}")))?;

    let output = Arc::new(Mutex::new(String::new()));
    // Thread bloquant dédié (identique à `read_pty` dans terminals.rs), PAS
    // tokio::io::AsyncRead comme pour Codex — un PTY se lit en bloquant.
    let sink = output.clone();
    std::thread::Builder::new()
        .name("antigravity-login-pty-read".into())
        .spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap_or_else(|e| e.into_inner())
                        .push_str(&String::from_utf8_lossy(&buf[..n])),
                }
            }
        })
        .map_err(|e| EngineError::Other(format!("pty reader thread: {e}")))?;
    // wait() est bloquant — spawn_blocking, comme terminals.rs
    let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
    // ... monitorer via spawn_blocking(move || child.wait()) puis stocker le code

    // Stocker { writer, output, killer, exit } dans LoginFlow::Antigravity,
    // puis boucler jusqu'à détecter le menu ou l'URL dans `output` (Step 3/4).
}
```

- [ ] **Step 3: Détecter le menu et envoyer la sélection**

```rust
// Attendre "Select login method" dans output, puis écrire "1\n" (ou la séquence
// clavier exacte trouvée en discovery) dans `writer`.
```

- [ ] **Step 4: Scanner l'URL Google dans la sortie post-sélection**

Réutiliser `scan_google_url()` (déjà écrit) sur le buffer `output` mis à jour par le PTY.

- [ ] **Step 5: Timeout de sécurité**

Si aucun menu ni URL n'apparaît en ~5s (comme le pattern Codex), tuer le process et renvoyer une erreur claire plutôt que de laisser un PTY orphelin.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/agent_accounts.rs Cargo.toml
git commit -m "feat: pty-driven antigravity login menu selection"
```

---

### Task 3: Soumission du code — écriture PTY + détection de succès

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs` (`complete_login()` ou un nouveau `complete_antigravity_login()`)
- Modify: `crates/ui/src/settings/accounts.rs` (router Antigravity vers `LoginFlow::PasteCode` au lieu de `Browser`)

**Interfaces:**
- Consumes: `login_id`, `code` (saisi par l'utilisateur dans le dialogue existant)
- Produces: écrit dans le PTY, poll jusqu'à succès/erreur

- [ ] **Step 1: Écrire le code dans le PTY**

```rust
pub async fn complete_antigravity_login(&self, login_id: &str, code: &str) -> Result<AgentAccountsSnapshot, EngineError> {
    let writer = /* récupérer depuis le flow */;
    lock(&writer).write_all(format!("{}\n", code.trim()).as_bytes())?;
    // ensuite : attendre un signal de succès dans `output` (ex: l'email du
    // compte qui s'affiche, ou "Welcome") avec un timeout raisonnable (~10s)
}
```

- [ ] **Step 2: Détecter succès vs échec dans la sortie**

Chercher un motif positif (email affiché, prompt de sélection de thème qui suit) vs un message d'erreur bubbletea explicite. À affiner après observation réelle en Task 0.

- [ ] **Step 3: Tuer le process dès le succès détecté**

Ne pas laisser l'utilisateur traverser les écrans suivants (choix de thème, "trust this folder") — ce sont des étapes de session normale, inutiles pour un simple login. `child.kill()` + cleanup PTY dès que le compte est confirmé.

- [ ] **Step 4: Lire le keyring fraîchement écrit (Task 1) et snapshotter**

```rust
let creds = secretservice::read_credentials(SERVICE_NAME).await;
if let Some((Some(raw), _)) = creds {
    if let Some(detected) = parse_antigravity_auth(raw) {
        self.snapshot_detected(HarnessId::Antigravity, &detected)?;
    }
}
```

- [ ] **Step 5: Router l'UI vers submit_code au lieu du poll browser**

Dans `crates/ui/src/settings/accounts.rs`, `start_login()` : pour `HarnessId::Antigravity`, traiter la réponse `AgentLoginStart` comme `AgentLoginMode::PasteCode` (même si le mode vient du serveur — sinon forcer côté client, à documenter clairement dans un commentaire pourquoi Antigravity diffère de Codex malgré le même chemin `start_login`).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/agent_accounts.rs crates/ui/src/settings/accounts.rs
git commit -m "feat: antigravity code submission via pty, paste-code ui flow"
```

---

### Task 4: Detect + Activate via keyring (remplace les fichiers)

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs:detect_antigravity()`, `activate_antigravity()`

**Interfaces:**
- Consumes: module Task 1
- Produces: `detect_antigravity()` et `activate_antigravity()` basés keyring, plus fallback fichier conservé en dernier recours (au cas où une future version d'`agy` change de mécanisme)

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn antigravity_detect_from_keyring_or_fallback_file() {
    // Mock: si le module secretservice n'est pas exerçable en test (pas de
    // daemon), tester uniquement le fallback fichier existant — ne pas
    // régresser le comportement déjà couvert par le test de la Task 2 du plan
    // précédent (2026-08-31).
}
```

- [ ] **Step 2: Réécrire `detect_antigravity()`**

```rust
fn detect_antigravity(&self) -> Option<Detected> {
    #[cfg(target_os = "linux")]
    {
        if let (Some(raw), _) = block_on_or_spawn(secretservice::read_credentials(SERVICE_NAME)) {
            if let Some(d) = parse_antigravity_auth(raw) {
                return Some(d);
            }
        }
    }
    // Fallback conservé (Task 2 du plan précédent) au cas où une install
    // spécifique écrit malgré tout un fichier.
    for name in ["auth.json", "oauth_token.json", "credentials.json"] {
        let path = self.inner.config.antigravity_home.join(name);
        if let Some(auth) = read_json(&path).and_then(parse_antigravity_auth) {
            return Some(auth);
        }
    }
    None
}
```

Note : `detect_antigravity()` est aujourd'hui synchrone alors que la lecture keyring est probablement async (D-Bus) — envisager de rendre `detect_antigravity` async et propager jusqu'à `list()` (qui est déjà `async fn`), plutôt que bloquer un runtime dans une fonction sync.

- [ ] **Step 3: Réécrire `activate_antigravity()`**

```rust
async fn activate_antigravity(&self, slot: &Slot) -> Result<(), EngineError> {
    #[cfg(target_os = "linux")]
    return secretservice::write_credentials(SERVICE_NAME, &slot.account_key, &slot.credentials.to_string()).await;
    #[cfg(not(target_os = "linux"))]
    {
        // Fallback fichier existant (Task 3 du plan précédent) tant que
        // macOS/Windows n'ont pas leur propre module keyring Antigravity.
    }
}
```

- [ ] **Step 4: Adapter `activate()` (le match plus haut) pour `.await` cette branche**

- [ ] **Step 5: Commit**

```bash
cargo test -p komet-engine antigravity
git add crates/engine/src/agent_accounts.rs
git commit -m "feat: antigravity detect/activate via linux keyring"
```

---

### Task 5: UI — copie dédiée + nettoyage du bug Codex/OpenAI

**Files:**
- Modify: `crates/ui/src/settings/accounts.rs` (`render_login_dialog`, variante `Browser`/nouvelle variante)

**Interfaces:**
- Produces: dialogue "Add Antigravity account" avec un texte propre, plus de mention "OpenAI" pour Antigravity

- [ ] **Step 1: Séparer le texte par harness dans le dialogue**

```rust
let browser_copy = match harness {
    HarnessId::Codex => "Finish signing in to OpenAI in your browser. The new login is captured in an isolated profile — your current session is untouched until you switch.",
    HarnessId::Antigravity => "A terminal-based Google sign-in has started. Approve access in your browser, then paste the code back here — your current session is untouched until you switch.",
    _ => unreachable!(),
};
```

(Si Task 3 route bien Antigravity vers `PasteCode`, ce dialogue `Browser` ne sera même plus utilisé pour Antigravity — dans ce cas, cette étape se réduit à vérifier qu'aucun texte Codex ne fuite ailleurs, ex. dans les logs d'erreur PTY.)

- [ ] **Step 2: Vérifier le message d'erreur TTY existant reste cohérent**

Le message actuel ("Antigravity nécessite un terminal interactif…") devient obsolète une fois le PTY en place — le remplacer par un message d'erreur générique si le PTY lui-même échoue à démarrer (`agy` non installé, etc.), pas un renvoi vers un terminal manuel.

- [ ] **Step 3: Test manuel end-to-end**

```bash
cargo run
# Settings → Accounts → Antigravity → Add account
# Suivre le flow complet : menu auto-sélectionné → navigateur → coller code → compte apparaît
# Ajouter un 2e compte, vérifier Switch bascule bien le keyring
```

- [ ] **Step 4: Commit**

```bash
git add crates/ui/src/settings/accounts.rs
git commit -m "fix: antigravity dialog copy, remove openai leak"
```

---

## Self-Review
- Spec coverage : discovery keyring, module libsecret, PTY login, soumission code, detect/activate keyring, UI dédiée — tous couverts.
- Task 0 résolue le 2026-09-02 sans login manuel (item keyring préexistant) : `service="gemini"`, `username="antigravity"`, JSON `{"token":{...},"auth_method":"consumer"}` ; headless paste-code en print mode confirmé dans le binaire.
- Divergence assumée vs plan du 2026-08-31 : abandon du mirroring `LoginFlow::Codex` pour Antigravity (variante dédiée), abandon du stockage fichier comme source de vérité (gardé en fallback uniquement).
- Risque non résolu à documenter pour la suite : portabilité macOS/Windows du module keyring Antigravity (hors scope de ce plan, Linux uniquement pour l'instant, cohérent avec l'environnement de dev actuel).
- Placeholder scan : aucun TODO silencieux — chaque incertitude (texte exact du menu, motif de succès) est explicitement une étape de discovery à faire avant codage, pas un TODO caché dans le code final.
