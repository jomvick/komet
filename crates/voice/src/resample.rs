use anyhow::{Result, ensure};
use rubato::{FftFixedInOut, Resampler};

pub const MODEL_RATE: u32 = 16_000;

pub fn for_model(samples: Vec<f32>, rate: u32) -> Result<Vec<f32>> {
    ensure!(
        (8_000..=192_000).contains(&rate),
        "unsupported microphone sample rate"
    );
    if rate == MODEL_RATE || samples.is_empty() {
        return Ok(samples);
    }
    let length = (samples.len() as u64 * MODEL_RATE as u64 / rate as u64) as usize;
    let mut resampler = FftFixedInOut::<f32>::new(rate as usize, MODEL_RATE as usize, 1024, 1)?;
    let delay = resampler.output_delay();
    let chunk = resampler.input_frames_next();
    let mut input = vec![vec![0.0; chunk]];
    let mut output = resampler.output_buffer_allocate(true);
    let mut converted = Vec::with_capacity(length + delay + resampler.output_frames_max());
    let mut offset = 0;
    while converted.len() < length + delay {
        input[0].fill(0.0);
        let end = (offset + chunk).min(samples.len());
        if offset < end {
            input[0][..end - offset].copy_from_slice(&samples[offset..end]);
        }
        let (_, written) = resampler.process_into_buffer(&input, &mut output, None)?;
        converted.extend_from_slice(&output[0][..written]);
        offset = end;
    }
    converted.drain(..delay);
    converted.truncate(length);
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_preserves_one_second_duration() {
        for rate in [8_000, 16_000, 24_000, 44_100, 48_000, 96_000] {
            let converted = for_model(vec![0.25; rate as usize], rate).unwrap();
            assert_eq!(converted.len(), MODEL_RATE as usize, "{rate}");
        }
    }

    #[test]
    fn unsupported_sample_rates_are_rejected() {
        assert!(for_model(vec![0.0], 0).is_err());
        assert!(for_model(vec![0.0], 192_001).is_err());
    }
}
