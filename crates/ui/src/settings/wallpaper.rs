use std::io;
use std::path::PathBuf;

use gpui::{App, Global};

use crate::settings::UiSettings;
use crate::theme::Theme;

pub struct WallpaperState {
    pub path: Option<PathBuf>,
    data_dir: PathBuf,
}

impl Global for WallpaperState {}

pub fn init(path: Option<String>, data_dir: PathBuf, cx: &mut App) {
    cx.set_global(WallpaperState {
        path: path.map(PathBuf::from),
        data_dir,
    });
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
