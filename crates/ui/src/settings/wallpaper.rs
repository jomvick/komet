use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{AnyElement, App, Global, Hsla, RenderImage, Task, div, prelude::*, px};
use image::Frame;
use sha2::{Digest, Sha256};

use crate::settings::UiSettings;
use crate::theme::Theme;

pub mod ascii;

/// Wallpapers are copied here on import so moving or deleting the source file
/// later never blanks the app.
fn wallpapers_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("wallpapers")
}

/// Copy `src` under `{data_dir}/wallpapers/`, keyed by content hash so picking
/// the same image twice reuses one file. No-op when `src` is already there.
fn import_copy(data_dir: &Path, src: &Path) -> io::Result<PathBuf> {
    let dir = wallpapers_dir(data_dir);
    let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    let canonical_src = src.canonicalize()?;
    if canonical_src.starts_with(&canonical_dir) {
        return Ok(canonical_src);
    }

    let bytes = std::fs::read(&canonical_src)?;
    let digest = Sha256::digest(&bytes);
    let stem: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let ext = canonical_src
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("img")
        .to_ascii_lowercase();
    std::fs::create_dir_all(&dir)?;
    let stored = dir.join(format!("{stem}.{ext}"));
    if !stored.exists() {
        std::fs::write(&stored, &bytes)?;
    }
    Ok(stored)
}

/// Import `new`, persist it, and drop the previous copy. Returns the stored path.
fn persist_path(data_dir: &Path, new: Option<&Path>) -> io::Result<Option<PathBuf>> {
    let mut settings = UiSettings::load(data_dir);
    let old = settings.wallpaper_path.clone().map(PathBuf::from);
    let stored = match new {
        Some(src) => Some(import_copy(data_dir, src)?),
        None => None,
    };
    if let Some(old) = old.as_deref()
        && Some(old) != stored.as_deref()
        && old.starts_with(wallpapers_dir(data_dir))
    {
        // Best-effort: a missing copy is not an error.
        let _ = std::fs::remove_file(old);
    }
    settings.wallpaper_path = stored
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    settings.save(data_dir)?;
    Ok(stored)
}

/// Where the wallpaper is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WallpaperScope {
    /// Behind the whole window — sidebar and titlebar included.
    #[default]
    Window,
    /// Inside the main content card only.
    Panel,
}

impl WallpaperScope {
    pub const ALL: [WallpaperScope; 2] = [Self::Window, Self::Panel];

    pub fn label(self) -> &'static str {
        match self {
            Self::Window => "Whole app",
            Self::Panel => "Main panel",
        }
    }
}

/// Opacity of the card-tone veil laid over the image in [`WallpaperScope::Panel`],
/// so text stays readable. Lower = more visible image.
pub const CARD_SCRIM_ALPHA: f32 = 0.82;

// ---------------------------------------------------------------------------
// Background effect
// ---------------------------------------------------------------------------

pub const DEFAULT_COLUMNS: u16 = 120;
pub const MIN_COLUMNS: u16 = 40;
pub const MAX_COLUMNS: u16 = 240;
pub const DEFAULT_OPACITY: f32 = 0.35;
pub const MIN_OPACITY: f32 = 0.05;

/// Glyph ramp used by the ASCII effect. Ordered darkest → brightest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AsciiCharset {
    #[default]
    Classic,
    Blocks,
    Binary,
}

impl AsciiCharset {
    pub const ALL: [AsciiCharset; 3] = [Self::Classic, Self::Blocks, Self::Binary];

    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Blocks => "Blocks",
            Self::Binary => "Binary",
        }
    }
}

/// Where the glyph colour comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AsciiColor {
    /// The average colour of the source cell.
    #[default]
    Source,
    /// Greyscale, the glyph's own luminance.
    Mono,
    /// The theme accent, scaled by the cell's luminance.
    Accent,
}

impl AsciiColor {
    pub const ALL: [AsciiColor; 3] = [Self::Source, Self::Mono, Self::Accent];

    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "Image colours",
            Self::Mono => "Mono",
            Self::Accent => "Accent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AsciiParams {
    /// Glyph columns across the target width; density follows from it.
    pub columns: u16,
    pub charset: AsciiCharset,
    pub color: AsciiColor,
    pub invert: bool,
    /// Overall strength of the texture (it is painted over black).
    pub opacity: f32,
}

impl Default for AsciiParams {
    fn default() -> Self {
        Self {
            columns: DEFAULT_COLUMNS,
            charset: AsciiCharset::default(),
            color: AsciiColor::default(),
            invert: false,
            opacity: DEFAULT_OPACITY,
        }
    }
}

impl AsciiParams {
    pub fn clamped(mut self) -> Self {
        self.columns = self.columns.clamp(MIN_COLUMNS, MAX_COLUMNS);
        self.opacity = if self.opacity.is_finite() {
            self.opacity.clamp(MIN_OPACITY, 1.0)
        } else {
            DEFAULT_OPACITY
        };
        self
    }
}

/// What is painted over the wallpaper image. `None` keeps the image itself.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundEffect {
    #[default]
    None,
    Ascii(AsciiParams),
}

impl BackgroundEffect {
    pub fn clamped(self) -> Self {
        match self {
            Self::Ascii(params) => Self::Ascii(params.clamped()),
            Self::None => Self::None,
        }
    }

    pub fn is_ascii(self) -> bool {
        matches!(self, Self::Ascii(_))
    }
}

/// The image, or its ASCII texture, as a full-bleed cover layer. `radius` is
/// applied to the image itself because gpui clips to a rectangle, not a curve.
pub fn image_layer(
    path: PathBuf,
    effect: BackgroundEffect,
    texture: Option<Arc<RenderImage>>,
    radius: f32,
) -> AnyElement {
    match (effect, texture) {
        (BackgroundEffect::Ascii(params), Some(texture)) => div()
            .size_full()
            .child(
                gpui::img(texture)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .rounded(px(radius)),
            )
            .opacity(params.opacity)
            .into_any_element(),
        _ => gpui::img(path)
            .size_full()
            .object_fit(gpui::ObjectFit::Cover)
            .rounded(px(radius))
            .into_any_element(),
    }
}

/// The image under a card-tone veil. gpui clips children to a *rectangle* even
/// when the parent is rounded, so the corner radius is applied to the image and
/// the veil themselves.
fn veiled_image(source: AnyElement, tone: Hsla, scrim_alpha: f32, radius: f32) -> gpui::Div {
    div().size_full().relative().child(source).child(
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            .rounded(px(radius))
            .bg(tone.opacity(scrim_alpha)),
    )
}

/// The wallpaper layer for [`WallpaperScope::Panel`], to be the FIRST child of
/// the main card (under its content, clipped by its rounded corners). `None` for
/// `Window`, which paints behind the whole window.
pub fn card_layer(
    path: PathBuf,
    scope: WallpaperScope,
    tone: Hsla,
    effect: BackgroundEffect,
    texture: Option<Arc<RenderImage>>,
) -> Option<AnyElement> {
    match scope {
        WallpaperScope::Window => None,
        WallpaperScope::Panel => {
            // The ASCII texture carries its own black backing, so the veil
            // would only wash the glyphs out; the effect's opacity is the scrim.
            let scrim = if effect.is_ascii() {
                0.0
            } else {
                CARD_SCRIM_ALPHA
            };
            Some(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(veiled_image(
                        image_layer(path, effect, texture, Theme::PANEL_RADIUS),
                        tone,
                        scrim,
                        Theme::PANEL_RADIUS,
                    ))
                    .into_any_element(),
            )
        }
    }
}

pub struct WallpaperState {
    pub path: Option<PathBuf>,
    pub scope: WallpaperScope,
    pub effect: BackgroundEffect,
    /// The rasterised ASCII texture, once a job has completed.
    ascii: Option<Arc<RenderImage>>,
    /// Cache key of `ascii` — one texture per (image, params, size bucket).
    ascii_key: Option<String>,
    /// Debounces regeneration: at most one job in flight.
    pending: Option<Task<()>>,
    data_dir: PathBuf,
}

impl Global for WallpaperState {}

pub fn init(path: Option<String>, effect: BackgroundEffect, data_dir: PathBuf, cx: &mut App) {
    let scope = UiSettings::load(&data_dir).wallpaper_scope;
    cx.set_global(WallpaperState {
        path: path.map(PathBuf::from),
        scope,
        effect,
        ascii: None,
        ascii_key: None,
        pending: None,
        data_dir,
    });
}

pub fn scope(cx: &App) -> WallpaperScope {
    cx.try_global::<WallpaperState>()
        .map(|state| state.scope)
        .unwrap_or_default()
}

pub fn path(cx: &App) -> Option<PathBuf> {
    cx.try_global::<WallpaperState>()
        .and_then(|state| state.path.clone())
}

pub fn is_enabled(cx: &App) -> bool {
    path(cx).is_some()
}

pub fn effect(cx: &App) -> BackgroundEffect {
    cx.try_global::<WallpaperState>()
        .map(|state| state.effect)
        .unwrap_or_default()
}

pub fn ascii_texture(cx: &App) -> Option<Arc<RenderImage>> {
    cx.try_global::<WallpaperState>()
        .and_then(|state| state.ascii.clone())
}

/// Switch the effect and persist it. Rejects ASCII for formats the decoder
/// cannot read, leaving the stored effect untouched.
pub fn set_effect(effect: BackgroundEffect, cx: &mut App) -> io::Result<()> {
    let Some(state) = cx.try_global::<WallpaperState>() else {
        return Err(io::Error::other("wallpaper settings are not initialized"));
    };
    if state.effect == effect {
        return Ok(());
    }
    if effect.is_ascii() {
        match state.path.as_deref() {
            Some(path) if ascii::supports_ascii(path) => {}
            Some(_) => {
                return Err(io::Error::other("ASCII needs a PNG or JPEG wallpaper"));
            }
            None => return Err(io::Error::other("Choose a wallpaper first")),
        }
    }
    let data_dir = state.data_dir.clone();

    let mut settings = UiSettings::load(&data_dir);
    settings.background_effect = effect;
    settings.save(&data_dir)?;

    let state = cx.global_mut::<WallpaperState>();
    state.effect = effect;
    // Drop any texture and cancel an in-flight job: they belong to the old effect.
    state.ascii = None;
    state.ascii_key = None;
    state.pending = None;
    cx.refresh_windows();
    Ok(())
}

/// Ask for the ASCII texture at `target` if the current one is stale. Called
/// from the shell render; idempotent, and never queues a second job.
pub fn ensure_ascii(target: (u32, u32), accent: [u8; 3], cx: &mut App) {
    let Some(state) = cx.try_global::<WallpaperState>() else {
        return;
    };
    let BackgroundEffect::Ascii(params) = state.effect else {
        return;
    };
    let Some(path) = state.path.clone().filter(|path| path.is_file()) else {
        return;
    };
    if !ascii::supports_ascii(&path) {
        return;
    }
    let key = ascii::cache_key(&path, &params, target);
    if !ascii::should_render(state.ascii_key.as_deref(), state.pending.is_some(), &key) {
        return;
    }
    let data_dir = state.data_dir.clone();
    let cache = ascii::cache_path(&data_dir, &key);

    let task = cx.spawn(async move |cx| {
        let built = cx
            .background_executor()
            .spawn(async move { build_texture(&path, &cache, &params, target, accent) })
            .await;
        cx.update(|cx| {
            let Some(state) = cx.try_global::<WallpaperState>() else {
                return;
            };
            // The effect may have changed while this ran; a stale texture is
            // dropped rather than painted.
            if !matches!(state.effect, BackgroundEffect::Ascii(_)) {
                cx.global_mut::<WallpaperState>().pending = None;
                return;
            }
            let state = cx.global_mut::<WallpaperState>();
            state.pending = None;
            match built {
                Ok(image) => {
                    state.ascii = Some(Arc::new(RenderImage::new(vec![Frame::new(image)])));
                    state.ascii_key = Some(key);
                }
                Err(error) => tracing::warn!(%error, "could not render the ASCII wallpaper"),
            }
            cx.refresh_windows();
        });
    });
    cx.global_mut::<WallpaperState>().pending = Some(task);
}

/// Disk cache first, then decode + rasterise + store. Runs off the UI thread.
fn build_texture(
    path: &Path,
    cache: &Path,
    params: &AsciiParams,
    target: (u32, u32),
    accent: [u8; 3],
) -> Result<image::RgbaImage, String> {
    if let Some(cached) = ascii::load_cached(cache) {
        return Ok(cached);
    }
    let source = ascii::load_source(path)?;
    let out = ascii::render(&source, params, target, accent);
    ascii::store_cached(cache, &out);
    Ok(out)
}

pub fn set_path(path: Option<PathBuf>, cx: &mut App) -> io::Result<()> {
    let Some(state) = cx.try_global::<WallpaperState>() else {
        return Err(io::Error::other("wallpaper settings are not initialized"));
    };
    if state.path == path {
        return Ok(());
    }
    let data_dir = state.data_dir.clone();

    let stored = persist_path(&data_dir, path.as_deref())?;

    cx.global_mut::<WallpaperState>().path = stored;
    // A new image invalidates the texture; the next render regenerates it.
    let state = cx.global_mut::<WallpaperState>();
    state.ascii = None;
    state.ascii_key = None;
    state.pending = None;
    let appearance = Theme::of(cx).appearance;
    Theme::install(appearance, cx);
    cx.refresh_windows();
    Ok(())
}
/// Change where the wallpaper is painted and persist it. The theme is
/// re-installed because only the whole-window scope makes the shell surfaces
/// translucent.
pub fn set_scope(scope: WallpaperScope, cx: &mut App) -> io::Result<()> {
    let Some(state) = cx.try_global::<WallpaperState>() else {
        return Err(io::Error::other("wallpaper settings are not initialized"));
    };
    if state.scope == scope {
        return Ok(());
    }
    let data_dir = state.data_dir.clone();

    let mut settings = UiSettings::load(&data_dir);
    settings.wallpaper_scope = scope;
    settings.save(&data_dir)?;

    cx.global_mut::<WallpaperState>().scope = scope;
    let appearance = Theme::of(cx).appearance;
    Theme::install(appearance, cx);
    cx.refresh_windows();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn import_copy_moves_the_image_under_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let src = fixture(dir.path(), "a.png", b"png-bytes");
        let stored = import_copy(dir.path(), &src).unwrap();
        assert!(stored.starts_with(dir.path().join("wallpapers")));
        assert_eq!(std::fs::read(&stored).unwrap(), b"png-bytes");
        assert_eq!(
            import_copy(dir.path(), &stored).unwrap(),
            stored,
            "import is idempotent"
        );
    }

    #[test]
    fn persisting_a_new_wallpaper_drops_the_previous_copy() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "a.png", b"aaa");
        let b = fixture(dir.path(), "b.png", b"bbb");

        let first = persist_path(dir.path(), Some(&a)).unwrap().unwrap();
        let second = persist_path(dir.path(), Some(&b)).unwrap().unwrap();

        assert!(!first.exists(), "the replaced copy is deleted");
        assert!(second.exists());
        assert_eq!(
            UiSettings::load(dir.path()).wallpaper_path.as_deref(),
            Some(second.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn removing_the_wallpaper_deletes_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "a.png", b"aaa");
        let stored = persist_path(dir.path(), Some(&a)).unwrap().unwrap();
        assert_eq!(persist_path(dir.path(), None).unwrap(), None);
        assert!(!stored.exists());
        assert_eq!(UiSettings::load(dir.path()).wallpaper_path, None);
    }
}
