# Surface navigateur Komet unifiée (Humain & Agent CLI) — Design

Date : 2026-11-08. Statut : approuvé, partiellement réalisé. Mis à jour le 2026-11-24 :
§2 décrit désormais le code présent (Chromium système + CDP), §6 marque les jalons livrés
et §7 liste les écarts restants. Périmètre : surface Browser intégrée au panneau droit de Komet,
interactive pour l'humain et pilotable sans friction par les agents CLI, architecture unifiée
**CDP-First** inspirée d'Obscura (`h4ckf0r0day/obscura`).

## 1. Intention & Vision

Komet offre un navigateur intégré moderne qui sert à la fois :
1. **À l'utilisateur humain** :
   - Tester et prévisualiser des webapps locales (`localhost:3000`, `127.0.0.1:5173`, etc.) ou naviguer sur le web avec **100% de compatibilité**.
   - Bouton de **Capture d'écran** instantané (presse-papier / fichier / injection chat).
   - Mode **Overlay interactif (inspiration Obscura)** : surbrillance visuelle et UIDs numérotés (`#1`, `#2`) sur les éléments cliquables/saisissables.
   - Console DevTools intégrée (logs `console.*`, erreurs réseau, REPL interactif).
2. **Aux agents CLI (sans friction)** :
   - Pilotage déterministe via CDP / MCP : `navigate`, `click{uid}`, `fill{uid, value}`, `take_snapshot`, `take_screenshot`.
   - Transparence totale : l'utilisateur voit en temps réel les actions de l'agent dans le panneau.
   - Reprise en main manuelle possible à tout moment (ex. résolution de captcha, authentification).

## 2. État actuel (mesuré, pas supposé)

> Réécrit le 2026-11-24. La version d'origine mesurait un sidecar Obscura/Servo
> (`ego.*`, viewport figé 1280×800, polling PNG 120 ms, localhost forcé en HTTPS).
> Ce chemin a été remplacé par Chromium système + CDP ; le tableau ci-dessous décrit
> le code présent. La matrice de parité vit dans `docs/PARITY.md` (lignes 1.17, 1.18, 3.6).

| Constat | Preuve |
|---|---|
| Moteur = binaire Chromium/Chrome/Brave/Edge/Obscura déjà installé, lancé headless | `browser/process.rs:313-400` (découverte `CHROME_PATH`/`KOMET_BROWSER_BIN` → PATH → chemins OS), `:195-217` (`--headless=new --ozone-platform=headless --remote-debugging-port=0`) |
| CDP sur WebSocket loopback, port dynamique ; UA de la version réelle du binaire (le jeton `HeadlessChrome` déclenchait les anti-bots) | `process.rs:224-233`, `:271-286` |
| Un Chrome par clé de panneau (`profile_key`), refcounté ; la dernière surface fermée tue le process via le keeper, et `PR_SET_PDEATHSIG` protège des orphelins si Komet meurt | `process.rs:13-115`, `:235-243` |
| Profil **persistant** par panneau (cookies/stockage conservés entre sessions), sous `{data_dir}/komet/browser-profiles/{sha256(profile_key)}` — pas de profil isolé par session | `process.rs:580-600`, `settings.rs` |
| Rendu push : `Page.startScreencast` + `Page.screencastFrameAck` par frame (contrôle de flux), plus de polling | `browser/cdp.rs:71-99`, `browser.rs:4716-4760` |
| Viewport = taille réelle du panneau × facteur d'échelle, via `Emulation.setDeviceMetricsOverride` ; presets appareil (téléphone/tablette/laptop) + taille custom + rotation | `browser.rs:890-907`, `:979-1115`, `:57-73` |
| `localhost`, `*.localhost`, IP privées et `:PORT` en `http://` ; TLS auto-signé local accepté par `--allow-insecure-localhost`, aucune désactivation globale des vérifications | `browser.rs:4438-4490`, `process.rs:230` |
| Clavier (`Input.dispatchKeyEvent` / `Input.insertText`), souris, scroll, collage, dialogues JS (`Page.handleJavaScriptDialog`), find-in-page (script JS injecté + compteur) | `browser.rs:1802-1960`, `:5049-5060`, `browser/cdp.rs` |
| Console intégrée : `Runtime.consoleAPICalled` + `Runtime.exceptionThrown`, filtres par niveau, REPL `Runtime.evaluate` | `browser.rs:395-419`, `:1729-1745` |
| Éléments : mode sélection, badges `CSS.highlights`, description DOM au survol, « Add to chat » | `browser/element.rs`, `browser.rs:1476-1503` |
| Historique (rétention 30/90/365 jours/illimité), favoris, clear storage (`Storage.clearDataForOrigin`, `Network.clearBrowserCookies/Cache`) | `browser/history.rs`, `browser/clear.rs` |
| Échec de lancement ou perte CDP = état d'erreur visible + bouton **Retry** (relance la session CDP), pas de crash silencieux | `browser.rs:390-400`, `:4649-4660`, `:5319` |
| **Pas de téléchargements** : seule sortie fichier = `Page.captureScreenshot` vers `capture_dir` | `browser.rs:1758-1770` ; aucun `Browser.setDownloadBehavior`/`Page.download*` |
| **Pas d'inspection réseau** : aucun `Network.requestWillBeSent`/`loadingFailed`/`responseReceived` | `browser/cdp.rs` (méthodes Network = clear uniquement) |
| **Aucun outil agent** vers cette surface : le MCP moteur n'expose que les terminaux | `crates/engine/src/mcp/catalog.rs` ; aucun outil browser |
| Onglets = onglets du panneau droit, une surface CDP par onglet ; pas de restauration au redémarrage | `shell/right_pane.rs:214-225` |

## 3. Objectifs / non-objectifs

**Objectifs**

1. Viewport = taille réelle du panneau (responsive, HiDPI), sans letterboxing.
2. Interaction fluide : scroll, clavier complet, sélection, copier/coller, IME.
3. URL libres + localhost/IP privées en `http://` par défaut, ports conservés.
4. Console JS intégrée (messages `console.*` + REPL `evaluate`).
5. Coût machine borné : pas de processus ni de CPU quand la surface est fermée.

**Non-objectifs (v1)**

- Parité Firefox/Chrome, DRM, extensions, Service Workers (le moteur Chromium système les
  gère, Komet ne les expose pas comme fonctionnalités).
- Lecture audio/vidéo dédiée, impression, gestion de mots de passe (audio muet par `--mute-audio`).
- Navigateur quotidien : c'est un navigateur d'aperçu/test.
- Pages web généralistes cassées sans recours : la barre d'acceptation est « une app
  Vite/React et des pages de doc s'affichent », pas « tout le web ».
- **HTTPS auto-signé** : accepté uniquement pour `localhost`
  (`--allow-insecure-localhost`) ; une erreur de certificat distante reste affichée.
  Pas de `ignore_certificate_errors` global.
- **Téléchargements et inspection réseau** : hors de la v1 ci-dessus, traités en phase 2
  du plan navigateur (voir §7).

**Anti-objectifs**

- Ajouter un moteur Chromium (poids, packaging, OS support).
- Empiler des captures PNG à 60 fps (CPU, latence).

## 4. Approche Retenue : CDP-First Unifiée (Inspiration Obscura)

Au lieu d'un protocole WebSocket ad-hoc propriétaire (`ego.*`), Komet s'aligne sur le standard universel **CDP (Chrome DevTools Protocol)** :

| Backend | Rôle dans Komet | Avantages |
|---|---|---|
| **A. Chromium Système** (Défaut) | Moteur headless lancé via CLI (`--headless=new --remote-debugging-port=0`) | **100% de compatibilité web**, 0 Mo ajouté au binaire Komet, multi-OS natif (Linux, macOS, Windows) |
| **B. Obscura Headless Engine** | Moteur léger Rust + V8 (`h4ckf0r0day/obscura`) en sidecar ou service | Démarrage ultra-rapide (<50ms), ~35 Mo RAM, 100% Rust |

## 5. Décisions de Conception

### 5.1 Responsive & Viewport
- Le conteneur GPUI calcule `bounds.size * window.scale_factor()`.
- Appel de redimensionnement direct via `Emulation.setDeviceMetricsOverride { width, height, deviceScaleFactor, mobile: false }`.
- Plus de déformation, letterboxing ou viewport figé à 1280×800.

### 5.2 Rendu de Frames : Push Screencast
- Fin du polling CPU 120 ms (`ego.capture`).
- Souscription via `Page.startScreencast { format: "jpeg", quality: 80 }`.
- Les frames sont reçues par push (`Page.screencastFrame`), décodées et directement envoyées à `gpui::RenderImage`.
- Accusé de réception immédiat avec `Page.screencastFrameAck { sessionId }` pour un contrôle de flux fluide (30 à 60 fps).

### 5.3 Barre d'Outils & UI Interactive (Humain)
- **Navigation** : Précédent, Suivant, Recharger / Arrêter.
- **Barre d'URL intelligente** : Ports conservés, détection automatique de `localhost` et IP privées en `http://`, tolérance pour les certificats HTTPS auto-signés locaux (`--allow-insecure-localhost`).
- **Bouton Capture (Screenshot)** :
  - Snapshot en 1 clic via `Page.captureScreenshot`.
  - Actions : Copier dans le presse-papier, Enregistrer sous, ou attacher directement au chat de l'agent.
- **Bouton Mode Overlay (Inspiration Obscura)** :
  - Surbrillance visuelle des éléments interactifs avec badges numérotés (`#1`, `#2`).
  - L'utilisateur peut cliquer sur un badge pour copier son identifiant ou guider l'agent CLI.
- **Tiroir Console & Réseau** :
  - Capture en continu des événements `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, et erreurs réseau.
  - Champ de commande REPL pour exécuter du JS en direct (`Runtime.evaluate`).

### 5.4 Interopérabilité Agent CLI (Sans Friction)
- Les agents Komet / CLI interagissent avec la même instance via les outils :
  - `browser_navigate(url)`
  - `browser_snapshot()` : extraction de l'arbre d'accessibilité avec UIDs (façon Obscura).
  - `browser_click(uid | x, y)`
  - `browser_fill(uid, text)`
  - `browser_screenshot()`
- Transparence visuelle : quand l'agent navigue ou clique, la vue du panneau droit se met à jour en direct.
- L'utilisateur peut reprendre la main manuellement à tout instant.

### 5.5 Cycle de Vie & Multi-OS
- Détection ordonnée de l'exécutable : variable d'environnement `CHROME_PATH`, puis Google Chrome, Brave, Microsoft Edge, Chromium, ou binaire `obscura`.
- Profil utilisateur isolé temporaire per-session (`--user-data-dir`).
- Processus sous surveillance avec terminaison automatique (`kill_on_drop`) à la fermeture de l'onglet.

## 6. Plan de Réalisation par Jalons

1. **Jalon 1 (Détection & Processus)** — *livré.* Probe multi-OS, lancement headless avec port
   CDP dynamique, pool par profil, keeper sans orphelin (`browser/process.rs`).
2. **Jalon 2 (Client CDP & Screencast)** — *livré.* Handshake, `Page.navigate`, screencast push
   avec ack, rendu GPUI (`browser.rs`, `browser/cdp.rs`).
3. **Jalon 3 (Interactivité Humaine & Responsive)** — *livré.* Souris, scroll, clavier/IME,
   redimensionnement dynamique, dialogues JS, find-in-page, zoom.
4. **Jalon 4 (Capture & Overlay, Console)** — *livré pour la capture, l'overlay d'éléments, la
   console/REPL, les téléchargements et l'onglet Network (erreurs + liste des requêtes).*
   **Reste :** vue des corps requête/réponse.
5. **Jalon 5 (Bridge Agent CLI)** — *non commencé.* La surface n'est pas pilotable par un agent ;
   les outils prévus (`browser_navigate/snapshot/click/fill/screenshot`) doivent vivre dans le
   moteur/MCP, pas dans les structures UI.

## 7. Écarts restants au 2026-11-24

Le plan de travail consolidé (phases 0–4) est repris dans `docs/PARITY.md` :

- **Recherche par défaut** — *livré.* Google est le défaut (`DEFAULT_SEARCH_URL`,
  `BrowserPrefs::default`) et une migration à la lecture remplace l'ancien défaut DuckDuckGo
  exact ; tout choix explicite est conservé (`BrowserPrefs::migrate`, testé).
- **Téléchargements** — *livré.* `Browser.setDownloadBehavior {behavior: allow, eventsEnabled}`
  au démarrage de session (dossier = réglage ou Téléchargements), événements
  `downloadWillBegin`/`downloadProgress` → onglet Downloads (progression throttlée 250 ms,
  Open/Reveal via gpui, « Clear finished »), réglage « Downloads folder » ré-appliqué en direct.
- **Inspection réseau (erreurs + liste requêtes)** — *livré.* `Network.enable` → onglet Network
  (méthode, URL, statut/erreur, annulations ≠ erreurs, `ERR_CERT_*` signalé, hôte local / http /
  https distingués, plafond 300 entrées, Clear). Reste : voir les corps requête/réponse.
- **Onglets et profils** — persistance des onglets au redémarrage ; et séparation explicite entre
  profil humain (persistant par panneau) et profil réservé aux sessions agent (aucun partage
  implicite des cookies personnels).
- **Outils agent** — surface typée dans le moteur/MCP, identifiants d'éléments valides pour un
  seul snapshot, résultats d'action explicites, confirmation avant action sensible, CDP jamais
  exposé hors de l'interface locale.
- **Annotations** — l'utilisateur annote un élément dans la page, l'agent reçoit l'annotation
  avec capture et contexte DOM.
