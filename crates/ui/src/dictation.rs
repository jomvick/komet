//! Device-local voice model location shared by Settings and the composer.

use std::path::PathBuf;

use gpui::{App, Global};

struct ModelDirectory(PathBuf);

impl Global for ModelDirectory {}

pub(crate) fn init(data_dir: PathBuf, cx: &mut App) {
    cx.set_global(ModelDirectory(komet_voice::model_dir(&data_dir)));
}

pub(crate) fn model_dir(cx: &App) -> PathBuf {
    cx.global::<ModelDirectory>().0.clone()
}

pub(crate) fn installed(cx: &App) -> bool {
    komet_voice::installed(&model_dir(cx))
}
