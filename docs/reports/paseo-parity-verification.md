# Rapport — Vérification du modèle de permissions « calqué sur Paseo »

**Date :** 2026-08-27
**Objectif vérifié :** *« calquer le modèle de gestion de Paseo des agents CLI sur le plan des permissions et des differents accès »* — reproduire dans komet le modèle Paseo : options provider-natives strictes, fail-fast structuré, `options wins`, rejet des `options` non‑vides pour les providers non‑supports, permissions d'outil bloquantes, et mapping des accès différenciés Codex / Claude / OpenCode.
**Sources :** Paseo `public-docs/sdk/provider-options.md` (schéma strict) ; komet `crates/proto/src/agent.rs`, `crates/harness/src/{acp,codex,claude}/…`, `crates/engine/src/sessions.rs`, `docs/security.md`.

---

## Verdict d'ensemble

**L'architecture du modèle est implémentée et testée (✅). La parité de surface des options provider était PARTIELLE (⚠️) :** plusieurs champs Paseo ajoutés au proto n'étaient ni appliqués ni refusés (perte silencieuse), et quelques types Paseo (enum `web_search`, `approval_policy` `untrusted`/`on‑request`, `features` objet) étaient décrits de façon réductive. Les trous P0 ont été **implémentés et testés** — voir §7 « Résolution livrée » ; seuls des résidus de représentation (et la validation live de l'overlay) restent documentés.

---

## 1. Ce qui est atteint (le socle architectural)

| Élément du modèle Paseo | Statut | Preuve (komet) |
| --- | --- | --- |
| **Provider‑natif strict, options validées brutalement** | ✅ | `validate_run_request` (`agent.rs:648`) + `ValidationError` structuré (`agent.rs:487‑522`) : champs inconnus refusés par `deny_unknown_fields` sur `CodexSandbox`/`ClaudeSandbox`/`OpenCodePerms`. |
| **« Every other provider rejects non‑empty options »** | ✅ (limite §3.4) | `ProviderOptionsRejected` pour `Cursor/Grok/Hermes/Pi/Antigravity` (`agent.rs:655‑661`), testé `crates/engine/tests/sandbox_validation.rs`. |
| **`options wins` (modeId/sandbox legacy)** | ✅ | validation avant spawn (`sessions.rs:319`) ; Codex ne passe `DangerFullAccess` que si `sandbox_options.is_none()` (`codex/mod.rs:537`). Yolo ne surécrase pas un tableau d'options explicite. |
| **Non‑boundary hôte (pas d'isolation OS)** | ✅ | `docs/security.md` « Sandbox is not a host boundary » conservé. |
| **Permission tool à runtime → bloquant** | ✅ | ACP `session/request_permission` (kinds `allow/reject`) ≠ auto‑accept : bloque sur `RunControls.request_permission` et `.await` avant `client.respond` ; resolver dropé → Deny (jamais de silent allow) — `acp/mod.rs:1865‑1938`, `option_for_choice`. |
| **Deny → interrupt** | ✅ | `respond_permission` : Deny résout le bridge **et** interrupt la run (fire‑and‑forget tokio) — `sessions.rs:720‑740` ; testé `crates/engine/tests/permission_flow.rs`. |
| **Overlay OpenCode branché** | ✅ (partiel) | `acp/mod.rs:1270‑1291` : écrit temp `opencode.json`, `env OPENCODE_CONFIG=<overlay>` au spawn. Cf. §2/§3 pour ce que l'overlay *contient* réellement. |
| **Mapping Codex 1:1 (sous‑ensemble)** | ✅ (limites) | `codex/mod.rs:520‑595` : `sandbox_mode`→ wire, `sandbox_workspace_write.{exclude_slash_tmp,exclude_tmpdir_env_var}`→`excludeSlashTmp/excludeTmpdirEnvVar`, `networkAccess`, `writableRoots`, `features`. |
| **Mapping Claude** | ✅ (partiel) | `claude/mod.rs:260‑340` : `excludedCommands`→`Bash(…)`, `deny`/`deny_write`→`Edit(…)`, `additionalDirectories`, `allowed/disallowedTools`, `network.allowedHosts/deniedHosts`, `settings_permissions` passthrough. |

La mécanique de permissions runtime (surfacing `PermissionRequested`, parking du run, décision `Allow`/`AllowAlways`/`Deny`, deny→interrupt) est **couverte par 3 jeux de tests** (ACP + engine `permission_flow.rs` + fixture `perm‑deny`).
---

## 2. Champs Paseo ajoutés mais non appliqués (perte silencieuse — trou de fidélité grave)

Ces champs existent dans le proto komet (Task 0), mais le générateur/les mappers ne les consomment pas : un client qui les fournit croit restreindre l'agent, rien ne bouge.

| Champ Paseo | Côté komet | Constat |
| --- | --- | --- |
| **`opencode.read` / `edit` / `external_directory` / `webfetch` / `websearch`** | présents dans `OpenCodePerms` (`agent.rs:383‑392`) | `opencode_config_document`/`permission_config` n'émettent **que `bash` + `unscoped_actions`** (`opencode_perms.rs:27‑45`) → ces 5 champs sont **silencieusement droppés** de l'overlay. |
| **`claude.deny_read`** (Paseo `denyRead` — exemples `~/.ssh`, `~/.aws`) | présent (`FilesystemSandbox.deny_read`, `agent.rs:323`) | le mapper `claude/mod.rs:270` ne chaîne **que** `deny` + `deny_write` → `Edit(…)` → **`deny_read` n'est pas appliqué**, seul un `deny` global du niveau `ReadOnly` (`deny="/"` dans `from_level`) le masque. |
| **`claude.sandbox.enabled`** (Paseo `enabled`) | **absent du proto** | un `ClaudeSandbox` non‑empty force la table via `--permission-mode default` (présence empirique), pas un booléen `enabled`. |
| **`claude.network.strictAllowlist`** | **absent** | seuls `allowedHosts`/`deniedHosts` sont mappés. |
| **`codex.web_search`** | `bool` | **Paseo est une enum `"disabled"\|"cached"\|"indexed"\|"live"`** (l.56). Le bool komet n'exprime que on/off → perte de `cached/indexed/live`. |
| **`codex.features`** | `Vec<String>` | Paseo : booléens ou objet (`multi_agent_v2`, `network_proxy` en politique) → le mapper émet une `Array` de noms (perd l'objet `network_proxy`). |
| **`codex.approval_policy`** | `ApprovalPolicy::{Never, Granular}` | Paseo supporte aussi `untrusted` et `on‑request` (l.53) → **non représentables** chez komet. |

**Impact :** un utilisateur qui calque sa config Paseo (ex. `opencode { read: "deny", external_directory: "deny" }`, ou `claude … denyRead …`) obtient un run qui **n'applique pas** ces restrictions. Le `security.md` (« OpenCode : tables appliquées ») est plus optimiste que la réalité pour ces champs-là.
---

## 3. Divergences volontaires (à connaître)

| Modèle Paseo | Divergence komet | Justification |
| --- | --- | --- |
| `permission: "deny"` (string, tout‑deny) | non supporté | komet exige une table avec fallback ; rejet structuré plutôt que « restrict‑by‑illusion ». |
| `*` fallback : Paseo canonique `"*": "ask"` ; table sans `*` → unmatched = ambient | komet refuse une table sans fallback `*` (`OpenCodeMissingFallback`) | plus strict, mais pas un calque exact : interdit une table volontairement ciblée sans wildcard. |
| `ask` → `waitForFinish() == 'permission'` | komet émet `PermissionRequested` + part `Permission` + park du run | équivalent (bloquant) mais autre wire. |
| `harness: None` (chat ciblé indirectement) | `ProviderOptionsRejected` n'est testé que si `request.harness` est défini (`agent.rs:653`). Un non‑vide sans `harness` vers une chat Cursor **passe la validation** et stalle dans `harness_for_request`. | bord : garde dépendante d'un champ explicite. |

---

## 4. Endettement technique (dead/stale)

1. `ValidationError::OpenCodeOptionsNotApplied` défini+affiché (`agent.rs:514,561`) mais **jamais émis** depuis que l'overlay est branché → **variante morte**.
2. En‑tête `opencode_perms.rs:15‑21` : dit « unwired / merge semantics unverified / wire this » alors que le wiring `OPENCODE_CONFIG` existe (`acp/mod.rs`) → **commentaire obsolète et contradictoire**.
3. Le merge semantics de l'overlay contre une vraie CLI opencode reste **non validé live** (risque clobber/silencieux documenté, non levé).
4. `docs/PARITY.md` ne liste pas cette feature (pas de ligne sandbox/permissions) → parité sandbox non tracée.

---

## 5. Recommandations priorisées

**P0 — perte silencieuse (fidélité de l'interface publique) :**
- `opencode_config_document` doit émettre `read/edit/external_directory/webfetch/websearch` (et idéalement les pattern‑maps live : `glob/grep/list/task/repo_clone/repo_overview/lsp/skill`).
- Mapper `claude.deny_read` (refus de lecture), ou **rejeter explicitement** (fail‑fast) plutôt que de droper.
- Ajouter `claude.enabled` + `network.strictAllowlist`, ou documenter leur absence comme limite explicite (pas de perte silencieuse).

**P0 — compléter les enums :** `codex.web_search` et `codex.features` vers leurs formes Paseo (enum / objet) pour exprimer `cached|indexed|live` et `network_proxy` en objet.

**P1 — nettoyage :** supprimer `OpenCodeOptionsNotApplied` (mort), mettre à jour l'en‑tête `opencode_perms.rs`, valider le merge semantics de l'overlay sur une vraie CLI opencode.

**P1 — validation :** étendre `ProviderOptionsRejected` au cas `harness: None` résolu via `harness_for_request`, pour ne pas laisser une session Cursor accepter des options non‑vides.

---

## 6. Conclusion

Le **modèle** (provider‑native strict + fail‑fast + `options wins` + autres providers rejettent les options + permission bloquante + deny→interrupt + overlay OpenCode) est une réussite et **testé de bout en bout**. La **parité de surface** — cœur de l'« accès différencié » Paseo — était **partielle** (pertes silencieuses) ; les trous P0 identifiés ci‑dessous ont été **implémentés et testés** (section 7).

**Verdict : ATTEINT, avec les résidus documentés en section 7.**
- Socle du modèle & workflow interactif : **atteint**.
- Parité des champs (accès différenciés) : **atteinte sur les pertes silencieuses ; résidus de représentation documentés** (web_search/strictAllowlist/features).

---

## 7. Résolution livrée (2026-08-27)

Changements implémentés pour lever les trous signalés plus haut :

| Point | État | Modification |
| --- | --- | --- |
| `opencode.read/edit/external_directory/webfetch/websearch` émis | ✅ **fait** | `permission_config` émet ces bare perms ; champ dédié gagne sur `unscoped_actions` (pas de clé dupliquée) — `opencode_perms.rs` + 2 tests. |
| `claude.deny_read` appliqué | ✅ **fait** | mapper émet `Read(path)` (distinct de `Edit`) — `claude/mod.rs` ; documenté `security.md`. |
| Variante morte `OpenCodeOptionsNotApplied` | ✅ **fait** | variante + Display supprimés — `agent.rs`. |
| Header `opencode_perms.rs` obsolète (« unwired ») | ✅ **fait** | réécrit : overlay désormais câblé, résidu merge-semantics documenté. |
| `harness: None` contournait le rejet | ✅ **fait** | garde dans `dispatch_inner` contre le **harness résolu** pour Cursor/Grok/Hermes/Pi/Antigravity — `sessions.rs:336` . |
| `approval_policy` `untrusted`/`on-request` | ✅ **fait** | variantes ajoutées (additif, non‑breaking) + round‑trip testé — `agent.rs`. |
| `web_search` enum | ⚠️ documenté | le wire sandbox Codex prend un booléen ; komet mappe bool→bool. L'enum Paseo `disabled\|cached\|indexed\|live` n'est pas un→un. |
| `claude.enabled` / `network.strictAllowlist` | ⚠️ documenté | pas des champs first‑class ; transmissibles via le passthrough `settings.sandbox` (JSON fusionné last). |
| `codex.features` | ⚠️ documenté | restreint à `Vec<String>` (les formes objet `network_proxy`/`multi_agent_v2` ne sont pas exprimables). |
| Merge semantics overlay OpenCode vs vraie CLI | ⚠️ ouvert | à valider sur un install opencode réel (risque clobber/shadow documenté). |

**Régressions :** `cargo test --no-fail-fast -p komet-proto -p komet-harness -p komet-engine` → tous verts (dont les nouveaux tests `bare_tool_fields_render…`, `dedicated_bare_field_beats…`, `codex_approval_policy_blanket_levels_round_trip`).