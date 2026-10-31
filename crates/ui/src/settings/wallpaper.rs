use std::io;
use std::path::PathBuf;

use gpui::{AnyElement, App, Global, Hsla, div, prelude::*, px};

use crate::settings::UiSettings;
use crate::theme::Theme;

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

/// The image under a card-tone veil. gpui clips children to a *rectangle* even
/// when the parent is rounded, so the corner radius is applied to the image and
/// the veil themselves.
fn veiled_image(path: PathBuf, tone: Hsla, scrim_alpha: f32, radius: f32) -> gpui::Div {
    div()
        .size_full()
        .relative()
        .child(
            gpui::img(path)
                .size_full()
                .object_fit(gpui::ObjectFit::Cover)
                .rounded(px(radius)),
        )
        .child(
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
pub fn card_layer(path: PathBuf, scope: WallpaperScope, tone: Hsla) -> Option<AnyElement> {
    match scope {
        WallpaperScope::Window => None,
        WallpaperScope::Panel => Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .child(veiled_image(
                    path,
                    tone,
                    CARD_SCRIM_ALPHA,
                    Theme::PANEL_RADIUS,
                ))
                .into_any_element(),
        ),
    }
}

pub struct WallpaperState {
    pub path: Option<PathBuf>,
    pub scope: WallpaperScope,
    data_dir: PathBuf,
}

impl Global for WallpaperState {}

pub fn init(path: Option<String>, data_dir: PathBuf, cx: &mut App) {
    let scope = UiSettings::load(&data_dir).wallpaper_scope;
    cx.set_global(WallpaperState {
        path: path.map(PathBuf::from),
        scope,
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

pub fn set_path(path: Option<PathBuf>, cx: &mut App) -> io::Result<()> {
    let Some(state) = cx.try_global::<WallpaperState>() else {
        return Err(io::Error::other("wallpaper settings are not initialized"));
    };
    if state.path == path {
        return Ok(());
    }
    let data_dir = state.data_dir.clone();

    let mut settings = UiSettings::load(&data_dir);
    settings.wallpaper_path = path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    settings.save(&data_dir)?;

    cx.global_mut::<WallpaperState>().path = path;
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
