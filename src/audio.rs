//! Sample-format adapters. Conversion uses fixed stack storage, never callback allocations.
use cpal::traits::DeviceTrait;
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, SupportedStreamConfig};

pub struct Playback {
    samples: Vec<f32>,
    position: f64,
    ratio: f64,
}
impl Playback {
    pub fn new(samples: Vec<f32>, input_rate: u32, output_rate: u32) -> Self {
        assert!(input_rate > 0 && output_rate > 0);
        Self {
            samples,
            position: 0.0,
            ratio: input_rate as f64 / output_rate as f64,
        }
    }
    pub fn next_sample(&mut self) -> Option<f32> {
        let index = self.position as usize;
        let first = *self.samples.get(index)?;
        let second = self.samples.get(index + 1).copied().unwrap_or(first);
        let sample = first + (second - first) * self.position.fract() as f32;
        self.position += self.ratio;
        Some(sample)
    }
}

fn input<T, F, E>(
    device: &Device,
    supported: &SupportedStreamConfig,
    mut callback: F,
    error: E,
) -> Result<Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
    F: FnMut(&[f32]) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let channels = supported.channels() as usize;
    if channels == 0 || channels > 4096 {
        return Err("Unsupported channel count".into());
    }
    device
        .build_input_stream(
            &supported.clone().into(),
            move |data: &[T], _| {
                let mut converted = [0.0_f32; 4096];
                let block_size = converted.len() / channels * channels;
                for block in data.chunks(block_size) {
                    for (target, sample) in converted.iter_mut().zip(block) {
                        *target = sample.to_sample::<f32>();
                    }
                    callback(&converted[..block.len()]);
                }
            },
            error,
            None,
        )
        .map_err(|e| e.to_string())
}

pub fn build_input_stream<F, E>(
    device: &Device,
    config: &SupportedStreamConfig,
    callback: F,
    error: E,
) -> Result<Stream, String>
where
    F: FnMut(&[f32]) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    macro_rules! dispatch { ($($variant:ident => $ty:ty),*) => { match config.sample_format() {
        $(SampleFormat::$variant => input::<$ty, _, _>(device, config, callback, error),)*
        other => Err(format!("Unsupported input format: {other:?}")),
    } }; }
    dispatch!(I8 => i8, I16 => i16, I24 => cpal::I24, I32 => i32, I64 => i64,
        U8 => u8, U16 => u16, U32 => u32, U64 => u64, F32 => f32, F64 => f64)
}

fn output<T, F, E>(
    device: &Device,
    supported: &SupportedStreamConfig,
    mut callback: F,
    error: E,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
    F: FnMut() -> f32 + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let channels = supported.channels() as usize;
    if channels == 0 {
        return Err("Unsupported channel count".into());
    }
    device
        .build_output_stream(
            &supported.clone().into(),
            move |data: &mut [T], _| {
                for frame in data.chunks_mut(channels) {
                    let sample = callback();
                    let sample = if sample.is_finite() {
                        sample.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    for target in frame {
                        *target = T::from_sample(sample);
                    }
                }
            },
            error,
            None,
        )
        .map_err(|e| e.to_string())
}

pub fn build_output_stream<F, E>(
    device: &Device,
    config: &SupportedStreamConfig,
    callback: F,
    error: E,
) -> Result<Stream, String>
where
    F: FnMut() -> f32 + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    macro_rules! dispatch { ($($variant:ident => $ty:ty),*) => { match config.sample_format() {
        $(SampleFormat::$variant => output::<$ty, _, _>(device, config, callback, error),)*
        other => Err(format!("Unsupported output format: {other:?}")),
    } }; }
    dispatch!(I8 => i8, I16 => i16, I24 => cpal::I24, I32 => i32, I64 => i64,
        U8 => u8, U16 => u16, U32 => u32, U64 => u64, F32 => f32, F64 => f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::Sample;
    #[test]
    fn playback_uses_the_recorded_sample_rate() {
        let mut playback = Playback::new(vec![0.0, 0.5, 1.0], 24000, 48000);
        let mut output = Vec::new();
        while let Some(sample) = playback.next_sample() {
            output.push(sample);
        }
        assert_eq!(output, [0.0, 0.25, 0.5, 0.75, 1.0, 1.0]);
    }
    #[test]
    fn integer_samples_use_normalized_audio_and_unsigned_silence() {
        assert_eq!(i16::MIN.to_sample::<f32>(), -1.0);
        assert_eq!(0_i16.to_sample::<f32>(), 0.0);
        assert_eq!(32768_u16.to_sample::<f32>(), 0.0);
        assert_eq!(u16::from_sample(0.0_f32), 32768);
        assert!((i32::MAX.to_sample::<f32>() - 1.0).abs() < 0.00001);
    }
}
