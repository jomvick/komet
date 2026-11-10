# Surface navigateur Komet unifiée (Humain & Agent CLI) — Design

Date : 2026-11-08. Statut : approuvé. Périmètre : surface Browser intégrée au panneau droit de Komet,
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

| Constat | Preuve |
|---|---|
| Viewport figé 1280×800, jamais redimensionné | `crates/ui/src/browser.rs:31` ; le bridge n'expose aucun `resize` |
| Frames = PNG pleine page, poll toutes les 120 ms | `browser.rs:771-807` (`ego.capture`) |
| Coût réel d'une frame : 1,9 s (page vide) à 3–14 s (Wikipédia) | sonde directe du sidecar |
| Le moteur produit déjà des frames « sales » | `webview/engine/src/lib.rs:622-651` (`EngineEvent::Frame`), ignorées par l'UI |
| Molette envoyée en (0,0) | `webview/engine/src/lib.rs:527-535` |
| Clavier = caractères imprimables seulement | `browser.rs:241-258` |
| `localhost:5173` forcé en `https://` → échec TLS | `browser.rs:544-551` |
| Pas de console, pas de resize, pas d'IME, pas de presse-papier | UI et bridge |
| Compatibilité Servo partielle (pas de WebGL/IndexedDB/SW, pas de codecs) | sonde |

Conclusion : le plafond n'est pas le moteur mais le **pipeline de rendu et la couche
d'entrée**. Le moteur expose déjà tout ce qu'il faut (`InputEvent::Ime`,
`EditingAction`, `ClipboardAction`, `Cursor`, `show_console_message`,
`request_create_new`, préférences DevTools).

## 3. Objectifs / non-objectifs

**Objectifs**

1. Viewport = taille réelle du panneau (responsive, HiDPI), sans letterboxing.
2. Interaction fluide : scroll, clavier complet, sélection, copier/coller, IME.
3. URL libres + localhost/IP privées en `http://` par défaut, ports conservés.
4. Console JS intégrée (messages `console.*` + REPL `evaluate`).
5. Coût machine borné : pas de processus ni de CPU quand la surface est fermée.

**Non-objectifs (v1)**

- Parité Firefox/Chrome, DRM, extensions, Service Workers.
- Lecture audio/vidéo (exigerait `media-gstreamer` : dépendance système lourde).
- Téléchargements, impression, gestion de mots de passe.
- Navigateur quotidien : c'est un navigateur d'aperçu/test.
- Navigateur **des agents** : l'automatisation agent reste sur `chrome-devtools-mcp`
  (spec `2026-10-12-browser-use-komet-design.md`) ; cette surface est pour l'humain.
- Pages web généralistes cassées sans recours : la barre d'acceptation est « une app
  Vite/React et des pages de doc s'affichent », pas « tout le web ».
- **HTTPS auto-signé** (Vite `--https`, Next dev, mkcert) : hors v1. `http://` localhost
  seulement ; une erreur TLS explicite est affichée. Pas de `ignore_certificate_errors`
  global (il accepterait aussi les certificats invalides sur Internet).
- **macOS et Windows** : pas de sidecar en v1. La surface affiche un état « moteur
  indisponible » avec la commande de build (voir §5.7).

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

1. **Jalon 1 (Détection & Processus)** : Module de probing multi-OS des navigateurs disponibles et lancement du sous-processus headless avec port CDP dynamique.
2. **Jalon 2 (Client CDP & Screencast)** : Connexion WebSocket CDP, handshake, `Page.navigate` et boucle de réception push `Page.screencastFrame` rendue dans GPUI.
3. **Jalon 3 (Interactivité Humaine & Responsive)** : Dispatching des clics souris, scroll, frappe clavier, et redimensionnement dynamique du viewport.
4. **Jalon 4 (Fonctionnalités Clés : Capture & Overlay Obscura)** : Ajout du bouton Capture d'écran, du mode overlay avec badges UIDs et du tiroir Console/Network.
5. **Jalon 5 (Bridge Agent CLI)** : Liaison sans friction avec les outils de l'agent pour le pilotage et la synchronisation live.
