# Background effects (None / ASCII) — plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal :** ajouter un réglage `Background effect` à l'image de fond existante — `None` peint l'image telle quelle, `Ascii` la convertit une fois en une texture RGBA de caractères colorés (GPUI ne peint ensuite qu'une seule image).

**Architecture :** l'effet est un enum persisté à côté de `wallpaper_path` / `wallpaper_scope`. La conversion ASCII est une fonction pure (`src → RgbaImage`) exécutée sur le background executor, mise en cache disque, puis collée dans le global `WallpaperState` sous forme d'`Arc<RenderImage>` — le même chemin que `browser.rs::gpui_render_image`. Les couches de fond (`shell.rs`, `card_layer`) choisissent le chemin `img(path)` ou `img(texture)`.

**Tech Stack :** Rust 2024, GPUI (fork jomvick/zed), crate `image` 0.25 (`png`, `jpeg`), `font8x8` 0.3 (bitmaps 8×8), `sha2` (déjà dép. de `komet-ui`).

**Spec :** conversation du 2026-11-14 (recommandations fond/ASCII) + ce document.

## Global Constraints

- Effet **statique** : aucune animation, aucune régénération par frame. Une seule texture par (image, params, palier de taille).
- Aucun décodage ni rendu sur le thread UI : tout passe par `cx.background_executor()`.
- Formats ASCII supportés : **PNG et JPEG uniquement** (features du crate `image`). Les autres formats restent affichables en `None` ; le bouton ASCII affiche « ASCII needs a PNG or JPEG. »
- Défauts : `columns = 120`, `charset = Classic`, `color = Source`, `invert = false`, `opacity = 0.35`.
- Clamp au chargement : `columns ∈ 40..=240`, `opacity ∈ 0.05..=1.0`, valeurs non finies → défaut.
- Le fichier de réglages reste `{data_dir}/ui-settings.json`, forme camelCase, `serde(default)` partout (fichiers plus anciens lisibles).
- Aucune nouvelle dépendance hors `font8x8`.
- Vérif avant commit : `cargo fmt --all`, `cargo check -p komet-ui`, `cargo clippy -p komet-ui --all-targets -- -D warnings`, `cargo test -p komet-ui <module>`.

## Review Focus

- Fichier source corrompu ou tronqué après import : le décodage renvoie `Err`, l'UI garde l'image d'origine et le log — jamais de panic ni d'écran noir (test Task 4).
- Format non décodable (webp/gif) choisi pour ASCII : message explicite, `None` continue de fonctionner (test Task 3).
- Fenêtre redimensionnée à 0 px ou image 1×1 : `grid_size`/`cell_at` ne divisent jamais par zéro (test Task 3).
- Changement d'effet ou de paramètre pendant qu'un job tourne : une seule texture en vol, le dernier réglage gagne, l'ancienne ne réapparaît pas (test `should_render`, Task 4).
- Paramètre hors bornes (colonnes 10 000, opacité `NaN`) relu depuis le JSON : clampé au chargement, l'UI ne s'affiche pas avec une texture noire ou invisible (test Task 1).

---

### Task 1 : Persister l'effet et ses paramètres

**Files :**
- Modify: `crates/ui/src/settings/wallpaper.rs`
- Modify: `crates/ui/src/settings.rs:166-185` (champs), `:191-226` (Default), `:440-466` (clamped), tests `:568-580`
- Test: `crates/ui/src/settings.rs` (module `tests` existant)

**Interfaces :**
- Consumes : rien.
- Produces :
```rust
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundEffect { #[default] None, Ascii(AsciiParams) }

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AsciiParams {
    pub columns: u16,          // DEFAULT_COLUMNS
    pub charset: AsciiCharset, // Classic | Blocks | Binary
    pub color: AsciiColor,     // Source | Mono | Accent
    pub invert: bool,
    pub opacity: f32,          // DEFAULT_OPACITY = 0.35
}
impl AsciiParams { pub fn clamped(self) -> Self; }
impl BackgroundEffect { pub fn clamped(self) -> Self; pub fn is_ascii(self) -> bool; }
```
et `UiSettings::background_effect: BackgroundEffect`.

- [ ] **Step 1 : écrire les tests qui échouent** dans `crates/ui/src/settings.rs`

```rust
#[test]
fn background_effect_defaults_to_none_for_old_files() {
    let loaded: UiSettings = serde_json::from_str(r#"{"soundEnabled": false}"#).unwrap();
    assert_eq!(loaded.background_effect, wallpaper::BackgroundEffect::None);
}

#[test]
fn ascii_effect_round_trips_and_clamps() {
    let params = wallpaper::AsciiParams {
        columns: 10_000, opacity: f32::NAN, ..Default::default()
    };
    let json = serde_json::to_string(&wallpaper::BackgroundEffect::Ascii(params)).unwrap();
    assert!(json.contains(r#""ascii""#), "got {json}");
    let decoded: wallpaper::BackgroundEffect = serde_json::from_str(&json).unwrap();
    let wallpaper::BackgroundEffect::Ascii(params) = decoded.clamped() else {
        panic!("expected ascii");
    };
    assert_eq!(params.columns, 240);
    assert_eq!(params.opacity, wallpaper::DEFAULT_OPACITY);
}
```

- [ ] **Step 2 : run** `cargo test -p komet-ui settings::tests::background_effect -- --nocapture`
  Expected : FAIL (`background_effect` n'existe pas).

- [ ] **Step 3 : implémenter** dans `settings/wallpaper.rs` : `AsciiCharset`, `AsciiColor` (avec `ALL`, `label()` — « Classic », « Blocks », « Binary », « Image colours », « Mono », « Accent »), `AsciiParams` + `Default` + `clamped`, `BackgroundEffect` + `clamped`/`is_ascii`. Ajouter `#[serde(default)] pub background_effect: BackgroundEffect` à `UiSettings`, l'initialiser à `None` dans `Default`, appeler `.clamped()` dans `UiSettings::clamped`.

- [ ] **Step 4 : run** `cargo test -p komet-ui settings::tests`
  Expected : PASS (y compris `round_trip` et `settings_without_wallpaper_default_to_none`).

- [ ] **Step 5 : commit**
```bash
git add crates/ui/src/settings.rs crates/ui/src/settings/wallpaper.rs
git commit -m "feat(appearance): persist background effect and ASCII params"
```

---

### Task 2 : Importer l'image dans le dossier de données

**Files :**
- Modify: `crates/ui/src/settings/wallpaper.rs` (`set_path`, nouveaux `import_copy`/`persist_path`)
- Test: `crates/ui/src/settings/wallpaper.rs` (nouveau `mod tests`)

**Interfaces :**
- Consumes : `UiSettings` (Task 1).
- Produces :
```rust
fn import_copy(data_dir: &Path, src: &Path) -> io::Result<PathBuf>;
/// Importe `new`, met à jour ui-settings.json, supprime l'ancienne copie.
fn persist_path(data_dir: &Path, new: Option<&Path>) -> io::Result<Option<PathBuf>>;
```
`import_copy` copie sous `{data_dir}/wallpapers/{16 premiers hex de sha256}<ext>` ; no-op si `src` est déjà dans ce dossier.

- [ ] **Step 1 : test qui échoue**

```rust
#[test]
fn import_copy_moves_the_image_under_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.png");
    std::fs::write(&src, b"png-bytes").unwrap();
    let stored = import_copy(dir.path(), &src).unwrap();
    assert!(stored.starts_with(dir.path().join("wallpapers")));
    assert_eq!(std::fs::read(&stored).unwrap(), b"png-bytes");
    assert_eq!(import_copy(dir.path(), &stored).unwrap(), stored, "import is idempotent");
}

#[test]
fn persisting_a_new_wallpaper_drops_the_previous_copy() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.png");
    let b = dir.path().join("b.png");
    std::fs::write(&a, b"aaa").unwrap();
    std::fs::write(&b, b"bbb").unwrap();
    let first = persist_path(dir.path(), Some(&a)).unwrap().unwrap();
    let second = persist_path(dir.path(), Some(&b)).unwrap().unwrap();
    assert!(!first.exists(), "the replaced copy is deleted");
    assert!(second.exists());
    assert_eq!(UiSettings::load(dir.path()).wallpaper_path.as_deref(),
               Some(second.to_string_lossy().as_ref()));
}
```

- [ ] **Step 2 : run** `cargo test -p komet-ui settings::wallpaper::tests::import_copy`
  Expected : FAIL (`import_copy` absent).

- [ ] **Step 3 : implémenter** `import_copy` et `persist_path` (lire les octets, `sha2::Sha256`, `create_dir_all`, `fs::copy`, suppression de l'ancienne copie si elle est sous `{data_dir}/wallpapers/`) ; `set_path` devient `persist_path(...)` + mise à jour du global + `Theme::install` + `cx.refresh_windows()`.

- [ ] **Step 4 : run** `cargo test -p komet-ui settings::wallpaper` → PASS.

- [ ] **Step 5 : commit**
```bash
git add crates/ui/src/settings/wallpaper.rs
git commit -m "feat(appearance): import wallpapers into the data dir"
```

---

### Task 3 : Cœur ASCII pur (rampe, grille, raster)

**Files :**
- Create: `crates/ui/src/settings/wallpaper/ascii.rs`
- Modify: `crates/ui/src/settings/wallpaper.rs` (`pub mod ascii;`)
- Modify: `crates/ui/Cargo.toml` (`font8x8 = "0.3"`), `Cargo.toml` (workspace.dependencies)
- Test: `crates/ui/src/settings/wallpaper/ascii.rs` (`mod tests`)

**Interfaces :**
- Consumes : `AsciiParams`, `AsciiCharset`, `AsciiColor` (Task 1).
- Produces :
```rust
pub fn ramp(charset: AsciiCharset) -> &'static str;            // " .:-=+*#%@", " ░▒▓█", " 01"
pub fn cell_char(luma: f32, invert: bool, ramp: &str) -> char;
pub fn supports_ascii(path: &Path) -> bool;                     // png/jpg/jpeg, insensible à la casse
pub fn grid_size(target: (u32, u32), columns: u16) -> (u32, u32, u32); // (cell_w, cell_h, rows)
pub fn cell_at(src: &RgbaImage, cell: (u32, u32), grid: (u32, u32, u32), target: (u32, u32)) -> (u8, u8, u8);
pub fn render(src: &RgbaImage, params: &AsciiParams, target: (u32, u32), accent: [u8; 3]) -> RgbaImage;
```

- [ ] **Step 1 : tests qui échouent**

```rust
#[test]
fn luma_to_char_is_monotonic_and_inverted() {
    let r = ramp(AsciiCharset::Classic);
    assert_eq!(cell_char(0.0, false, r), ' ');
    assert_eq!(cell_char(1.0, false, r), '@');
    assert_eq!(cell_char(0.0, true, r), '@');
    assert_eq!(cell_char(1.0, true, r), ' ');
}

#[test]
fn grid_never_divides_by_zero() {
    assert_eq!(grid_size((0, 0), 120).2, 1);
    let (cw, ch, rows) = grid_size((1600, 1000), 120);
    assert_eq!(cw, 13);
    assert_eq!(ch, 26);
    assert_eq!(rows, 38);
}

#[test]
fn render_is_black_behind_a_space_cell() {
    let src = RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
    let params = AsciiParams::default();          // color Source
    let out = render(&src, &params, (120, 60), [255, 255, 255]);
    assert_eq!((out.width(), out.height()), (120, 60));
    assert!(out.pixels().all(|p| p.0[..3] == [0, 0, 0]));
}

#[test]
fn render_paints_light_cells_with_the_accent() {
    let src = RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255]));
    let params = AsciiParams { columns: 4, color: AsciiColor::Accent, ..Default::default() };
    let out = render(&src, &params, (64, 64), [200, 10, 10]);
    assert!(out.pixels().any(|p| p.0[0] > 100 && p.0[1] < 60), "accent tint missing");
}

#[test]
fn ascii_support_is_png_jpeg_only() {
    assert!(supports_ascii(Path::new("a.PNG")));
    assert!(supports_ascii(Path::new("a.jpeg")));
    assert!(!supports_ascii(Path::new("a.webp")));
}
```

- [ ] **Step 2 : run** `cargo test -p komet-ui settings::wallpaper::ascii`
  Expected : FAIL (module absent).

- [ ] **Step 3 : implémenter** dans `ascii.rs` :

1. `ramp` : les trois rampes ci-dessus (du plus sombre au plus clair).
2. `cell_char` : `let i = ((luma.clamp(0.0,1.0) * (ramp.len()-1) as f32).round() as usize).min(...)`, inversion avant le calcul ; `ramp.chars().nth(i)`.
3. `grid_size` : `cell_w = max(1, target.0 / columns)`, `cell_h = cell_w * 2`, `rows = max(1, target.1 / cell_h)`.
4. `cell_at` : `let (cw, ch, _) = grid;` puis échantillon au centre de la cellule avec le cadrage `Cover` :
```rust
let scale = (src.width() as f32 / target.0 as f32).max(src.height() as f32 / target.1 as f32);
let ox = (src.width() as f32 - target.0 as f32 * scale) / 2.0;
let oy = (src.height() as f32 - target.1 as f32 * scale) / 2.0;
let sx = (((cell.0 * cw + cw / 2) as f32) * scale + ox).clamp(0.0, src.width() as f32 - 1.0);
let sy = (((cell.1 * ch + ch / 2) as f32) * scale + oy).clamp(0.0, src.height() as f32 - 1.0);
```
5. `render` : canvas noir opaque ; pour chaque cellule, luminance `0.2126r + 0.7152g + 0.0722b` / 255, caractère via `cell_char`, couleur via `AsciiColor` (`Source` = rgb de la cellule, `Mono` = gris `luma*255`, `Accent` = `accent * luma`), bitmap `font8x8::UnicodeFonts::get(ch)` (essayer `BASIC_FONTS` puis `BLOCK_FONTS`) et pour chaque pixel de la cellule `(bitmap[py*8/cell_h] >> (px*8/cell_w)) & 1` — agrandissement/réduction au plus proche voisin ; caractère absent → cellule noire.

- [ ] **Step 4 : run** `cargo test -p komet-ui settings::wallpaper::ascii` → PASS, puis `cargo check -p komet-ui`.

- [ ] **Step 5 : commit**
```bash
git add Cargo.toml Cargo.lock crates/ui/Cargo.toml crates/ui/src/settings/wallpaper.rs crates/ui/src/settings/wallpaper/ascii.rs
git commit -m "feat(appearance): add the ASCII raster core"
```

---

### Task 4 : Texture hors-thread, clé de cache et cache disque

**Files :**
- Modify: `crates/ui/src/settings/wallpaper.rs` (`WallpaperState`, `init`, accesseurs)
- Modify: `crates/ui/src/settings/wallpaper/ascii.rs` (`cache_key`, `should_render`, `load_source`)
- Modify: `crates/ui/src/lib.rs:163` (passer aussi `background_effect` à `wallpaper::init`)
- Test: `crates/ui/src/settings/wallpaper/ascii.rs`

**Interfaces :**
- Consumes : `render`, `supports_ascii`, `grid_size` (Task 3) ; `import_copy` (Task 2).
- Produces :
```rust
pub fn init(path: Option<String>, effect: BackgroundEffect, data_dir: PathBuf, cx: &mut App);
impl WallpaperState { ascii: Option<Arc<RenderImage>>, ascii_key: Option<String>, pending: Option<Task<()>> }
pub fn effect(cx: &App) -> BackgroundEffect;
pub fn set_effect(effect: BackgroundEffect, cx: &mut App) -> io::Result<()>;
pub fn ascii_texture(cx: &App) -> Option<Arc<RenderImage>>;
pub fn ensure_ascii(target: (u32, u32), accent: [u8; 3], cx: &mut App);   // idempotent, appelé au render
// dans ascii.rs, purs :
pub fn cache_key(source: &Path, params: &AsciiParams, target: (u32, u32)) -> String; // palier de 256 px
pub fn should_render(ready_key: Option<&str>, pending: bool, wanted: &str) -> bool;
pub fn load_source(path: &Path) -> Result<RgbaImage, String>;            // décodage, erreur lisible
```

- [ ] **Step 1 : tests qui échouent**

```rust
#[test]
fn cache_key_buckets_the_target_size() {
    let p = AsciiParams::default();
    let a = cache_key(Path::new("/w/a.png"), &p, (1600, 1000));
    assert_eq!(a, cache_key(Path::new("/w/a.png"), &p, (1700, 1030)));   // même palier 256
    assert_ne!(a, cache_key(Path::new("/w/a.png"), &p, (2000, 1000)));   // palier différent
    assert_ne!(a, cache_key(Path::new("/w/a.png"), &AsciiParams { invert: true, ..p }, (1600, 1000)));
}

#[test]
fn a_corrupt_source_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.png");
    std::fs::write(&path, b"not a png").unwrap();
    assert!(load_source(&path).is_err());
}

#[test]
fn a_render_is_skipped_while_one_holds_the_right_key() {
    assert!(should_render(None, false, "k"));          // rien en cache
    assert!(!should_render(Some("k"), false, "k"));    // déjà à jour
    assert!(should_render(Some("old"), false, "k"));   // à régénérer
    assert!(!should_render(None, true, "k"));          // un job tourne déjà
}
```

- [ ] **Step 2 : run** `cargo test -p komet-ui settings::wallpaper`
  Expected : FAIL (`cache_key` absent).

- [ ] **Step 3 : implémenter**

1. `WallpaperState` gagne `effect`, `ascii`, `ascii_key`, `pending` ; `init` lit `ui_settings.background_effect`.
2. `set_effect` : écrit `UiSettings.background_effect`, met à jour le global, vide `ascii`/`ascii_key` si l'effet change, `cx.refresh_windows()`. Pour `Ascii` sur une image non supportée, renvoyer `io::Error` (l'UI affiche le message) **sans** changer l'effet enregistré.
3. `cache_key` : `sha256(palier_w, palier_h, path, columns, charset, color, invert, opacity arrondie à 1e-3)` en hexadécimal ; palier = `(v / 256 * 256).max(256)`.
4. `ensure_ascii` :
   - retourne si `effect` n'est pas `Ascii`, pas de `path`, ou `!should_render(ascii_key, pending.is_some(), &key)` ;
   - `pending = Some(cx.spawn(async move |cx| { ... }))` : `let built = cx.background_executor().spawn(async move { /* cache disque ou `load_source` + `render` + encodage PNG */ }).await;` puis `cx.update(|cx| { /* installe `ascii`/`ascii_key`, vide `pending`, `cx.refresh_windows()` */ })`. Sur `Err`, `tracing::warn!` et vide `pending` sans toucher `ascii`.
   - cache : `{data_dir}/wallpaper-cache/{key}.png`, écriture `tmp` + `rename`. Une texture par palier de 256 px ; la dernière génération du palier gagne.
5. `load_source` : `image::open(path).map(|i| i.to_rgba8()).map_err(|e| e.to_string())`.
6. `should_render` : `ready_key != Some(wanted) && !pending`.
7. `ascii_texture` renvoie le clone de l'`Arc`.

- [ ] **Step 4 : run** les tests → PASS ; `cargo clippy -p komet-ui --all-targets -- -D warnings`.

- [ ] **Step 5 : commit**
```bash
git add crates/ui/src/settings/wallpaper.rs crates/ui/src/settings/wallpaper/ascii.rs
git commit -m "feat(appearance): rasterize the ASCII wallpaper off-thread"
```

---

### Task 5 : Peindre la texture (fenêtre + panneau)

**Files :**
- Modify: `crates/ui/src/settings/wallpaper.rs` (`veiled_image`, `card_layer`, nouveau `image_layer`)
- Modify: `crates/ui/src/shell.rs:1482-1497` (scope Window), `:1644-1661` (scope Panel)
- Modify: `crates/ui/src/theme.rs:776-801` (translucidité : inchangé, mais vérifier que `is_enabled` couvre l'ASCII)

**Interfaces :**
- Consumes : `ascii_texture`, `effect`, `ensure_ascii`.
- Produces : `pub fn image_layer(path: PathBuf, effect: BackgroundEffect, texture: Option<Arc<RenderImage>>) -> AnyElement;` et `card_layer(path, scope, tone, effect, texture)`.

- [ ] **Step 1 : implémenter** `image_layer`

```rust
match (effect, texture) {
    (BackgroundEffect::Ascii(p), Some(tex)) => div().size_full()
        .child(img(tex).size_full().object_fit(ObjectFit::Cover))
        .opacity(p.opacity).into_any_element(),
    _ => img(path).size_full().object_fit(ObjectFit::Cover).into_any_element(),
}
```
`veiled_image` reçoit `AnyElement` au lieu du `path` ; en ASCII, `veil_alpha = 0.0` (l'opacité de l'effet sert de voile), sinon `CARD_SCRIM_ALPHA`.

- [ ] **Step 2 : brancher `shell.rs`** — remplacer `img(path)...` par `wallpaper::image_layer(path, wallpaper::effect(cx), wallpaper::ascii_texture(cx))` (scope Window), et passer `wallpaper::effect(cx)` + `wallpaper::ascii_texture(cx)` à `wallpaper::card_layer(...)` (scope Panel, `:1644-1646`). Puis appeler `wallpaper::ensure_ascii(...)` une fois par render dans `render_ready_page` (avant `panel_wallpaper`), avec :

```rust
let c = Theme::of(cx).accent.to_rgb();                 // Rgba, composantes f32 0..=1
let accent = [(c.r * 255.0) as u8, (c.g * 255.0) as u8, (c.b * 255.0) as u8];
let v = window.viewport_size();
let target = ((f32::from(v.width)) as u32, (f32::from(v.height)) as u32);
wallpaper::ensure_ascii(target, accent, cx);
```

- [ ] **Step 3 : vérifier** `cargo check -p komet-ui` puis `cargo fmt --all`.

- [ ] **Step 4 : manuel** — `scripts/dev-demo.sh`, choisir une image PNG, régler `Background effect` sur ASCII une fois Task 6 faite. En cas de besoin ponctuel, forcer `backgroundEffect` dans `ui-settings.json` :
```json
"backgroundEffect": { "ascii": { "columns": 120, "charset": "classic", "color": "source", "invert": false, "opacity": 0.35 } }
```
Attendu : fond noir + caractères colorés, aucun repaint continu (fenêtre inactive = 0 % CPU).

- [ ] **Step 5 : commit**
```bash
git add crates/ui/src/shell.rs crates/ui/src/settings/wallpaper.rs
git commit -m "feat(appearance): paint the ASCII wallpaper"
```

---

### Task 6 : Contrôles dans Settings › Appearance

**Files :**
- Modify: `crates/ui/src/settings/appearance.rs` (bloc « Wallpaper », `:259-343` et `:369-519`)
- Modify: `crates/ui/src/settings/widgets.rs` (aucun nouveau widget : boutons segmentés inline, comme `scope_row`)

**Interfaces :**
- Consumes : `effect`, `set_effect`, `BackgroundEffect`, `AsciiParams`, `AsciiCharset`, `AsciiColor`, `ascii_texture`, `supports_ascii`.
- Produces : rien (UI feuille).

- [ ] **Step 1 : state de page** — `AppearancePage` garde `picker_task` et `wallpaper_error` ; aucune donnée d'effet en mémoire (tout dans le global).

- [ ] **Step 2 : rendre le sélecteur d'effet**, sous le bloc image, visible seulement si `has_wallpaper` :
  - deux boutons segmentés `None` / `ASCII` (même recette que `scope_row`) ; clic → `set_effect`, erreur dans `wallpaper_error` ;
  - si `Ascii(params)` : ligne `Character set` (Classic/Blocks/Binary), ligne `Colour` (Image colours/Mono/Accent), ligne `Density` (S/M/L → 80/120/160 colonnes), `Invert` via `widgets::toggle_switch`, `Opacity` via 15 %/25 %/35 %/50 % ; chaque clic réécrit `AsciiParams` et appelle `set_effect(Ascii(new), cx)` ;
  - bouton `Reset` → `set_effect(Ascii(AsciiParams::default()), cx)` ;
  - si `!supports_ascii(&path)` : le bouton ASCII est atténué et un texte explique « ASCII needs a PNG or JPEG. »
  - l'aperçu 54 px (`:335-343`) affiche `ascii_texture` quand elle existe : `img(tex).size_full().object_fit(Cover)`.

- [ ] **Step 3 : test** (pur) — `AsciiParams::default().columns == 120`, et chaque preset de densité produit 80/120/160 : extraire `fn density_columns(preset: usize) -> u16` et l'asserter.

- [ ] **Step 4 : vérifier** `cargo check -p komet-ui`, `cargo clippy -p komet-ui --all-targets -- -D warnings`, `scripts/dev-demo.sh` (les contrôles n'apparaissent qu'avec une image ; changer charset/couleur/densité régénère la texture une fois).

- [ ] **Step 5 : commit**
```bash
git add crates/ui/src/settings/appearance.rs
git commit -m "feat(settings): background effect controls"
```

---

### Task 7 : Documentation et vérification finale

**Files :**
- Modify: `docs/PARITY.md` (ligne de statut « wallpapers / background effect »)
- Modify: `ARCHITECTURE.md` § UI si le pipeline de texture mérite une ligne (facultatif, une phrase)

- [ ] **Step 1 : noter** dans `docs/PARITY.md` : image de fond (fenêtre/panneau), effets `None`/`ASCII`, cache disque, formats ASCII PNG/JPEG.
- [ ] **Step 2 : run final**
```bash
cargo fmt --all
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p komet-ui
```
- [ ] **Step 3 : commit**
```bash
git add docs/PARITY.md ARCHITECTURE.md
git commit -m "docs(appearance): background effects"
```

---

## Hors périmètre (nommé, pas oublié)

- Luminosité/contraste/gamma : la rampe et `invert` couvrent le besoin ; à ajouter comme trois champs dans `AsciiParams` si l'utilisateur le demande.
- Scintillement animé, fps, pause hors focus : incompatible avec la contrainte CPU statique.
- Fonds par projet, galerie de fonds intégrés, URL distante, images clair/sombre distinctes.
- Réduction de l'image à l'import (2560 px) : le décodage ASCII passe déjà par une grille de 40–240 colonnes, et `gpui::img` gère le redimensionnement pour `None`.
- WebP/GIF en ASCII : `image` n'est compilé qu'avec `png`/`jpeg` ; élargir les features si le besoin apparaît.
- Élagage du cache disque : les clés sont bornées par (fichier × 6 paliers × jeux de paramètres) ; ajouter un `rm` des entrées de plus de 30 jours si le dossier grossit.
