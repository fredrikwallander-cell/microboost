use crate::noise_gate::NoiseGate;
use crossbeam_queue::ArrayQueue;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioMode {
    #[default]
    Boost,
    Bypass,
    Mute,
}

impl AudioMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Boost => "Boost",
            Self::Bypass => "Bypass",
            Self::Mute => "Mute",
        }
    }
    pub fn from_byte(value: u8) -> Self {
        match value {
            0 => Self::Boost,
            1 => Self::Bypass,
            _ => Self::Mute,
        }
    }
}

pub struct AtomicLevel(AtomicU32);
impl AtomicLevel {
    pub fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
pub struct ProcessingSettings {
    pub mode: AudioMode,
    pub gain: f32,
    pub noise_floor: f32,
    pub gate_enabled: bool,
    pub peak_protection: bool,
}

/// Only atomics and bounded queues cross the audio/UI boundary.
pub struct AudioControls {
    pub running: AtomicBool,
    pub mode: AtomicU8,
    pub gain: AtomicLevel,
    pub noise_floor: AtomicLevel,
    pub gate_enabled: AtomicBool,
    pub peak_protection: AtomicBool,
    pub input_rms: AtomicLevel,
    pub output_rms: AtomicLevel,
    pub reduction_db: AtomicLevel,
    pub clipped: AtomicU64,
    pub dropped: AtomicU64,
    pub underruns: AtomicU64,
    pub recording_id: AtomicU64,
    pub recording: ArrayQueue<(u64, f32)>,
    pub recording_dropped: AtomicU64,
}

impl Default for AudioControls {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            mode: AtomicU8::new(0),
            gain: AtomicLevel::new(2.0),
            noise_floor: AtomicLevel::new(0.0),
            gate_enabled: AtomicBool::new(false),
            peak_protection: AtomicBool::new(true),
            input_rms: AtomicLevel::new(0.0),
            output_rms: AtomicLevel::new(0.0),
            reduction_db: AtomicLevel::new(0.0),
            clipped: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            recording_id: AtomicU64::new(0),
            recording: ArrayQueue::new(192000),
            recording_dropped: AtomicU64::new(0),
        }
    }
}

impl AudioControls {
    pub fn settings(&self) -> ProcessingSettings {
        ProcessingSettings {
            mode: AudioMode::from_byte(self.mode.load(Ordering::Acquire)),
            gain: self.gain.get(),
            noise_floor: self.noise_floor.get(),
            gate_enabled: self.gate_enabled.load(Ordering::Relaxed),
            peak_protection: self.peak_protection.load(Ordering::Relaxed),
        }
    }
}

/// Callback-owned state: gate the original signal, then amplify and limit.
pub struct AudioProcessor {
    gate: NoiseGate,
    gain: f32,
    gain_coefficient: f32,
    limiter_gain: f32,
    release_coefficient: f32,
    pub reduction_db: f32,
}

impl AudioProcessor {
    pub fn new(sample_rate: u32, initial_gain: f32) -> Self {
        let mut gate = NoiseGate::new();
        gate.set_sample_rate(sample_rate);
        Self {
            gate,
            gain: initial_gain,
            gain_coefficient: (-1.0 / (sample_rate as f32 * 0.005)).exp(),
            limiter_gain: 1.0,
            release_coefficient: (-1.0 / (sample_rate as f32 * 0.100)).exp(),
            reduction_db: 0.0,
        }
    }

    pub fn process(&mut self, sample: f32, settings: ProcessingSettings) -> f32 {
        let raw = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        if settings.mode == AudioMode::Mute {
            self.reduction_db = 0.0;
            return 0.0;
        }
        if settings.mode == AudioMode::Bypass {
            self.reduction_db = 0.0;
            return raw;
        }
        self.gate
            .restore(settings.noise_floor, settings.gate_enabled, 1.5);
        let gated = self.gate.process(raw);
        self.gain = self.gain_coefficient * self.gain
            + (1.0 - self.gain_coefficient) * settings.gain.clamp(0.1, 10.0);
        let boosted = gated * self.gain;
        if !settings.peak_protection {
            self.limiter_gain = 1.0;
            self.reduction_db = 0.0;
            return boosted.clamp(-1.0, 1.0);
        }
        let desired_gain = if boosted.abs() > 0.95 {
            0.95 / boosted.abs()
        } else {
            1.0
        };
        // Immediate peak protection, gradual recovery: sustained peaks reduce
        // the gain instead of flat-topping every loud waveform sample.
        self.limiter_gain = desired_gain
            .min(self.release_coefficient * self.limiter_gain + (1.0 - self.release_coefficient));
        self.reduction_db = -20.0 * self.limiter_gain.max(0.0001).log10();
        boosted * self.limiter_gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings(mode: AudioMode, gain: f32) -> ProcessingSettings {
        ProcessingSettings {
            mode,
            gain,
            noise_floor: 0.0,
            gate_enabled: false,
            peak_protection: true,
        }
    }
    #[test]
    fn bypass_is_identity_and_mute_is_immediate() {
        let mut processor = AudioProcessor::new(48000, 10.0);
        for sample in [-0.9, -0.1, 0.0, 0.1, 0.9] {
            assert_eq!(
                processor.process(sample, settings(AudioMode::Bypass, 10.0)),
                sample
            );
            assert_eq!(
                processor.process(sample, settings(AudioMode::Mute, 10.0)),
                0.0
            );
        }
    }
    #[test]
    fn protection_can_be_disabled_and_reenabled_live() {
        let mut processor = AudioProcessor::new(48000, 5.0);
        let mut config = settings(AudioMode::Boost, 5.0);
        assert!(processor.process(1.0, config) <= 0.950001);
        config.peak_protection = false;
        assert_eq!(processor.process(1.0, config), 1.0);
        assert_eq!(processor.reduction_db, 0.0);
        assert!((processor.process(0.01, config) - 0.05).abs() < 0.00001);
        config.peak_protection = true;
        assert!(processor.process(-1.0, config).abs() <= 0.950001);
        assert!(processor.reduction_db > 10.0);
    }

    #[test]
    fn limiter_preserves_headroom_and_releases_gradually() {
        let mut processor = AudioProcessor::new(48000, 5.0);
        assert!(
            processor
                .process(1.0, settings(AudioMode::Boost, 5.0))
                .abs()
                <= 0.950001
        );
        assert!(processor.reduction_db > 10.0);
        let recovery = processor.process(0.01, settings(AudioMode::Boost, 5.0));
        assert!(recovery < 0.02);
        for _ in 0..48000 {
            processor.process(0.01, settings(AudioMode::Boost, 5.0));
        }
        assert!(processor.reduction_db < 0.01);
    }
    #[test]
    fn increasing_boost_does_not_open_the_noise_gate() {
        for gain in [1.0, 5.0, 10.0] {
            let mut processor = AudioProcessor::new(48000, gain);
            let config = ProcessingSettings {
                mode: AudioMode::Boost,
                gain,
                noise_floor: 0.01,
                gate_enabled: true,
                peak_protection: true,
            };
            for _ in 0..96000 {
                processor.process(0.01, config);
            }
            assert!(processor.process(0.01, config).abs() < 0.00001);
        }
    }
    #[test]
    fn invalid_samples_cannot_reach_output() {
        let mut processor = AudioProcessor::new(48000, 2.0);
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                processor.process(sample, settings(AudioMode::Boost, 2.0)),
                0.0
            );
        }
    }
}
