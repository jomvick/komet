# Official Logo Rollout Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Propager le nouveau visuel officiel (`dist/logo.png`, déjà déposé dans le repo aujourd'hui) vers TOUS les artefacts binaires dérivés — icône Linux (tarball + AppImage), `.ico` Windows embarqué dans l'exécutable, `.icns` macOS — sans casser aucun script de packaging existant, puisque tous pointent déjà vers des noms de fichiers stables.

## Contexte (discovery déjà fait)

- **`dist/logo.png`** (1254×1254, RGB, sans alpha) — créé aujourd'hui (02/09), c'est le nouveau master. Design : comète blanche + traînée de points sur fond noir, avec un masque "squircle" (coin arrondis) et une ombre subtile déjà incrustés dans les pixels — exactement la convention que le pipeline macOS attend déjà (voir `scripts/package-macos.sh` : *"squircle + margins + shadow baked into dist/macos/icon-1024.png — sips can't alpha-mask, so the mask is applied ahead of time"*).
- **Artefacts dérivés actuels, tous datés du 29/08 (donc PÉRIMÉS)** — même composition mais en résolution pixelisée/dentelée (visiblement upscalée depuis une petite source, ~64-128px) :
  - `dist/komet.png` (247 Ko) — variante **plate, carrée, sans masque** (coins vifs à 90°) : utilisée par `scripts/package-linux.sh` (tarball, icône hicolor) ET `scripts/package-appimage.sh` (AppImage, `.DirIcon`).
  - `dist/macos/icon-1024.png` (204 Ko) — variante **masquée squircle + ombre**, utilisée par `scripts/package-macos.sh` pour générer le `.iconset` → `.icns`.
  - `dist/windows/komet.ico` — multi-résolution, embarqué au build via `apps/komet/build.rs` (crate `winres`) — donc packagé directement DANS l'exécutable `.exe`, pas copié à côté.
  - `dist/Komet.png` (355 Ko, majuscule) — doublon non référencé par aucun script trouvé ; à vérifier puis nettoyer.
  - `dist/komet_logo.svg` — vectoriel, probablement pour la doc/le site ; pas de source vectorielle du nouveau design disponible, donc hors scope de régénération automatique (voir Task 4).
- **`.deb` et `.rpm` n'existent PAS actuellement** dans ce repo — ni script local (`scripts/` n'a que tarball + AppImage pour Linux), ni job CI (`.github/workflows/release.yml` ne construit que tarball Linux, AppImage, dmg macOS, zip/exe Windows). Ta demande les mentionne comme "déjà en place" — ce n'est pas le cas ; je le traite comme une **Task optionnelle séparée** (Task 5) plutôt que de faire semblant qu'ils existent.
- Le site web (`komet-site/public/*`) a ses propres copies (`komet.png`, `komet-logo.svg`, `favicon.ico`) — hors du périmètre "exécutables" demandé, noté en Task 6 comme suivi optionnel.

## Décision technique clé : un seul master, deux variantes dérivées automatiquement

Le pipeline existant a toujours utilisé **deux variantes** de la même illustration :
1. **Plate** (coins carrés, pas de masque) → Linux (`komet.png`) et, par cohérence, Windows (`.ico` — Windows ne pré-arrondit pas ses icônes non plus).
2. **Masquée squircle + ombre** → macOS (`icon-1024.png` → `.icns`).

Le nouveau `dist/logo.png` fourni EST déjà la variante (2). Comme le design est un monochrome blanc-sur-noir sans dégradés complexes ailleurs que l'ombre du coin, la variante (1) peut être **dérivée automatiquement et sans perte** de (2) : on recompose la comète (tout pixel non-noir) sur un fond noir plat, ce qui élimine le fondu d'ombre du coin sans toucher au dessin lui-même. Pas besoin de redemander un second export à la source du design.

## Global Constraints
- Ne renommer AUCUN fichier existant (`dist/komet.png`, `dist/macos/icon-1024.png`, `dist/windows/komet.ico`) — tous les scripts de packaging et `build.rs` les référencent par leur chemin exact ; changer un nom casserait silencieusement un ou plusieurs scripts.
- Suivre la convention déjà établie par `scripts/dmg-background.py` : un script Python (Pillow) exécuté **une fois localement par le développeur**, dont la SORTIE (les PNG/ICO régénérés) est committée — la CI n'a jamais besoin de Python/Pillow à l'exécution.
- Ne pas introduire de nouvelle dépendance système (pas d'ImageMagick, pas de `rsvg-convert`) — Pillow suffit et est déjà la norme du repo.
- Chaque sortie doit être vérifiée visuellement avant commit (pas de confiance aveugle dans le script) — les tailles réduites (16×16, 32×32) sont les plus à risque de perdre le détail des petits points de la traînée.

---

### Task 1: Script de génération unique (`scripts/gen-icons.py`)

**Files:**
- Create: `scripts/gen-icons.py`

**Interfaces:**
- Consumes: `dist/logo.png` (seule source de vérité)
- Produces: `dist/komet.png`, `dist/macos/icon-1024.png`, `dist/windows/komet.ico` (tous réécrits en place)

- [ ] **Step 1: Charger le master et vérifier ses dimensions**

```python
#!/usr/bin/env python3
"""Regenerate every derived app-icon asset from the single master artwork.

Source of truth: dist/logo.png (the squircle-masked, shadow-baked render —
same convention scripts/package-macos.sh already expects for icon-1024.png).
Everything else (the flat Linux/Windows variant) is derived from it
automatically: the design is a monochrome white comet on solid black, so
recompositing "anything non-black" onto a flat black square removes the
corner shadow/vignette without touching the artwork itself.

Outputs are committed (same policy as scripts/dmg-background.py) — CI never
needs Pillow. Re-run this only when dist/logo.png changes.

Usage: python3 scripts/gen-icons.py
"""
import os
from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MASTER = os.path.join(ROOT, "dist/logo.png")

master = Image.open(MASTER).convert("RGB")
assert master.width == master.height, f"expected a square master, got {master.size}"
```

- [ ] **Step 2: Dériver la variante plate (Linux/Windows) par recomposition sur fond noir**

```python
def flatten_to_square(img: Image.Image, threshold: int = 18) -> Image.Image:
    """Recomposite the comet onto a flat black square, dropping the
    squircle-corner shadow/vignette baked into the macOS-shaped master.
    `threshold`: pixels with max(R,G,B) at or below this are treated as
    "background" (pure black) — comfortably above the ~0-10 shadow gradient
    observed near the rounded corners, comfortably below the comet's dimmest
    visible gray."""
    px = img.load()
    out = Image.new("RGB", img.size, (0, 0, 0))
    out_px = out.load()
    for y in range(img.height):
        for x in range(img.width):
            r, g, b = px[x, y]
            if max(r, g, b) > threshold:
                out_px[x, y] = (r, g, b)
    return out

flat = flatten_to_square(master)
```

- [ ] **Step 3: QA visuelle immédiate — comparer les 4 coins avant/après**

```python
# Sanity check before writing anything: every corner pixel of `flat` must be
# pure black now (the masked master's corners were the rounded/shadowed
# ones). If this fails, the threshold picked up real artwork — stop and
# inspect visually instead of writing bad output.
w, h = flat.size
corners = [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]
for cx, cy in corners:
    assert flat.getpixel((cx, cy)) == (0, 0, 0), f"corner {cx},{cy} not pure black — inspect before continuing"
```

- [ ] **Step 4: Écrire `dist/komet.png` (Linux — tarball + AppImage, 1024×1024 comme documenté dans `dist/README.md`)**

```python
flat_1024 = flat.resize((1024, 1024), Image.LANCZOS)
flat_1024.save(os.path.join(ROOT, "dist/komet.png"))
```

- [ ] **Step 5: Écrire `dist/macos/icon-1024.png` (simple resize du master déjà masqué, pas de retraitement)**

```python
macos_icon = master.resize((1024, 1024), Image.LANCZOS)
macos_icon.save(os.path.join(ROOT, "dist/macos/icon-1024.png"))
```

- [ ] **Step 6: Écrire `dist/windows/komet.ico` (multi-résolution depuis la variante plate)**

```python
# Windows classic size set (matches what's already embedded in the current
# .ico — 16/32/48/256 covers taskbar, Explorer details/large-icons, and the
# Alt-Tab switcher).
ico_sizes = [(16, 16), (32, 32), (48, 48), (256, 256)]
flat.save(
    os.path.join(ROOT, "dist/windows/komet.ico"),
    sizes=ico_sizes,
)
```

- [ ] **Step 7: Message de fin + rappel de vérification manuelle**

```python
print("Regenerated: dist/komet.png, dist/macos/icon-1024.png, dist/windows/komet.ico")
print("Next: visually inspect all three, especially at small sizes (16x16, 32x32)")
print("      before committing — run scripts/package-*.sh locally to confirm.")
```

- [ ] **Step 8: Exécuter et committer**

```bash
python3 scripts/gen-icons.py   # ou: pip install --user pillow d'abord si absent
git add dist/komet.png dist/macos/icon-1024.png dist/windows/komet.ico scripts/gen-icons.py
git commit -m "chore: regenerate app icons from the new dist/logo.png master"
```

---

### Task 2: Vérification — chaque pipeline de packaging consomme bien les fichiers régénérés

**Files:** aucune modification attendue ici — cette tâche est une VÉRIFICATION, pas une réécriture, puisque les 3 scripts référencent déjà les bons chemins par nom stable.

- [ ] **Step 1: Rebuild + inspection Linux (tarball)**

```bash
scripts/package-linux.sh
tar -xzf target/package/komet-*-linux-*.tar.gz -C /tmp/komet-check
xdg-open /tmp/komet-check/komet-*/komet.png   # doit montrer le NOUVEAU logo, coins carrés nets
```

- [ ] **Step 2: Rebuild + inspection AppImage**

```bash
scripts/package-appimage.sh
# Lancer l'AppImage puis vérifier l'icône dans le lanceur d'applications
# (varie selon le DE — GNOME/KDE mettent en cache les icônes, un
# `killall gnome-shell` ou re-login peut être nécessaire pour voir le
# nouveau .DirIcon si l'ancien est resté en cache)
```

- [ ] **Step 3: Rebuild + inspection Windows (nécessite un runner/VM Windows, ou CI)**

```bash
scripts/package-windows.sh   # doit être lancé sous Windows (ou via bash Git/WSL) — build.rs embarque le .ico au moment de la compilation Rust, PAS à l'empaquetage
```
Vérifier ensuite dans l'Explorateur (clic droit → Propriétés → icône, ou simplement visualiser le `.exe` en mode "grandes icônes") que le nouveau logo apparaît — et pas seulement dans le zip, mais bien SUR l'exécutable lui-même (preuve que `winres`/`build.rs` a bien recompilé avec le nouveau `.ico` — un `cargo clean -p komet` avant rebuild élimine le risque de cache incrémental gardant l'ancien `.ico` embarqué).

- [ ] **Step 4: Rebuild + inspection macOS (nécessite un Mac, ou CI)**

```bash
scripts/package-macos.sh
open target/package/Komet.app   # vérifier l'icône dans le Dock ET dans le Finder (Cmd+I)
```

- [ ] **Step 5: Commit d'un éventuel correctif si une taille précise ne rend pas bien** (ex: si les petits points de la traînée disparaissent à 16×16, ajuster `ico_sizes` ou accepter une version légèrement simplifiée à cette taille — décision visuelle, pas technique).

---

### Task 3: Nettoyage — doublon `dist/Komet.png`

**Files:**
- Delete (après vérification) : `dist/Komet.png`

- [ ] **Step 1: Confirmer l'absence de référence** (scripts, CI, site) avant suppression :

```bash
grep -rn "Komet\.png" --include="*.sh" --include="*.yml" --include="*.ts" --include="*.tsx" --include="*.rs" .
```

- [ ] **Step 2: Si aucune référence trouvée, supprimer**

```bash
git rm dist/Komet.png
git commit -m "chore: remove unreferenced duplicate Komet.png"
```

---

### Task 4: `dist/komet_logo.svg` — flag manuel, pas d'automatisation

**Pas de sous-étapes de code ici** — juste une décision à documenter : on ne possède pas de source vectorielle du nouveau design (seulement un raster 1254×1254). Deux options, à trancher par toi :
- **(a) Recommandé** : laisser le SVG tel quel pour l'instant (ancien design), le retraiter à la main plus tard si un vrai fichier vectoriel du nouveau logo existe (Figma/Illustrator export).
- **(b) Dépannage rapide** : embarquer le PNG dans un SVG trivial (`<image href="data:image/png;base64,...">`) — fonctionne visuellement partout où le SVG est utilisé, mais n'est plus un "vrai" vecteur (ne remise pas à l'échelle proprement à très haute résolution).

---

### Task 5 (optionnelle, hors scope actuel — à confirmer avant de lancer): Ajout réel de `.deb` et `.rpm`

Comme noté en Discovery, ces formats n'existent pas encore. Si tu veux les ajouter (pas juste préparer l'icône), ça implique :
- `cargo install cargo-deb` + section `[package.metadata.deb]` dans `apps/komet/Cargo.toml` (référence `dist/komet.desktop` + `dist/komet.png` comme `assets`).
- `cargo install cargo-generate-rpm` + section `[package.metadata.generate-rpm]` équivalente.
- Nouveau job CI dans `.github/workflows/release.yml` (ou étape ajoutée au job `linux` existant).

Je ne détaille pas ça plus avant en tâches — dis-moi si tu veux que j'en fasse un plan séparé complet, ou si l'icône seule suffisait pour l'instant et que le `.deb`/`.rpm` était juste une confusion sur l'état actuel du repo.

---

### Task 6 (optionnelle, hors scope "exécutables"): Site web

`komet-site/public/komet.png`, `komet-logo.svg`, `favicon.ico`, et `komet-site/src/app/favicon.ico` ont leurs propres copies, actuellement aussi sur l'ancien design. Même limitation qu'en Task 4 pour le SVG. Le PNG/favicon peuvent être régénérés avec le même script (`gen-icons.py` étendu) si tu veux que je l'inclue.

---

## Self-Review
- Couverture : Linux (tarball + AppImage), Windows (.ico embarqué au build), macOS (.icns) — les 3 plateformes demandées sont couvertes par régénération in-place sans casser aucun chemin de fichier existant.
- `.deb`/`.rpm` : signalés comme INEXISTANTS plutôt que traités comme "déjà en place" — écart assumé avec la demande initiale, proposé en tâche séparée optionnelle.
- Aucun changement de code Rust n'est nécessaire (`build.rs` référence déjà `dist/windows/komet.ico` par chemin stable) — uniquement des assets binaires régénérés + un script committé.
- Risque non résolu à documenter : le seuil de `flatten_to_square` (18) est une estimation prudente non testée sur le fichier réel — Step 3 du Task 1 inclut une assertion automatique sur les coins, mais la qualité du dégradé/anti-aliasing autour de la comète elle-même (pas juste les coins) doit être vérifiée visuellement avant commit.
