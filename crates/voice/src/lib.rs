//! Optional, device-local Parakeet transcription. Audio is captured and
//! transcribed in this process; only the resulting text is returned to callers.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parakeet_rs::{ParakeetTDT, Transcriber};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

mod resample;

pub const MAX_SECONDS: usize = 60;
const MODEL_ID: &str = "parakeet-tdt-0.6b-v3-int8";

#[derive(serde::Deserialize)]
pub struct Artifact {
    name: String,
    size: u64,
    sha256: String,
}

#[derive(serde::Deserialize)]
struct Manifest {
    repository: String,
    revision: String,
    files: Vec<Artifact>,
}

fn manifest() -> Manifest {
    serde_json::from_str(include_str!("../model.json")).expect("pinned model manifest")
}

pub fn model_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models").join(MODEL_ID)
}

pub fn download_size() -> u64 {
    manifest().files.iter().map(|file| file.size).sum()
}

pub fn installed(dir: &Path) -> bool {
    let model = manifest();
    model.files.iter().all(|file| {
        std::fs::metadata(dir.join(&file.name)).is_ok_and(|metadata| metadata.len() == file.size)
    }) && std::fs::read_to_string(dir.join("verified"))
        .is_ok_and(|revision| revision == model.revision)
}

/// Download the pinned model to staging files and verify each checksum before
/// marking it usable. Intended to run on a worker thread.
pub fn download(
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        std::fs::create_dir_all(dir)?;
        let model = manifest();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        let mut total = 0_u64;
        for file in &model.files {
            if cancel.load(Ordering::Acquire) {
                bail!("model download cancelled");
            }
            let staging = dir.join(format!("{}.part", file.name));
            let result = async {
                let url = format!(
                    "https://huggingface.co/{}/resolve/{}/{}",
                    model.repository, model.revision, file.name
                );
                let mut response = tokio::select! {
                    _ = cancelled(cancel) => bail!("model download cancelled"),
                    response = tokio::time::timeout(Duration::from_secs(30), client.get(url).send()) => {
                        response??.error_for_status()?
                    }
                };
                let mut output = std::fs::File::create(&staging)?;
                let mut hash = Sha256::new();
                let mut size = 0_u64;
                loop {
                    let chunk = tokio::select! {
                        _ = cancelled(cancel) => bail!("model download cancelled"),
                        chunk = tokio::time::timeout(Duration::from_secs(30), response.chunk()) => chunk??,
                    };
                    let Some(chunk) = chunk else { break };
                    size += chunk.len() as u64;
                    if size > file.size {
                        bail!("unexpected model artifact size");
                    }
                    hash.update(&chunk);
                    std::io::Write::write_all(&mut output, &chunk)?;
                    total += chunk.len() as u64;
                    progress(total);
                }
                if size != file.size || format!("{:x}", hash.finalize()) != file.sha256 {
                    bail!("model checksum verification failed");
                }
                output.sync_all()?;
                std::fs::rename(&staging, dir.join(&file.name))?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if result.is_err() {
                let _ = std::fs::remove_file(staging);
            }
            result?;
        }
        if cancel.load(Ordering::Acquire) {
            bail!("model download cancelled");
        }
        std::fs::write(dir.join("verified"), model.revision)?;
        Ok(())
    })
}

async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Recognizer(ParakeetTDT);

impl Recognizer {
    fn load(dir: &Path) -> Result<Self> {
        let model = manifest();
        if !installed(dir) {
            bail!("Parakeet model is incomplete; download it again in Settings → Dictation");
        }
        for file in model.files {
            let mut input = std::fs::File::open(dir.join(&file.name))?;
            let mut hash = Sha256::new();
            std::io::copy(&mut input, &mut hash)?;
            if format!("{:x}", hash.finalize()) != file.sha256 {
                bail!("Parakeet model checksum failed; remove and download it again");
            }
        }
        Ok(Self(
            ParakeetTDT::from_pretrained(dir, None)
                .map_err(|_| anyhow::anyhow!("could not load Parakeet v3"))?,
        ))
    }

    fn transcribe(&mut self, samples: Vec<f32>, rate: u32) -> Result<String> {
        if !(8_000..=192_000).contains(&rate) || samples.len() > rate as usize * MAX_SECONDS {
            bail!("unsupported microphone recording");
        }
        if samples.len() < rate as usize / 5 || samples.iter().all(|sample| sample.abs() < 0.0001)
        {
            return Ok(String::new());
        }
        let samples = resample::for_model(samples, rate)?;
        self.0
            .transcribe_samples(samples, resample::MODEL_RATE, 1, None)
            .map(|result| result.text)
            .map_err(|_| anyhow::anyhow!("could not transcribe this recording"))
    }
}

struct CachedRecognizer {
    path: PathBuf,
    recognizer: Recognizer,
    last_used: Instant,
}

static RECOGNIZER: OnceLock<Mutex<Option<CachedRecognizer>>> = OnceLock::new();
static CACHE_REAPER: OnceLock<()> = OnceLock::new();

fn cached_transcribe(dir: &Path, samples: Vec<f32>, rate: u32) -> Result<String> {
    CACHE_REAPER.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("komet-voice-model-reaper".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_secs(10));
                let cache = RECOGNIZER.get_or_init(|| Mutex::new(None));
                let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
                if cache
                    .as_ref()
                    .is_some_and(|model| model.last_used.elapsed() >= Duration::from_secs(30))
                {
                    *cache = None;
                }
            });
    });
    let cache = RECOGNIZER.get_or_init(|| Mutex::new(None));
    let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
    if cache.as_ref().is_none_or(|model| model.path != dir) {
        *cache = Some(CachedRecognizer {
            path: dir.to_path_buf(),
            recognizer: Recognizer::load(dir)?,
            last_used: Instant::now(),
        });
    }
    let model = cache.as_mut().expect("recognizer loaded above");
    let result = model.recognizer.transcribe(samples, rate);
    model.last_used = Instant::now();
    result
}

pub fn unload() -> Result<()> {
    if busy() {
        bail!("stop dictation before removing its model");
    }
    if let Some(cache) = RECOGNIZER.get() {
        *cache.lock().unwrap_or_else(|err| err.into_inner()) = None;
    }
    Ok(())
}

struct Audio {
    samples: Mutex<Vec<f32>>,
    failed: AtomicBool,
    full: AtomicBool,
}

impl Audio {
    fn new(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(capacity)),
            failed: AtomicBool::new(false),
            full: AtomicBool::new(false),
        }
    }
}

fn append<T: cpal::Sample>(data: &[T], channels: usize, rate: u32, audio: &Audio)
where
    f32: cpal::FromSample<T>,
{
    let Ok(mut samples) = audio.samples.try_lock() else {
        audio.failed.store(true, Ordering::Release);
        return;
    };
    for frame in data.chunks_exact(channels.max(1)) {
        if samples.len() >= rate as usize * MAX_SECONDS {
            audio.full.store(true, Ordering::Release);
            break;
        }
        let mono = frame.iter().map(|sample| sample.to_sample::<f32>()).sum::<f32>()
            / frame.len() as f32;
        samples.push(mono);
    }
}

pub struct Capture {
    stream: Option<cpal::Stream>,
    audio: Arc<Audio>,
    rate: u32,
}

impl Capture {
    fn start() -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .context("no microphone available")?;
        let config = device.default_input_config()?;
        let rate = config.sample_rate();
        let channels = config.channels() as usize;
        let audio = Arc::new(Audio::new(rate as usize * MAX_SECONDS));
        let callback_audio = audio.clone();
        let error_audio = audio.clone();
        let error_callback = move |_| error_audio.failed.store(true, Ordering::Release);
        macro_rules! stream {
            ($sample:ty) => {
                device.build_input_stream(
                    &config.clone().into(),
                    move |data: &[$sample], _| append(data, channels, rate, &callback_audio),
                    error_callback,
                    None,
                )?
            };
        }
        let stream = match config.sample_format() {
            cpal::SampleFormat::I8 => stream!(i8),
            cpal::SampleFormat::I16 => stream!(i16),
            cpal::SampleFormat::I24 => stream!(cpal::I24),
            cpal::SampleFormat::I32 => stream!(i32),
            cpal::SampleFormat::I64 => stream!(i64),
            cpal::SampleFormat::U8 => stream!(u8),
            cpal::SampleFormat::U16 => stream!(u16),
            cpal::SampleFormat::U24 => stream!(cpal::U24),
            cpal::SampleFormat::U32 => stream!(u32),
            cpal::SampleFormat::U64 => stream!(u64),
            cpal::SampleFormat::F32 => stream!(f32),
            cpal::SampleFormat::F64 => stream!(f64),
            _ => bail!("unsupported microphone sample format"),
        };
        stream.play()?;
        Ok(Self {
            stream: Some(stream),
            audio,
            rate,
        })
    }

    fn ended(&self) -> bool {
        self.audio.full.load(Ordering::Acquire) || self.audio.failed.load(Ordering::Acquire)
    }

    fn finish(mut self) -> Result<(Vec<f32>, u32)> {
        self.stream.take();
        if self.audio.failed.load(Ordering::Acquire) {
            bail!("microphone recording failed; your draft is safe");
        }
        let mut samples = self
            .audio
            .samples
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        Ok((std::mem::take(&mut *samples), self.rate))
    }
}

#[derive(Debug)]
pub enum Event {
    Listening,
    Finalizing,
    Final(String),
    Failed(String),
}

static BUSY: AtomicBool = AtomicBool::new(false);

struct BusyGuard;

impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    events: std::sync::mpsc::Receiver<Event>,
}

impl Session {
    pub fn start(dir: PathBuf) -> Result<Self> {
        if BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            bail!("dictation is already active");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, events) = std::sync::mpsc::sync_channel(4);
        let worker_stop = stop.clone();
        let worker_cancel = cancel.clone();
        let result = std::thread::Builder::new()
            .name("komet-dictation".into())
            .spawn(move || {
                let _busy = BusyGuard;
                let result = std::panic::catch_unwind(|| -> Result<Option<String>> {
                    if worker_cancel.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    let capture = Capture::start()?;
                    if worker_cancel.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    let _ = sender.try_send(Event::Listening);
                    let started = Instant::now();
                    while !worker_stop.load(Ordering::Acquire)
                        && !worker_cancel.load(Ordering::Acquire)
                        && !capture.ended()
                        && started.elapsed() < Duration::from_secs(MAX_SECONDS as u64)
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if worker_cancel.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    let (samples, rate) = capture.finish()?;
                    let _ = sender.try_send(Event::Finalizing);
                    let text = cached_transcribe(&dir, samples, rate)?;
                    Ok(Some(text))
                });
                match result {
                    Ok(Ok(Some(text))) if !worker_cancel.load(Ordering::Acquire) => {
                        let _ = sender.try_send(Event::Final(text));
                    }
                    Ok(Err(err)) => {
                        let _ = sender.try_send(Event::Failed(format!("{err:#}")));
                    }
                    Err(_) => {
                        let _ = sender.try_send(Event::Failed(
                            "dictation stopped unexpectedly; your draft is safe".into(),
                        ));
                    }
                    _ => {}
                }
            });
        if let Err(err) = result {
            BUSY.store(false, Ordering::Release);
            return Err(err.into());
        }
        Ok(Self {
            stop,
            cancel,
            events,
        })
    }

    pub fn finish(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn poll(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub fn busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_pinned_to_expected_model_artifacts() {
        let model = manifest();
        assert_eq!(model.revision.len(), 40);
        assert_eq!(model.files.len(), 3);
        assert_eq!(download_size(), 670_479_942);
        assert!(model.files.iter().all(|file| {
            file.sha256.len() == 64 && !file.name.contains('/') && file.size > 0
        }));
    }

    #[test]
    fn cancelled_download_does_not_mark_model_installed() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(true);
        assert!(download(dir.path(), &cancel, |_| {}).is_err());
        assert!(!installed(dir.path()));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn installed_requires_all_files_and_verification_marker() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!installed(dir.path()));
    }
}
