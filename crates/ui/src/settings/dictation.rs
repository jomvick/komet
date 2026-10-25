use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use gpui::{Context, EventEmitter, SharedString, Task, Window, div, prelude::*, px};

use crate::{dictation, icons, settings::widgets, theme::Theme};

pub enum DictationEvent {
    Changed,
}

pub struct DictationPage {
    directory: std::path::PathBuf,
    ready: bool,
    cancel: Option<Arc<AtomicBool>>,
    progress: Arc<AtomicU64>,
    error: Option<String>,
    task: Option<Task<()>>,
}

impl EventEmitter<DictationEvent> for DictationPage {}

impl DictationPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let directory = dictation::model_dir(cx);
        Self {
            ready: komet_voice::installed(&directory),
            directory,
            cancel: None,
            progress: Arc::new(AtomicU64::new(0)),
            error: None,
            task: None,
        }
    }

    fn start_download(&mut self, cx: &mut Context<Self>) {
        if self.cancel.is_some() {
            return;
        }
        self.error = None;
        self.progress.store(0, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let progress = self.progress.clone();
        let worker_progress = progress.clone();
        let directory = self.directory.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let spawn = std::thread::Builder::new()
            .name("komet-voice-download".into())
            .spawn(move || {
                let result = komet_voice::download(&directory, &worker_cancel, |bytes| {
                    worker_progress.store(bytes, Ordering::Relaxed);
                })
                .map_err(|err| format!("{err:#}"));
                let _ = sender.send(result);
            });
        if let Err(err) = spawn {
            self.error = Some(format!("Could not start model download: {err}"));
            cx.notify();
            return;
        }
        self.cancel = Some(cancel);
        self.task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let result = receiver.try_recv();
                let done = !matches!(result, Err(std::sync::mpsc::TryRecvError::Empty));
                if this
                    .update(cx, |page, cx| {
                        match result {
                            Ok(Ok(())) => {
                                page.ready = true;
                                page.cancel = None;
                                page.error = None;
                            }
                            Ok(Err(err)) => {
                                page.ready = komet_voice::installed(&page.directory);
                                page.cancel = None;
                                page.error = Some(err);
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                page.cancel = None;
                                page.error = Some("Model download was interrupted.".into());
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        }
                        cx.notify();
                    })
                    .is_err()
                    || done
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn remove_model(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = komet_voice::unload() {
            self.error = Some(err.to_string());
            cx.notify();
            return;
        }
        match std::fs::remove_dir_all(&self.directory) {
            Ok(()) => {
                self.ready = false;
                self.error = None;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.ready = false;
                self.error = None;
            }
            Err(err) => {
                self.error = Some(format!("Could not remove the model: {err}"));
            }
        }
        cx.notify();
    }
}

impl Render for DictationPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let downloading = self.cancel.is_some();
        let progress = self.progress.load(Ordering::Relaxed);
        let progress_label = format!(
            "Downloading Parakeet model: {:.0} / {:.0} MB",
            progress as f64 / 1_000_000.0,
            komet_voice::download_size() as f64 / 1_000_000.0
        );
        let action_label = if downloading {
            "Cancel download"
        } else if self.ready {
            "Remove model"
        } else {
            "Download model (about 670 MB)"
        };
        let action = div()
            .id("dictation-model-action")
            .px(px(14.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .bg(theme.input_bg)
            .border_1()
            .border_color(theme.border)
            .text_size(px(12.0))
            .text_color(theme.text)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(cancel) = &this.cancel {
                    cancel.store(true, Ordering::Release);
                } else if this.ready {
                    this.remove_model(cx);
                } else {
                    this.start_download(cx);
                }
            }))
            .child(SharedString::from(action_label));
        let info = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::MICROPHONE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Offline dictation"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Audio is processed on this device and is never sent to an agent or synced. \
                                             Hold the composer microphone to record; release to transcribe. \
                                             Recordings are limited to 60 seconds.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    ),
            )
            .child(
                div()
                    .px(px(20.0))
                    .py(px(16.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(if downloading {
                                progress_label
                            } else if self.ready {
                                "Parakeet model installed and verified.".into()
                            } else {
                                "The model is downloaded only after you choose to install it.".into()
                            })),
                    )
                    .child(action),
            );
        let error = self.error.as_ref().map(|message| {
            div()
                .mt(px(12.0))
                .text_size(px(12.0))
                .text_color(theme.danger)
                .child(SharedString::from(message.clone()))
        });
        div()
            .w_full()
            .max_w(px(680.0))
            .mx_auto()
            .px(px(32.0))
            .py(px(28.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_size(px(20.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child("Dictation"),
            )
            .child(info)
            .children(error)
    }
}
