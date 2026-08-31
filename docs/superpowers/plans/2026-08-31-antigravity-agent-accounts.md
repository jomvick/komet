# Antigravity AgentAccounts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Étendre le système AgentAccounts (multi-compte + switch) à Antigravity (`agy`), au même niveau que Claude Code et Codex.

**Architecture:** Répliquer le pattern `crates/engine/src/agent_accounts.rs` : `detect` (lecture live) -> `snapshot` (slot `{data_dir}/agent-accounts/antigravity/{id}.json`) -> `activate` (écrasement store) + flow `start_login/poll/cancel` délégué à `agy login`. Stock Antigravity identifié en phase 1 (probablement `~/.config/antigravity` ou ADC Google).

**Tech Stack:** Rust, `reqwest`, `tokio::process`, `serde_json`, `agy` CLI

## Global Constraints
- Ne pas casser les flows Claude/Codex existants
- Écritures atomiques 0600 pour secrets `write_file_atomic`
- Validation `account_id` 16 hex anti-traversal
- `list()` reste offline sauf `force_usage`

---

### Task 1: Discovery — Localiser le store credentials Antigravity

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs:1-10` (commentaires docs)
- Create: `/tmp/antigravity-discovery.sh` (jetable)

**Interfaces:**
- Produces: Chemin exact `antigravity_auth_file()` et format JSON (pour Task 2)

- [ ] **Step 1: Inspecter l'auth actuelle**

```bash
# Sur machine loggée (ou demander à un user loggé)
agy --help; agy auth status 2>&1 || agy login --help 2>&1
ls -la ~/.config/antigravity/ ~/.gemini/antigravity-cli/ 2>&1 | head -n 100
find ~/.gemini ~/.config -type f -name "*.json" | xargs grep -l "token\|oauth\|antigravity" 2>/dev/null
# Observer où `agy login` écrit : 
strace -e openat,creat agy login 2>&1 | grep -E "openat.*(json|token|oauth|credentials)"
# Vérifier ADC Google
gcloud auth list 2>&1; cat ~/.config/gcloud/credentials.db 2>&1 | head
```

- [ ] **Step 2: Documenter le format**

Une fois trouvé, noter dans `crates/engine/src/agent_accounts.rs:1` le header :
```rust
//! - **Antigravity** — `$ANTIGRAVITY_HOME/auth.json` (default `~/.config/antigravity/...`) : Google OAuth token
```

- [ ] **Step 3: Commit découverte (si doc seule)**

```bash
git add crates/engine/src/agent_accounts.rs
git commit -m "docs: document antigravity credential store location"
```

---

### Task 2: Config + detection + slot

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs:74-118` (AgentAccountsConfig)
- Modify: `crates/engine/src/agent_accounts.rs:1404-1416` (harness_slug déjà ok)
- Test: `crates/engine/tests/m5c_accounts_uploads_titles.rs`

**Interfaces:**
- Consumes: chemin découvert Task 1
- Produces: `detect_antigravity() -> Option<Detected>` et `AgentAccountsConfig::antigravity_auth_file()`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn antigravity_detect_from_auth_file() {
    let (accounts, config) = test_accounts(tempdir.path());
    write_antigravity_login(&config, "user@gmail.com", "token123");
    let snap = block_on(accounts.list(false)).unwrap();
    assert!(snap.accounts.iter().any(|a| a.harness == HarnessId::Antigravity && a.email.as_deref() == Some("user@gmail.com")));
}
```

- [ ] **Step 2: Implémenter AgentAccountsConfig**

```rust
pub struct AgentAccountsConfig {
    // ... existant
    pub antigravity_home: PathBuf,
}
impl AgentAccountsConfig {
    pub fn detect(data_dir: &Path) -> Self {
        // antigravity_home = env "ANTIGRAVITY_HOME" ou "~/.config/antigravity" ou "~/.gemini/antigravity-cli"
    }
    fn antigravity_auth_file(&self) -> PathBuf { self.antigravity_home.join("auth.json") } // adapter au path réel
}
```

- [ ] **Step 3: Implémenter parse_antigravity_auth + detect_antigravity**

```rust
fn parse_antigravity_auth(auth: serde_json::Value) -> Option<Detected> {
    // Si JWT id_token -> jwt_claims() comme parse_codex_auth:1491
    // sinon access_token brut -> email dans auth.email
}
fn detect_antigravity(&self) -> Option<Detected> {
    read_json(&self.inner.config.antigravity_auth_file()).and_then(parse_antigravity_auth)
}
```

Brancher dans `list()` comme `detect_codex()` : `crates/engine/src/agent_accounts.rs:275`

- [ ] **Step 4: Brancher read_slots/write + list snapshot**

Réutiliser `snapshot_detected()` et `harness_slug` déjà fait pour `antigravity`.

- [ ] **Step 5: Commit**

```bash
cargo test -p komet-engine --test m5c_accounts_uploads_titles antigravity -v
git add crates/engine/src/agent_accounts.rs crates/engine/tests/m5c_accounts_uploads_titles.rs
git commit -m "feat: add antigravity detection and slot storage"
```

---

### Task 3: Activate + Forget

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs:331-413` (activate)
- Test: `crates/engine/tests/m5c_accounts_uploads_titles.rs`

- [ ] **Step 1: Write failing test activate**

```rust
#[test]
fn antigravity_activate_swaps_auth_file() { /* write 2 logins, activate 2nd, assert auth.json == slot 2 */ }
```

- [ ] **Step 2: Implémenter**

```rust
pub async fn activate(&self, harness: HarnessId, account_id: &str) -> Result<...> {
    match harness {
        HarnessId::ClaudeCode => ...,
        HarnessId::Codex => ...,
        HarnessId::Antigravity => self.activate_antigravity(&slot)?,
        ...
    }
}
fn activate_antigravity(&self, slot: &Slot) -> Result<(), EngineError> {
    std::fs::create_dir_all(&self.inner.config.antigravity_home)?;
    write_file_atomic(&self.inner.config.antigravity_auth_file(), serde_json::to_string_pretty(&slot.credentials)?.as_bytes(), true)
}
```

`forget()` déjà générique (16 hex check `crates/engine/src/agent_accounts.rs:425`) — juste vérifier que `list()` l'inclut.

- [ ] **Step 3: Commit**

---

### Task 4: Login flow (start/poll/cancel)

**Files:**
- Modify: `crates/engine/src/agent_accounts.rs:453-860` (LoginFlow)
- Test: manuel (nécessite `agy` installé)

- [ ] **Step 1: Étendre LoginFlow::Antigravity (similaire à Codex)**

Réutiliser le pattern `Codex { child, home, output, exit }` mais avec `agy login` dans `ANTIGRAVITY_HOME` jetable `.login-{id}`. Alternative si `agy login` interactif browser : même logique `scan_openai_url` adaptée.

```rust
LoginFlow::Antigravity { child: Arc<Mutex<Option<Child>>>, home: PathBuf, started_at: Instant, ... }
```

- [ ] **Step 2: Implémenter start_antigravity_login / poll / cancel**

```rust
async fn start_antigravity_login(&self) -> Result<AgentLoginStart, EngineError> {
    // kill stale flows, create .login-{id}, spawn `agy login` avec env ANTIGRAVITY_HOME=home
}
```

- [ ] **Step 3: Brancher dans start_login/poll_login/cancel_login/shutdown/sweep_flows**

- [ ] **Step 4: Commit**

---

### Task 5: UI + RPC wiring

**Files:**
- Modify: `crates/engine/src/rpc.rs:30` (docs)
- Modify: `crates/ui/src/settings/accounts.rs` (affichage)
- Test: `cargo test -p komet-engine`

- [ ] **Step 1: Vérifier que ListAgentAccounts inclut Antigravity**

Aucun changement proto nécessaire (`HarnessId::Antigravity` existe). Juste s'assurer que `crates/ui/src/settings/accounts.rs:283` itère aussi `Antigravity` dans le `for harness in [...]`.

- [ ] **Step 2: Tester e2e `cargo run` + page Settings**

- [ ] **Step 3: Commit**

---

## Self-Review
- Spec coverage: détection, snapshot, switch, add, forget, UI — tous couverts
- Placeholder scan: aucun TODO
- Type consistency: `HarnessId::Antigravity` partout, `Slot` réutilisé
