use std::sync::{Arc, Mutex};

/// Simple noise gate that learns background noise level then suppresses it.
///
/// Workflow:
/// 1. Call `start_calibration()` — user stays silent for ~3 seconds
/// 2. Audio samples are fed via `feed_calibration()`
/// 3. Call `finish_calibration()` — computes noise floor RMS
/// 4. After calibration, `process()` gates audio below the learned threshold
///
/// The gate uses a smooth attack/release to avoid harsh clicks:
/// - When signal > threshold: gate opens quickly (attack)
/// - When signal < threshold: gate closes slowly (release)

#[derive(Clone)]
pub struct NoiseGate {
    /// Whether the gate is enabled
    pub enabled: bool,
    /// Learned noise floor RMS (linear)
    noise_floor_rms: f32,
    /// Gate threshold = noise_floor_rms * headroom_multiplier
    threshold: f32,
    /// Current gate level (0.0 = closed, 1.0 = open)
    gate_level: f32,
    envelope_sq: f32,
    hold_samples: u32,
    detector_coefficient: f32,
    attack_coefficient: f32,
    release_coefficient: f32,
    hold_length: u32,
    /// How much above noise floor to set the threshold (default 1.5x)
    pub headroom: f32,
}

/// Shared state for calibration (fed from audio thread)
pub struct CalibrationState {
    pub active: bool,
    pub samples: Vec<f32>,
}

impl NoiseGate {
    pub fn new() -> Self {
        Self {
            enabled: false,
            noise_floor_rms: 0.0,
            threshold: 0.0,
            gate_level: 1.0,
            envelope_sq: 0.0,
            hold_samples: 0,
            detector_coefficient: (-1.0_f32 / (48000.0 * 0.010)).exp(),
            attack_coefficient: (-1.0_f32 / (48000.0 * 0.005)).exp(),
            release_coefficient: (-1.0_f32 / (48000.0 * 0.080)).exp(),
            hold_length: 4800,
            headroom: 1.5,
        }
    }

    /// Restore from saved noise floor value
    pub fn restore(&mut self, noise_floor_rms: f32, enabled: bool, headroom: f32) {
        self.noise_floor_rms = if noise_floor_rms.is_finite() {
            noise_floor_rms.max(0.0)
        } else {
            0.0
        };
        self.headroom = if headroom.is_finite() {
            headroom.max(1.0)
        } else {
            1.5
        };
        self.threshold = self.noise_floor_rms * self.headroom;
        self.enabled = enabled;
    }

    /// Get the raw noise floor RMS for saving
    pub fn noise_floor_rms(&self) -> f32 {
        self.noise_floor_rms
    }

    /// Finish calibration: compute noise floor from collected samples
    pub fn finish_calibration(&mut self, samples: &[f32]) -> Result<f32, &'static str> {
        if samples.len() < 4800 {
            return Err("Not enough audio captured");
        }

        // Compute RMS of the noise samples
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        let rms = (sum_sq / samples.len() as f32).sqrt();

        if !rms.is_finite() || rms < 0.00001 {
            return Err("No audio detected — check your microphone");
        }

        self.noise_floor_rms = rms;
        self.threshold = rms * self.headroom;
        self.enabled = true;

        let db = 20.0 * rms.log10();
        Ok(db)
    }

    /// Process a single mono sample through the gate.
    /// Returns the gated sample.
    #[inline]
    pub fn process(&mut self, sample: f32) -> f32 {
        let sample = if sample.is_finite() { sample } else { 0.0 };
        if !self.enabled || self.threshold == 0.0 {
            return sample;
        }

        // Compare an RMS envelope to the raw calibrated RMS, not individual
        // waveform samples. Hold and hysteresis keep quiet syllables intact.
        let detector = self.detector_coefficient;
        self.envelope_sq = detector * self.envelope_sq + (1.0 - detector) * sample * sample;
        let envelope = self.envelope_sq.sqrt();
        if envelope >= self.threshold {
            self.hold_samples = self.hold_length;
        } else if envelope < self.threshold * 0.75 {
            self.hold_samples = self.hold_samples.saturating_sub(1);
        }
        let open = envelope >= self.threshold || self.hold_samples > 0;
        let coefficient = if open {
            self.attack_coefficient
        } else {
            self.release_coefficient
        };
        let target = if open { 1.0 } else { 0.0 };
        self.gate_level = coefficient * self.gate_level + (1.0 - coefficient) * target;

        sample * self.gate_level
    }

    /// Get the current threshold in dBFS
    pub fn threshold_db(&self) -> f32 {
        if self.threshold > 0.0 {
            20.0 * self.threshold.log10()
        } else {
            -80.0
        }
    }

    /// Get the noise floor in dBFS
    pub fn noise_floor_db(&self) -> f32 {
        if self.noise_floor_rms > 0.0 {
            20.0 * self.noise_floor_rms.log10()
        } else {
            -80.0
        }
    }

    /// Whether calibration has been done
    pub fn is_calibrated(&self) -> bool {
        self.noise_floor_rms > 0.0
    }

    /// Reset the gate
    pub fn reset(&mut self) {
        self.enabled = false;
        self.noise_floor_rms = 0.0;
        self.threshold = 0.0;
        self.gate_level = 1.0;
        self.envelope_sq = 0.0;
        self.hold_samples = 0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1) as f32;
        self.detector_coefficient = (-1.0 / (rate * 0.010)).exp();
        self.attack_coefficient = (-1.0 / (rate * 0.005)).exp();
        self.release_coefficient = (-1.0 / (rate * 0.080)).exp();
        self.hold_length = (rate * 0.100) as u32;
    }
}

/// Create shared calibration state
pub fn new_calibration_state() -> Arc<Mutex<CalibrationState>> {
    Arc::new(Mutex::new(CalibrationState {
        active: false,
        samples: Vec::new(),
    }))
}
