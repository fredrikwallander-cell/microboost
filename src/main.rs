#![windows_subsystem = "windows"]

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use eframe::egui;
use microboost::{
    audio, noise_gate,
    processing::{AudioControls, AudioMode, AudioProcessor},
    Resampler, SpscRing, RING_SIZE,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
mod desktop;

/// Device profiles — saved per microphone name
mod profiles {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn profiles_path() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Microboost").join("profiles.json")
    }

    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Profile {
        pub boost: u32,
        #[serde(default)]
        pub noise_floor_rms: Option<f32>,
        #[serde(default)]
        pub noise_gate_enabled: bool,
    }

    /// Map from device name -> Profile
    pub type ProfileMap = HashMap<String, Profile>;

    pub fn load() -> ProfileMap {
        let path = profiles_path();
        if let Ok(data) = std::fs::read_to_string(&path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    pub fn save(map: &ProfileMap) {
        let path = profiles_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(json) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn settings_path() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Microboost").join("settings.json")
    }

    #[derive(Serialize, Deserialize, Clone)]
    #[serde(default)]
    pub struct Settings {
        pub last_input_device: Option<String>,
        pub startup_mode: microboost::processing::AudioMode,
        pub close_to_tray: bool,
        pub peak_protection: bool,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                last_input_device: None,
                startup_mode: microboost::processing::AudioMode::Boost,
                close_to_tray: false,
                peak_protection: true,
            }
        }
    }

    pub fn load_settings() -> Settings {
        let path = settings_path();
        if let Ok(data) = std::fs::read_to_string(&path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Settings::default()
        }
    }

    pub fn save_settings(settings: &Settings) {
        let path = settings_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(json) = serde_json::to_string_pretty(settings) {
            let _ = std::fs::write(&path, json);
        }
    }
}

/// VB-CABLE auto-setup
mod vbcable {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn app_dir() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Microboost")
    }

    /// Check if VB-CABLE is installed by looking for its devices
    pub fn is_installed() -> bool {
        let host = cpal::default_host();
        use cpal::traits::HostTrait;
        if let Ok(devs) = host.output_devices() {
            use cpal::traits::DeviceTrait;
            for d in devs {
                if let Ok(name) = d.name() {
                    if name.to_lowercase().contains("cable input") {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Download VB-CABLE zip
    pub fn download(status: &Arc<Mutex<String>>) -> Result<PathBuf, String> {
        let dir = app_dir();
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join("VBCABLE_Driver_Pack.zip");

        // If already downloaded, skip
        if zip_path.exists() {
            let meta = std::fs::metadata(&zip_path).ok();
            if meta.map(|m| m.len() > 100_000).unwrap_or(false) {
                return Ok(zip_path);
            }
        }

        *status.lock().unwrap() = "Downloading VB-CABLE...".to_string();

        // Use PowerShell to download (available on all modern Windows)
        let result = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!(
                    "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
                     Invoke-WebRequest -Uri 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip' \
                     -OutFile '{}'",
                    zip_path.display()
                ),
            ])
            .output();

        match result {
            Ok(output) if output.status.success() && zip_path.exists() => Ok(zip_path),
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr);
                Err(format!("Download failed: {}", err))
            }
            Err(e) => Err(format!("Could not run PowerShell: {}", e)),
        }
    }

    /// Extract the zip and run the installer with admin privileges
    pub fn install(zip_path: &PathBuf, status: &Arc<Mutex<String>>) -> Result<(), String> {
        let dir = app_dir().join("vbcable");
        let _ = std::fs::create_dir_all(&dir);

        *status.lock().unwrap() = "Extracting VB-CABLE...".to_string();

        // Extract using PowerShell
        let extract = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    zip_path.display(),
                    dir.display()
                ),
            ])
            .output();

        if extract.is_err() || !extract.as_ref().unwrap().status.success() {
            return Err("Failed to extract VB-CABLE".to_string());
        }

        // Find the 64-bit setup exe
        let setup_exe = dir.join("VBCABLE_Setup_x64.exe");
        if !setup_exe.exists() {
            return Err(format!(
                "Setup not found at {}. Check extraction.",
                setup_exe.display()
            ));
        }

        *status.lock().unwrap() = "Installing VB-CABLE (admin prompt)...".to_string();

        // Run installer elevated (will trigger UAC prompt)
        let install = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!(
                    "Start-Process -FilePath '{}' -ArgumentList '-i','-h' -Verb RunAs -Wait",
                    setup_exe.display()
                ),
            ])
            .output();

        match install {
            Ok(output) if output.status.success() => {
                // Give Windows a moment to register the device
                std::thread::sleep(std::time::Duration::from_secs(2));
                if is_installed() {
                    Ok(())
                } else {
                    // Device might need a restart of audio service
                    Err("Installed but device not yet visible. Try restarting the app.".to_string())
                }
            }
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr);
                Err(format!("Install failed: {}", err))
            }
            Err(e) => Err(format!("Could not launch installer: {}", e)),
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 740.0])
            .with_resizable(true)
            .with_min_inner_size([360.0, 480.0])
            .with_title("Microboost"),
        ..Default::default()
    };

    eframe::run_native(
        "Microboost",
        options,
        Box::new(|_cc| Ok(Box::new(MicroboostApp::new()))),
    )
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    fn mouse_redraws_do_not_change_waveform_speed_or_repaint_deadlines() {
        let start = std::time::Instant::now();
        let mut idle = MicroboostApp::new();
        let mut moving = MicroboostApp::new();
        for app in [&mut idle, &mut moving] {
            app.input_history.clear();
            app.output_history.clear();
            app.last_visual_sample = start;
            app.next_visual_repaint = start;
            app.controls.input_rms.set(0.25);
            app.controls.output_rms.set(0.5);
        }
        // Idle redraws at 30 Hz; mouse events cause 1000 redraws/second.
        for millis in (0..=990).step_by(33) {
            let _ = idle.tick_visualizer(start + std::time::Duration::from_millis(millis));
        }
        for millis in 0..=990 {
            let _ = moving.tick_visualizer(start + std::time::Duration::from_millis(millis));
        }
        assert_eq!(idle.input_history, vec![0.25; 30]);
        assert_eq!(idle.output_history, vec![0.5; 30]);
        assert_eq!(moving.input_history, idle.input_history);
        assert_eq!(moving.output_history, idle.output_history);
        assert_eq!(moving.next_visual_repaint, idle.next_visual_repaint);
        let delay = moving.tick_visualizer(start + std::time::Duration::from_millis(995));
        assert_eq!(delay, std::time::Duration::from_millis(28));
    }

    #[test]
    fn visualizer_catchup_is_bounded_after_a_long_pause() {
        let mut app = MicroboostApp::new();
        let start = std::time::Instant::now();
        app.last_visual_sample = start;
        app.next_visual_repaint = start;
        let now = start + std::time::Duration::from_secs(3600);
        let delay = app.tick_visualizer(now);
        assert_eq!(app.input_history.len(), 150);
        assert_eq!(app.output_history.len(), 150);
        assert!(now.duration_since(app.last_visual_sample) < std::time::Duration::from_millis(33));
        assert!(delay > std::time::Duration::ZERO && delay <= std::time::Duration::from_millis(33));
    }

    #[test]
    fn old_settings_keep_safe_defaults() {
        let settings: profiles::Settings =
            serde_json::from_str(r#"{"last_input_device":"My mic"}"#).unwrap();
        assert_eq!(settings.last_input_device.as_deref(), Some("My mic"));
        assert_eq!(settings.startup_mode, AudioMode::Boost);
        assert!(!settings.close_to_tray);
        assert!(settings.peak_protection);
        let settings: profiles::Settings = serde_json::from_str(r#"{"peak_protection":false}"#).unwrap();
        assert!(!settings.peak_protection);
        for mode in [AudioMode::Boost, AudioMode::Bypass, AudioMode::Mute] {
            let settings = profiles::Settings {
                startup_mode: mode,
                ..Default::default()
            };
            let restored: profiles::Settings =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(restored.startup_mode, mode);
        }
    }

    #[test]
    fn stream_errors_are_visible_and_preserve_mute_for_recovery() {
        let mut app = MicroboostApp::new();
        app.setup_state = SetupState::NotInstalled;
        app.mode = AudioMode::Mute;
        *app.pipeline_active.lock().unwrap() = true;
        app.controls.running.store(true, Ordering::Release);
        app.stream_errors
            .push("Microphone disconnected".into())
            .unwrap();
        app.service_audio();
        assert_eq!(app.mode, AudioMode::Mute);
        assert!(!*app.pipeline_active.lock().unwrap());
        assert!(!app.controls.running.load(Ordering::Acquire));
        assert_eq!(*app.status.lock().unwrap(), "Microphone disconnected");
    }

    #[test]
    fn recording_discards_previous_sessions_and_enforces_its_limit() {
        let mut app = MicroboostApp::new();
        app.is_recording = true;
        app.recording_id = 2;
        app.sample_rate = 1;
        *app.rec_samples.lock().unwrap() = Vec::with_capacity(60);
        app.controls.recording.push((1, -1.0)).unwrap();
        for _ in 0..100 {
            app.controls.recording.push((2, 0.25)).unwrap();
        }
        app.drain_recording();
        assert_eq!(*app.rec_samples.lock().unwrap(), vec![0.25; 60]);
        assert!(app.controls.recording.is_empty());
    }

    #[test]
    fn stopping_an_empty_recording_clears_recording_state() {
        let mut app = MicroboostApp::new();
        app.is_recording = true;
        app.controls.recording_id.store(1, Ordering::Release);
        app.recording_id = 1;
        app.recording_start = Some(std::time::Instant::now());
        app.stop_recording();
        assert!(!app.is_recording);
        assert_eq!(app.controls.recording_id.load(Ordering::Acquire), 0);
        assert!(app.recording_start.is_none());
        assert_eq!(*app.status.lock().unwrap(), "No audio recorded");
    }

    #[test]
    fn pipeline_recording_keeps_the_microphones_sample_rate() {
        let mut app = MicroboostApp::new();
        *app.pipeline_active.lock().unwrap() = true;
        app.pipeline_sample_rate = 44100;
        app.is_active = true;
        app.start_recording();
        assert_eq!(app.sample_rate, 44100);
        assert_ne!(app.controls.recording_id.load(Ordering::Acquire), 0);
        app.stop_recording();
    }

    #[test]
    fn replacing_a_microphone_at_the_same_index_requires_reconnection() {
        assert_eq!(
            MicroboostApp::select_input_device(
                &["replacement mic".to_string()],
                Some("old mic"),
                Some("replacement mic")
            ),
            (0, true)
        );
        assert_eq!(
            MicroboostApp::select_input_device(
                &["new mic".to_string(), "old mic".to_string()],
                Some("old mic"),
                Some("new mic")
            ),
            (1, false)
        );
    }

    #[test]
    fn passthrough_waveforms_are_finite_and_clipped_to_the_graph() {
        let mut app = MicroboostApp::new();
        *app.pipeline_active.lock().unwrap() = true;
        app.is_active = false;
        app.input_history = vec![0.0, f32::NAN, f32::INFINITY, 0.1];
        app.output_history = app.input_history.clone();
        let ctx = egui::Context::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 740.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.show_main_screen(ui));
            },
        );
        let mut traces = 0;
        for shape in &output.shapes {
            if let egui::Shape::Path(path) = &shape.shape {
                assert!(
                    !(path.closed
                        && path.fill == egui::Color32::from_rgba_premultiplied(40, 180, 40, 25))
                );
                if matches!(path.stroke.color, egui::epaint::ColorMode::Solid(color)
                    if color == egui::Color32::from_rgb(60, 140, 220)
                        || color == egui::Color32::from_rgb(80, 220, 80))
                {
                    traces += 1;
                    assert!(shape.clip_rect.width() <= 370.0);
                    assert!(shape.clip_rect.height() <= 60.0);
                    assert!(path
                        .points
                        .iter()
                        .all(|point| point.is_finite() && shape.clip_rect.contains(*point)));
                }
            }
        }
        assert_eq!(traces, 2);
    }

    #[test]
    fn saved_profiles_render_names_with_multibyte_truncation_boundaries() {
        let mut app = MicroboostApp::new();
        // Byte 27 falls inside the accented character in this real device name.
        let name = format!("{}é microphone", "a".repeat(26));
        app.input_devices = vec![name.clone()];
        app.selected_input = 0;
        app.device_profiles.clear();
        app.device_profiles.insert(
            name,
            profiles::Profile {
                boost: 100,
                noise_floor_rms: None,
                noise_gate_enabled: false,
            },
        );
        let ctx = egui::Context::default();
        ctx.style_mut(|style| style.animation_time = 0.0);
        // Exercise the real collapsed-section interaction, then its text rendering.
        let render = |ctx: &egui::Context, app: &mut MicroboostApp| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.show_main_screen(ui);
            });
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(420.0, 740.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input.clone(), |ctx| render(ctx, &mut app));
        let header_pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Saved Profiles" => {
                    Some(text.pos + egui::vec2(3.0, 3.0))
                }
                _ => None,
            })
            .expect("Saved Profiles header must be visible");
        for pressed in [true, false] {
            let mut click_input = input.clone();
            click_input.events = vec![
                egui::Event::PointerMoved(header_pos),
                egui::Event::PointerButton {
                    pos: header_pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ];
            let _ = ctx.run(click_input, |ctx| render(ctx, &mut app));
        }
        let output = ctx.run(input, |ctx| render(ctx, &mut app));
        assert!(
            output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text)
                if text.galley.text().contains(&format!("{}é...", "a".repeat(26))))
            }),
            "the expanded section must actually render the truncated name: {:?}",
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
    }
}

#[derive(PartialEq)]
enum SetupState {
    NotInstalled,
    Downloading,
    Failed(String),
    Ready,
}

#[derive(PartialEq, Clone)]
enum CalibrationPhase {
    Idle,
    Listening,
    Done {
        boost_pct: u32,
        raw_db: f32,     // Your voice level in dBFS
        boosted_db: f32, // After boost, in dBFS
        target_db: f32,  // YouTube target in dBFS
    },
}

const CALIBRATION_PHRASES: &[&str] = &[
    "Hello, testing one two three. This is my normal speaking voice.",
    "The quick brown fox jumps over the lazy dog near the river bank.",
    "I'm recording a video and want my audio to sound clear and loud.",
];

/// YouTube recommended voice target: ~-16 dBFS RMS (0.16 linear)
const TARGET_RMS: f32 = 0.16;

struct MicroboostApp {
    host: cpal::Host,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    selected_input: usize,
    selected_output: usize,

    boost: u32,
    is_active: bool,
    mode: AudioMode,
    settings: profiles::Settings,
    controls: Arc<AudioControls>,
    stream_errors: Arc<ArrayQueue<String>>,
    ui_errors: Arc<ArrayQueue<String>>,
    playback_stream: Option<cpal::Stream>,
    playback_finished: Arc<AtomicBool>,
    last_recovery: std::time::Instant,
    last_audio_tick: std::time::Instant,
    tray: Option<desktop::DesktopTray>,
    tray_initialized: bool,
    hidden: bool,
    quitting: bool,
    start_with_windows: bool,
    recording_id: u64,
    status: Arc<Mutex<String>>,

    // VB-CABLE setup
    setup_state: SetupState,
    setup_thread: Option<std::thread::JoinHandle<Result<(), String>>>,

    // Audio pipeline
    input_stream: Arc<Mutex<Option<cpal::Stream>>>,
    output_stream: Arc<Mutex<Option<cpal::Stream>>>,
    ring_buffer: Arc<SpscRing>,
    pipeline_active: Arc<Mutex<bool>>,
    pipeline_sample_rate: u32,
    input_history: Vec<f32>, // Rolling waveform history (RMS values)
    output_history: Vec<f32>,
    last_visual_sample: std::time::Instant,
    next_visual_repaint: std::time::Instant,

    // Test recording
    is_recording: bool,
    recording_start: Option<std::time::Instant>,
    last_recording: Option<PathBuf>,
    sample_rate: u32,
    rec_samples: Arc<Mutex<Vec<f32>>>,

    // Auto-calibration
    cal_phase: CalibrationPhase,
    cal_start: Option<std::time::Instant>,
    cal_stream: Arc<Mutex<Option<cpal::Stream>>>,
    cal_active: Arc<Mutex<bool>>,
    cal_samples: Arc<Mutex<Vec<f32>>>,
    cal_rms_live: Arc<Mutex<f32>>,
    cal_phrase_idx: usize,

    // Profiles
    device_profiles: HashMap<String, profiles::Profile>,

    // Auto-start
    first_frame: bool,

    // Noise gate
    noise_gate: Arc<Mutex<noise_gate::NoiseGate>>,
    ng_cal_state: Arc<Mutex<noise_gate::CalibrationState>>,
    ng_calibrating: bool,
    ng_cal_start: Option<std::time::Instant>,
    ng_cal_stream: Arc<Mutex<Option<cpal::Stream>>>,

    // Hot-plug detection
    last_device_check: std::time::Instant,
}

impl MicroboostApp {
    /// Audio history follows wall-clock time, never the number of UI redraws.
    /// Input events can trigger extra frames anywhere on the desktop.
    fn tick_visualizer(&mut self, now: std::time::Instant) -> std::time::Duration {
        // One point per 30 Hz tick gives a roughly five-second graph window.
        let sample_interval = std::time::Duration::from_millis(33);
        let elapsed = now.saturating_duration_since(self.last_visual_sample);
        let steps = elapsed.as_nanos() / sample_interval.as_nanos();
        if steps > 0 {
            self.last_visual_sample = now
                - std::time::Duration::from_nanos(
                    (elapsed.as_nanos() % sample_interval.as_nanos()) as u64,
                );
            // No historical audio is available after a long UI pause. Hold the
            // latest level, and bound catch-up work to the visible history.
            let input = self.controls.input_rms.get();
            let output = self.controls.output_rms.get();
            for _ in 0..steps.min(150) {
                self.input_history.push(input);
                self.output_history.push(output);
            }
            for history in [&mut self.input_history, &mut self.output_history] {
                if history.len() > 150 {
                    history.drain(..history.len() - 150);
                }
            }
        }

        let frame_interval = std::time::Duration::from_millis(33);
        if now >= self.next_visual_repaint {
            let elapsed = now.duration_since(self.next_visual_repaint);
            self.next_visual_repaint = now + frame_interval
                - std::time::Duration::from_nanos(
                    (elapsed.as_nanos() % frame_interval.as_nanos()) as u64,
                );
        }
        self.next_visual_repaint.saturating_duration_since(now)
    }

    fn handle_desktop(&mut self, ctx: &egui::Context) {
        if !self.tray_initialized {
            self.tray_initialized = true;
            match desktop::DesktopTray::new(ctx.clone()) {
                Ok(tray) => self.tray = Some(tray),
                Err(error) => *self.status.lock().unwrap() = format!("Tray unavailable: {error}"),
            }
            if self.tray.is_some() && std::env::args().any(|arg| arg == "--tray") {
                self.hidden = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        while let Some(action) = self.tray.as_ref().and_then(|tray| tray.actions.pop()) {
            match action {
                desktop::TrayAction::Show => {
                    self.hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    return;
                }
                desktop::TrayAction::Mode(mode) => self.set_mode(mode),
                desktop::TrayAction::Quit => {
                    if self.is_recording {
                        self.stop_recording();
                    }
                    self.save_current_profile();
                    self.kill_pipeline();
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if let Some(tray) = self.tray.as_mut() {
            tray.update_mode(self.mode);
        }
        let close = ctx.input(|input| input.viewport().close_requested());
        let minimize = ctx.input(|input| input.viewport().minimized == Some(true));
        if close && self.settings.close_to_tray && self.tray.is_some() && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hidden = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        } else if close {
            self.quitting = true;
            if self.is_recording {
                self.stop_recording();
            }
            self.save_current_profile();
            self.kill_pipeline();
        } else if minimize && !self.hidden && self.tray.is_some() {
            self.hidden = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn service_audio(&mut self) {
        if self.playback_finished.swap(false, Ordering::AcqRel)
            && self.playback_stream.take().is_some()
        {
            *self.status.lock().unwrap() = "Playback finished".into();
        }
        while let Some(error) = self.ui_errors.pop() {
            if self.cal_phase == CalibrationPhase::Listening {
                self.cancel_calibration();
            }
            if self.ng_calibrating {
                self.cancel_noise_calibration();
            }
            *self.status.lock().unwrap() = error;
        }
        let resumed = self.last_audio_tick.elapsed() > std::time::Duration::from_secs(5);
        self.last_audio_tick = std::time::Instant::now();
        let mut failure = None;
        while let Some(error) = self.stream_errors.pop() {
            failure = Some(error);
        }
        if resumed
            || failure.is_some()
            || (*self.pipeline_active.lock().unwrap()
                && !self.controls.running.load(Ordering::Acquire))
        {
            if self.is_recording {
                self.stop_recording();
            }
            self.kill_pipeline();
            *self.status.lock().unwrap() =
                failure.unwrap_or_else(|| "Restoring audio connection…".into());
        }
        if self.setup_state == SetupState::Ready
            && !*self.pipeline_active.lock().unwrap()
            && self.last_recovery.elapsed() >= std::time::Duration::from_secs(2)
        {
            let current = self.input_devices.get(self.selected_input).cloned();
            self.refresh_devices();
            let (selected, changed) = Self::select_input_device(
                &self.input_devices,
                current.as_deref(),
                self.host
                    .default_input_device()
                    .and_then(|device| device.name().ok())
                    .as_deref(),
            );
            self.selected_input = selected;
            if changed {
                self.load_profile_for_device();
            }
            self.start_pipeline();
        }
        self.drain_recording();
        if self.is_recording
            && self
                .recording_start
                .map(|time| time.elapsed().as_secs() >= 60)
                .unwrap_or(false)
        {
            self.stop_recording();
        }
    }

    fn new() -> Self {
        let host = cpal::default_host();
        let (input_devices, output_devices) = Self::enumerate_devices(&host);

        let cable_installed = vbcable::is_installed();
        let selected_output = Self::find_cable_output(&output_devices).unwrap_or(0);

        let device_profiles = profiles::load();
        let settings = profiles::load_settings();

        // Select input device: 1) last used, 2) Windows default, 3) first
        let selected_input = settings
            .last_input_device
            .as_ref()
            .and_then(|saved| input_devices.iter().position(|d| d == saved))
            .or_else(|| {
                // Try to match Windows default input device
                host.default_input_device()
                    .and_then(|d| d.name().ok())
                    .and_then(|default_name| input_devices.iter().position(|d| d == &default_name))
            })
            .unwrap_or(0);

        // Load saved boost for the selected device
        let current_profile = input_devices
            .get(selected_input)
            .and_then(|name| device_profiles.get(name));
        let boost = current_profile.map(|p| p.boost).unwrap_or(200);

        // Restore noise gate from profile
        let noise_gate = noise_gate::NoiseGate::new();
        let ng = {
            let mut ng = noise_gate;
            if let Some(profile) = current_profile {
                if let Some(floor) = profile.noise_floor_rms {
                    let headroom = ng.headroom;
                    ng.restore(floor, profile.noise_gate_enabled, headroom);
                }
            }
            ng
        };

        Self {
            host,
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            boost,
            is_active: false,
            mode: settings.startup_mode,
            settings,
            controls: Arc::new(AudioControls::default()),
            stream_errors: Arc::new(ArrayQueue::new(8)),
            ui_errors: Arc::new(ArrayQueue::new(8)),
            playback_stream: None,
            playback_finished: Arc::new(AtomicBool::new(false)),
            last_recovery: std::time::Instant::now(),
            last_audio_tick: std::time::Instant::now(),
            tray: None,
            tray_initialized: false,
            hidden: false,
            quitting: false,
            start_with_windows: desktop::starts_with_windows(),
            recording_id: 0,
            status: Arc::new(Mutex::new("Ready".to_string())),
            setup_state: if cable_installed {
                SetupState::Ready
            } else {
                SetupState::NotInstalled
            },
            setup_thread: None,
            input_stream: Arc::new(Mutex::new(None)),
            output_stream: Arc::new(Mutex::new(None)),
            ring_buffer: Arc::new(SpscRing::new(RING_SIZE)),
            pipeline_active: Arc::new(Mutex::new(false)),
            pipeline_sample_rate: 48000,
            input_history: vec![0.0; 120],
            output_history: vec![0.0; 120],
            last_visual_sample: std::time::Instant::now(),
            next_visual_repaint: std::time::Instant::now(),
            is_recording: false,
            recording_start: None,
            last_recording: None,
            sample_rate: 48000,
            rec_samples: Arc::new(Mutex::new(Vec::new())),

            cal_phase: CalibrationPhase::Idle,
            cal_start: None,
            cal_stream: Arc::new(Mutex::new(None)),
            cal_active: Arc::new(Mutex::new(false)),
            cal_samples: Arc::new(Mutex::new(Vec::new())),
            cal_rms_live: Arc::new(Mutex::new(0.0)),
            cal_phrase_idx: 0,

            device_profiles,

            first_frame: true,

            noise_gate: Arc::new(Mutex::new(ng)),
            ng_cal_state: noise_gate::new_calibration_state(),
            ng_calibrating: false,
            ng_cal_start: None,
            ng_cal_stream: Arc::new(Mutex::new(None)),

            last_device_check: std::time::Instant::now(),
        }
    }

    fn enumerate_devices(host: &cpal::Host) -> (Vec<String>, Vec<String>) {
        let inputs: Vec<String> = host
            .input_devices()
            .map(|devs| {
                devs.filter_map(|d| d.name().ok())
                    .filter(|name| {
                        let lower = name.to_lowercase();
                        !lower.contains("cable output") && !lower.contains("cable input")
                    })
                    .collect()
            })
            .unwrap_or_default();
        let outputs: Vec<String> = host
            .output_devices()
            .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();
        (inputs, outputs)
    }

    /// Find an input device by name (avoids index mismatch when CABLE is filtered from UI)
    fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
        host.input_devices()
            .ok()?
            .find(|d| d.name().ok().as_deref() == Some(name))
    }

    fn find_cable_output(devices: &[String]) -> Option<usize> {
        devices.iter().position(|name| {
            let lower = name.to_lowercase();
            lower.contains("cable input")
        })
    }

    fn refresh_devices(&mut self) {
        let (inputs, outputs) = Self::enumerate_devices(&self.host);
        self.input_devices = inputs;
        self.output_devices = outputs;
        if let Some(idx) = Self::find_cable_output(&self.output_devices) {
            self.selected_output = idx;
        }
    }

    /// Return the selected index and whether the microphone identity changed.
    fn select_input_device(
        devices: &[String],
        current: Option<&str>,
        default: Option<&str>,
    ) -> (usize, bool) {
        if let Some(pos) = current.and_then(|name| devices.iter().position(|d| d == name)) {
            return (pos, false);
        }
        let index = default
            .and_then(|name| devices.iter().position(|d| d == name))
            .unwrap_or(0);
        (index, true)
    }

    /// Periodically check for device changes (hot-plug).
    /// If the current device disappeared, switch to the Windows default (or first available).
    /// If a new default device appeared that wasn't there before, switch to it.
    fn check_device_changes(&mut self) {
        if self.last_device_check.elapsed().as_secs() < 2 {
            return;
        }
        self.last_device_check = std::time::Instant::now();

        let old_devices = self.input_devices.clone();
        let current_name = self.input_devices.get(self.selected_input).cloned();

        self.refresh_devices();

        // Check if device list actually changed
        if self.input_devices == old_devices {
            return;
        }

        // Get Windows default device name
        let default_name = self.host.default_input_device().and_then(|d| d.name().ok());

        let (new_idx, changed) = Self::select_input_device(
            &self.input_devices,
            current_name.as_deref(),
            default_name.as_deref(),
        );
        self.selected_input = new_idx;
        if changed {
            if self.is_recording {
                self.stop_recording();
            }
            self.selected_input = new_idx;
            self.load_profile_for_device();

            // Restart pipeline with new device
            self.kill_pipeline();
            if !self.input_devices.is_empty() {
                self.start_pipeline();
            }

            let dev_name = self
                .input_devices
                .get(self.selected_input)
                .cloned()
                .unwrap_or("none".to_string());
            *self.status.lock().unwrap() = format!("Device changed — switched to {}", dev_name);
        }
    }

    fn start_vbcable_install(&mut self) {
        self.setup_state = SetupState::Downloading;
        let status = self.status.clone();

        self.setup_thread = Some(std::thread::spawn(move || {
            let zip = vbcable::download(&status)?;
            vbcable::install(&zip, &status)?;
            *status.lock().unwrap() = "VB-CABLE installed!".to_string();
            Ok(())
        }));
    }

    fn check_setup_thread(&mut self) {
        if let Some(handle) = &self.setup_thread {
            if handle.is_finished() {
                let handle = self.setup_thread.take().unwrap();
                match handle.join() {
                    Ok(Ok(())) => {
                        self.setup_state = SetupState::Ready;
                        self.refresh_devices();
                    }
                    Ok(Err(e)) => {
                        self.setup_state = SetupState::Failed(e);
                    }
                    Err(_) => {
                        self.setup_state = SetupState::Failed("Setup thread panicked".to_string());
                    }
                }
            }
        }
    }

    fn sync_audio_controls(&self) {
        self.controls.gain.set(self.boost as f32 / 100.0);
        let gate = self.noise_gate.lock().unwrap();
        self.controls.noise_floor.set(gate.noise_floor_rms());
        self.controls
            .gate_enabled
            .store(gate.enabled, Ordering::Relaxed);
        self.controls.mode.store(self.mode as u8, Ordering::Release);
        self.controls
            .peak_protection
            .store(self.settings.peak_protection, Ordering::Relaxed);
    }

    fn set_mode(&mut self, mode: AudioMode) {
        self.mode = mode;
        self.is_active = mode == AudioMode::Boost;
        self.settings.startup_mode = mode;
        self.sync_audio_controls();
        profiles::save_settings(&self.settings);
        if !*self.pipeline_active.lock().unwrap() {
            self.start_pipeline();
        } else {
            *self.status.lock().unwrap() = format!("{} — audio connected", mode.label());
        }
    }

    fn start_pipeline(&mut self) {
        if self.is_recording && !*self.pipeline_active.lock().unwrap() {
            self.stop_recording();
        }
        self.sync_audio_controls();
        if *self.pipeline_active.lock().unwrap() {
            return;
        }
        self.last_recovery = std::time::Instant::now();
        // Match both endpoints by name; never fall back to physical speakers.
        let input = self
            .input_devices
            .get(self.selected_input)
            .and_then(|name| Self::find_input_device_by_name(&self.host, name));
        let output = self
            .output_devices
            .get(self.selected_output)
            .filter(|name| name.to_lowercase().contains("cable input"))
            .and_then(|name| {
                self.host
                    .output_devices()
                    .ok()?
                    .find(|device| device.name().ok().as_ref() == Some(name))
            });
        let result = (|| -> Result<(cpal::Stream, cpal::Stream, u32), String> {
            let input = input.ok_or("Microphone unavailable; waiting to reconnect")?;
            let output = output.ok_or("Virtual cable unavailable; waiting to reconnect")?;
            let in_config = input
                .default_input_config()
                .map_err(|e| format!("Microphone: {e}"))?;
            let out_config = output
                .default_output_config()
                .map_err(|e| format!("Virtual cable: {e}"))?;
            let input_rate = in_config.sample_rate().0;
            let input_channels = in_config.channels() as usize;
            self.ring_buffer = Arc::new(SpscRing::new(RING_SIZE));
            let ring = self.ring_buffer.clone();
            let controls = self.controls.clone();
            let errors = self.stream_errors.clone();
            let error_controls = self.controls.clone();
            let mut processor = AudioProcessor::new(input_rate, self.boost as f32 / 100.0);
            let input_stream = audio::build_input_stream(
                &input,
                &in_config,
                move |data| {
                    if !controls.running.load(Ordering::Acquire) {
                        return;
                    }
                    let settings = controls.settings();
                    let mut sum_in = 0.0;
                    let mut sum_out = 0.0;
                    let mut count = 0;
                    let mut reduction = 0.0_f32;
                    for frame in data.chunks(input_channels) {
                        let raw = frame[0];
                        let processed = processor.process(raw, settings);
                        if raw.abs() >= 0.9999 {
                            controls.clipped.fetch_add(1, Ordering::Relaxed);
                        }
                        if !ring.push(processed) {
                            controls.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        let recording_id = controls.recording_id.load(Ordering::Acquire);
                        if recording_id != 0
                            && controls.recording.push((recording_id, processed)).is_err()
                        {
                            controls.recording_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        let raw = if raw.is_finite() { raw } else { 0.0 };
                        sum_in += raw * raw;
                        sum_out += processed * processed;
                        reduction = reduction.max(processor.reduction_db);
                        count += 1;
                    }
                    if count > 0 {
                        controls.input_rms.set(
                            controls.input_rms.get() * 0.8 + (sum_in / count as f32).sqrt() * 0.2,
                        );
                        controls.output_rms.set(
                            controls.output_rms.get() * 0.8 + (sum_out / count as f32).sqrt() * 0.2,
                        );
                        controls.reduction_db.set(reduction);
                    }
                },
                move |error| {
                    error_controls.running.store(false, Ordering::Release);
                    let _ = errors.push(format!("Microphone disconnected: {error}"));
                },
            )?;
            let ring = self.ring_buffer.clone();
            let controls = self.controls.clone();
            let errors = self.stream_errors.clone();
            let error_controls = self.controls.clone();
            let mut resampler = Resampler::new(input_rate, out_config.sample_rate().0);
            let prebuffer_samples = (input_rate as usize / 50).max(2);
            let mut prebuffered = false;
            let output_stream = audio::build_output_stream(
                &output,
                &out_config,
                move || {
                    if !controls.running.load(Ordering::Acquire) {
                        return 0.0;
                    }
                    if !prebuffered {
                        if ring.available() < prebuffer_samples {
                            return 0.0;
                        }
                        prebuffered = true;
                    }
                    let sample = match resampler.next_sample(&ring) {
                        Some(sample) => sample,
                        None => {
                            controls.underruns.fetch_add(1, Ordering::Relaxed);
                            prebuffered = false;
                            0.0
                        }
                    };
                    // Apply mute at playback too so already queued speech is silenced.
                    if AudioMode::from_byte(controls.mode.load(Ordering::Acquire))
                        == AudioMode::Mute
                    {
                        0.0
                    } else {
                        sample
                    }
                },
                move |error| {
                    error_controls.running.store(false, Ordering::Release);
                    let _ = errors.push(format!("Virtual cable disconnected: {error}"));
                },
            )?;
            input_stream
                .play()
                .map_err(|e| format!("Microphone start failed: {e}"))?;
            output_stream
                .play()
                .map_err(|e| format!("Virtual cable start failed: {e}"))?;
            Ok((input_stream, output_stream, input_rate))
        })();
        match result {
            Ok((input, output, rate)) => {
                *self.input_stream.lock().unwrap() = Some(input);
                *self.output_stream.lock().unwrap() = Some(output);
                self.pipeline_sample_rate = rate;
                *self.pipeline_active.lock().unwrap() = true;
                self.controls.running.store(true, Ordering::Release);
                self.is_active = self.mode == AudioMode::Boost;
                *self.status.lock().unwrap() =
                    format!("{} — connected at {} Hz", self.mode.label(), rate);
            }
            Err(error) => {
                self.controls.running.store(false, Ordering::Release);
                *self.pipeline_active.lock().unwrap() = false;
                self.is_active = false;
                *self.status.lock().unwrap() = format!("{error}. Retrying automatically…");
            }
        }
    }

    fn kill_pipeline(&mut self) {
        self.controls.running.store(false, Ordering::Release);
        self.controls.input_rms.set(0.0);
        self.controls.output_rms.set(0.0);
        self.controls.reduction_db.set(0.0);
        // Actually stop the audio streams (used on device switch)
        *self.pipeline_active.lock().unwrap() = false;
        *self.input_stream.lock().unwrap() = None;
        *self.output_stream.lock().unwrap() = None;
        self.is_active = false;
        *self.status.lock().unwrap() = "Stopped".to_string();
    }

    fn save_current_profile(&mut self) {
        if let Some(name) = self.input_devices.get(self.selected_input) {
            let gate = self.noise_gate.lock().unwrap();
            let noise_floor_rms = if gate.is_calibrated() {
                Some(gate.noise_floor_rms())
            } else {
                None
            };
            let noise_gate_enabled = gate.enabled;
            drop(gate);

            self.device_profiles.insert(
                name.clone(),
                profiles::Profile {
                    boost: self.boost,
                    noise_floor_rms,
                    noise_gate_enabled,
                },
            );
            profiles::save(&self.device_profiles);
            self.settings.last_input_device = Some(name.clone());
            profiles::save_settings(&self.settings);
        }
    }

    fn load_profile_for_device(&mut self) {
        self.boost = 200;
        self.noise_gate.lock().unwrap().reset();
        if let Some(name) = self.input_devices.get(self.selected_input) {
            if let Some(profile) = self.device_profiles.get(name) {
                self.boost = profile.boost;
                let mut gate = self.noise_gate.lock().unwrap();
                if let Some(floor) = profile.noise_floor_rms {
                    let headroom = gate.headroom;
                    gate.restore(floor, profile.noise_gate_enabled, headroom);
                } else {
                    gate.enabled = false;
                }
            }
        }
    }

    fn update_gain(&mut self) {
        self.sync_audio_controls();
        if self.mode == AudioMode::Boost {
            *self.status.lock().unwrap() = format!("Boost {:.1}×", self.boost as f32 / 100.0);
        }
    }

    fn start_calibration(&mut self) {
        // Keep pipeline running — calibration opens a second input stream for raw samples

        *self.cal_samples.lock().unwrap() = Vec::new();
        *self.cal_active.lock().unwrap() = true;
        *self.cal_rms_live.lock().unwrap() = 0.0;
        self.cal_phase = CalibrationPhase::Listening;
        self.cal_start = Some(std::time::Instant::now());
        // Pick a random phrase
        self.cal_phrase_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis() as usize)
            % CALIBRATION_PHRASES.len();

        let device = self
            .input_devices
            .get(self.selected_input)
            .and_then(|name| Self::find_input_device_by_name(&self.host, name))
            .or_else(|| self.host.default_input_device());

        if let Some(device) = device {
            if let Ok(supported_config) = device.default_input_config() {
                let input_channels = supported_config.channels() as usize;
                let samples = self.cal_samples.clone();
                let active = self.cal_active.clone();
                let rms_live = self.cal_rms_live.clone();
                let sample_rate = supported_config.sample_rate().0 as usize;
                *self.cal_samples.lock().unwrap() = Vec::with_capacity(sample_rate * 6);

                let errors = self.ui_errors.clone();
                if let Ok(stream) = audio::build_input_stream(
                    &device,
                    &supported_config,
                    move |data: &[f32]| {
                        if !active.try_lock().map(|value| *value).unwrap_or(false) {
                            return;
                        }
                        let Ok(mut s) = samples.try_lock() else {
                            return;
                        };
                        for chunk in data.chunks(input_channels) {
                            let mono = chunk[0];
                            if s.len() < s.capacity() {
                                s.push(mono);
                            } // Raw, no boost
                        }
                        // Update live RMS over last ~0.3s
                        let window = sample_rate / 3;
                        if s.len() > window {
                            let recent = &s[s.len() - window..];
                            let sum_sq: f32 = recent.iter().map(|x| x * x).sum();
                            let rms = (sum_sq / recent.len() as f32).sqrt();
                            if let Ok(mut live) = rms_live.try_lock() {
                                *live = rms;
                            }
                        }
                    },
                    move |error| {
                        let _ = errors.push(format!("Calibration audio error: {error}"));
                    },
                ) {
                    if stream.play().is_ok() {
                        *self.cal_stream.lock().unwrap() = Some(stream);
                    }
                }
            }
        }
        if self.cal_stream.lock().unwrap().is_none() {
            self.cancel_calibration();
            *self.status.lock().unwrap() = "Could not open microphone for calibration".into();
        } else {
            *self.status.lock().unwrap() = "Calibrating — speak now...".into();
        }
    }

    fn finish_calibration(&mut self) {
        *self.cal_active.lock().unwrap() = false;
        *self.cal_stream.lock().unwrap() = None;

        let raw_rms = {
            let samples = self.cal_samples.lock().unwrap();
            if samples.len() < 4800 {
                self.cal_phase = CalibrationPhase::Idle;
                *self.status.lock().unwrap() = "Calibration failed — not enough audio".to_string();
                return;
            }

            // Compute RMS of entire capture, ignoring silence (gate at -50 dBFS)
            let silence_gate = 0.003_f32; // ~-50 dBFS
            let voiced: Vec<f32> = samples
                .iter()
                .copied()
                .filter(|s| s.abs() > silence_gate)
                .collect();

            if voiced.len() < 2400 {
                self.cal_phase = CalibrationPhase::Idle;
                *self.status.lock().unwrap() =
                    "Calibration failed — couldn't detect speech. Try speaking louder.".to_string();
                return;
            }

            let sum_sq: f32 = voiced.iter().map(|x| x * x).sum();
            (sum_sq / voiced.len() as f32).sqrt()
        };

        // Calculate needed boost
        let needed = TARGET_RMS / raw_rms;
        let boost_pct = (needed * 100.0).round() as u32;
        let boost_pct = boost_pct.clamp(10, 500);

        let raw_db = 20.0 * raw_rms.log10();
        let boosted_rms = (raw_rms * boost_pct as f32 / 100.0).min(1.0);
        let boosted_db = 20.0 * boosted_rms.log10();
        let target_db = 20.0 * TARGET_RMS.log10();

        self.cal_phase = CalibrationPhase::Done {
            boost_pct,
            raw_db,
            boosted_db,
            target_db,
        };
        // Pipeline kept running — no need to restart
        *self.status.lock().unwrap() = format!(
            "Voice: {:.1} dB -> Boosted: {:.1} dB (target: {:.1} dB)",
            raw_db, boosted_db, target_db
        );
    }

    fn cancel_calibration(&mut self) {
        *self.cal_active.lock().unwrap() = false;
        *self.cal_stream.lock().unwrap() = None;
        self.cal_phase = CalibrationPhase::Idle;
        *self.status.lock().unwrap() = "Calibration cancelled".to_string();
    }

    fn start_noise_calibration(&mut self) {
        // Keep pipeline running — calibration opens a second input stream for raw samples

        {
            let mut cal = self.ng_cal_state.lock().unwrap();
            cal.active = true;
            cal.samples.clear();
        }
        self.ng_calibrating = true;
        self.ng_cal_start = Some(std::time::Instant::now());

        let device = self
            .input_devices
            .get(self.selected_input)
            .and_then(|name| Self::find_input_device_by_name(&self.host, name))
            .or_else(|| self.host.default_input_device());

        if let Some(device) = device {
            if let Ok(supported_config) = device.default_input_config() {
                let input_channels = supported_config.channels() as usize;
                let cal_state = self.ng_cal_state.clone();
                self.ng_cal_state.lock().unwrap().samples =
                    Vec::with_capacity(supported_config.sample_rate().0 as usize * 4);

                let errors = self.ui_errors.clone();
                if let Ok(stream) = audio::build_input_stream(
                    &device,
                    &supported_config,
                    move |data: &[f32]| {
                        let Ok(mut cal) = cal_state.try_lock() else {
                            return;
                        };
                        if !cal.active {
                            return;
                        }
                        for chunk in data.chunks(input_channels) {
                            let mono = chunk[0];
                            if cal.samples.len() < cal.samples.capacity() {
                                cal.samples.push(mono);
                            }
                        }
                    },
                    move |error| {
                        let _ = errors.push(format!("Calibration audio error: {error}"));
                    },
                ) {
                    if stream.play().is_ok() {
                        *self.ng_cal_stream.lock().unwrap() = Some(stream);
                    }
                }
            }
        }
        if self.ng_cal_stream.lock().unwrap().is_none() {
            self.cancel_noise_calibration();
            *self.status.lock().unwrap() = "Could not open microphone for noise calibration".into();
        } else {
            *self.status.lock().unwrap() = "Noise calibration — stay SILENT...".into();
        }
    }

    fn finish_noise_calibration(&mut self) {
        {
            let mut cal = self.ng_cal_state.lock().unwrap();
            cal.active = false;
        }
        *self.ng_cal_stream.lock().unwrap() = None;
        self.ng_calibrating = false;
        self.ng_cal_start = None;

        let samples = {
            let cal = self.ng_cal_state.lock().unwrap();
            cal.samples.clone()
        };

        let mut gate = self.noise_gate.lock().unwrap();
        match gate.finish_calibration(&samples) {
            Ok(db) => {
                *self.status.lock().unwrap() = format!(
                    "Noise floor: {:.1} dBFS | Gate threshold: {:.1} dBFS | Gate ON",
                    db,
                    gate.threshold_db()
                );
            }
            Err(e) => {
                *self.status.lock().unwrap() = format!("Noise calibration failed: {}", e);
            }
        }
        drop(gate);

        // Save noise gate to profile (pipeline keeps running)
        self.save_current_profile();
    }

    fn cancel_noise_calibration(&mut self) {
        {
            let mut cal = self.ng_cal_state.lock().unwrap();
            cal.active = false;
        }
        *self.ng_cal_stream.lock().unwrap() = None;
        self.ng_calibrating = false;
        self.ng_cal_start = None;
        *self.status.lock().unwrap() = "Noise calibration cancelled".to_string();
    }

    fn drain_recording(&mut self) {
        let limit = self.sample_rate as usize * 60;
        let mut samples = self.rec_samples.lock().unwrap();
        while let Some((id, sample)) = self.controls.recording.pop() {
            if self.is_recording && id == self.recording_id && samples.len() < limit {
                samples.push(sample);
            }
        }
    }

    fn start_recording(&mut self) {
        self.playback_stream = None;
        if !*self.pipeline_active.lock().unwrap() {
            self.start_pipeline();
        }
        if !*self.pipeline_active.lock().unwrap() {
            return;
        }
        self.sample_rate = self.pipeline_sample_rate;
        // Reserve a full minute on the UI thread; callbacks only push into the
        // preallocated bounded queue. A session id prevents stale recordings.
        *self.rec_samples.lock().unwrap() = Vec::with_capacity(self.sample_rate as usize * 60);
        while self.controls.recording.pop().is_some() {}
        self.recording_id = self.recording_id.wrapping_add(1).max(1);
        self.controls.recording_dropped.store(0, Ordering::Relaxed);
        self.is_recording = true;
        self.recording_start = Some(std::time::Instant::now());
        self.controls
            .recording_id
            .store(self.recording_id, Ordering::Release);
        *self.status.lock().unwrap() =
            format!("Recording {} output (60 second limit)…", self.mode.label());
    }

    fn stop_recording(&mut self) {
        self.controls.recording_id.store(0, Ordering::Release);
        self.drain_recording();
        self.is_recording = false;

        let samples = self.rec_samples.lock().unwrap().clone();
        if samples.is_empty() {
            self.recording_start = None;
            *self.status.lock().unwrap() = "No audio recorded".to_string();
            return;
        }

        let folder =
            PathBuf::from(std::env::var("APPDATA").unwrap_or(".".to_string())).join("Microboost");
        let _ = std::fs::create_dir_all(&folder);

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let path = folder.join(format!("test_{}.wav", timestamp));

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        if let Ok(mut writer) = hound::WavWriter::create(&path, spec) {
            for sample in &samples {
                let amp = (*sample * i16::MAX as f32) as i16;
                let _ = writer.write_sample(amp);
            }
            let _ = writer.finalize();

            let elapsed = self
                .recording_start
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            self.last_recording = Some(path);
            *self.status.lock().unwrap() = format!("Saved: {}s", elapsed);
        }

        self.recording_start = None;
    }

    fn play_recording(&mut self) {
        self.playback_stream = None;
        let Some(path) = self.last_recording.as_ref() else {
            return;
        };
        let result = (|| -> Result<cpal::Stream, String> {
            let mut reader = hound::WavReader::open(path).map_err(|e| format!("Recording: {e}"))?;
            let spec = reader.spec();
            let samples = reader
                .samples::<i16>()
                .map(|sample| sample.map(|s| s as f32 / 32768.0))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Recording: {e}"))?;
            if samples.is_empty() {
                return Err("Recording is empty".into());
            }
            let host = cpal::default_host();
            let physical = |device: &cpal::Device| {
                device
                    .name()
                    .map(|name| !name.to_lowercase().contains("cable input"))
                    .unwrap_or(false)
            };
            let speaker = host
                .default_output_device()
                .filter(physical)
                .or_else(|| host.output_devices().ok()?.find(physical))
                .ok_or("No speakers available")?;
            let config = speaker.default_output_config().map_err(|e| e.to_string())?;
            let mut playback =
                audio::Playback::new(samples, spec.sample_rate, config.sample_rate().0);
            self.playback_finished.store(false, Ordering::Release);
            let finished = self.playback_finished.clone();
            let error_finished = finished.clone();
            let errors = self.ui_errors.clone();
            let stream = audio::build_output_stream(
                &speaker,
                &config,
                move || match playback.next_sample() {
                    Some(sample) => sample,
                    None => {
                        finished.store(true, Ordering::Release);
                        0.0
                    }
                },
                move |error| {
                    error_finished.store(true, Ordering::Release);
                    let _ = errors.push(format!("Playback failed: {error}"));
                },
            )?;
            stream.play().map_err(|e| e.to_string())?;
            Ok(stream)
        })();
        match result {
            Ok(stream) => {
                self.playback_stream = Some(stream);
                *self.status.lock().unwrap() = "Playing recording…".into();
            }
            Err(error) => *self.status.lock().unwrap() = error,
        }
    }

    fn open_folder(&self) {
        let folder =
            PathBuf::from(std::env::var("APPDATA").unwrap_or(".".to_string())).join("Microboost");
        let _ = std::fs::create_dir_all(&folder);
        let _ = std::process::Command::new("explorer").arg(&folder).spawn();
    }
}

impl eframe::App for MicroboostApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_desktop(ctx);
        if self.quitting {
            return;
        }
        self.sync_audio_controls();
        self.service_audio();
        // Poll setup thread
        self.check_setup_thread();

        // Auto-start pipeline on first frame
        if self.first_frame && self.setup_state == SetupState::Ready {
            self.first_frame = false;
            self.start_pipeline();
        }

        // Periodic device hot-plug detection
        if self.setup_state == SetupState::Ready
            && !self.ng_calibrating
            && self.cal_phase == CalibrationPhase::Idle
        {
            self.check_device_changes();
        }

        // Auto-finish calibration after 5 seconds
        if self.cal_phase == CalibrationPhase::Listening {
            if let Some(start) = self.cal_start {
                if start.elapsed().as_secs() >= 5 {
                    self.finish_calibration();
                }
            }
        }

        // Auto-finish noise calibration after 3 seconds
        if self.ng_calibrating {
            if let Some(start) = self.ng_cal_start {
                if start.elapsed().as_secs() >= 3 {
                    self.finish_noise_calibration();
                }
            }
        }

        let pipeline_running = *self.pipeline_active.lock().unwrap();
        if self.is_recording
            || self.is_active
            || pipeline_running
            || self.ng_calibrating
            || self.cal_phase == CalibrationPhase::Listening
            || matches!(self.setup_state, SetupState::Downloading)
        {
            let delay = if self.hidden {
                // Don't replay tray time as waveform history when restored.
                self.last_visual_sample = std::time::Instant::now();
                std::time::Duration::from_millis(250)
            } else {
                self.tick_visualizer(std::time::Instant::now())
            };
            ctx.request_repaint_after(delay);
        } else if self.setup_state == SetupState::Ready {
            self.last_visual_sample = std::time::Instant::now();
            // Poll device recovery even when idle.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match &self.setup_state {
                    SetupState::NotInstalled => {
                        self.show_setup_screen(ui);
                    }
                    SetupState::Downloading => {
                        self.show_progress_screen(ui);
                    }
                    SetupState::Failed(err) => {
                        let err = err.clone();
                        self.show_failed_screen(ui, &err);
                    }
                    SetupState::Ready => {
                        self.show_main_screen(ui);
                    }
                });
        });
    }
}

impl MicroboostApp {
    fn show_setup_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Setup Required");
        ui.add_space(12.0);

        ui.label("Microboost needs VB-CABLE (free) to route boosted audio to other apps.");
        ui.add_space(8.0);
        ui.label(
            "VB-CABLE creates a virtual microphone that apps like Discord, Teams, etc. can use.",
        );
        ui.add_space(16.0);

        ui.label("What will happen:");
        ui.label("  1. Download VB-CABLE (~1 MB)");
        ui.label("  2. Install it (admin prompt)");
        ui.label("  3. A new \"CABLE Output\" mic appears in Windows");
        ui.add_space(16.0);

        let btn = ui.add_sized(
            [320.0, 40.0],
            egui::Button::new(
                egui::RichText::new("Install VB-CABLE")
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(egui::Color32::from_rgb(60, 120, 200)),
        );
        if btn.clicked() {
            self.start_vbcable_install();
        }

        ui.add_space(8.0);
        if ui
            .link("Already have a virtual cable? Skip setup")
            .clicked()
        {
            self.setup_state = SetupState::Ready;
            self.refresh_devices();
        }
    }

    fn show_progress_screen(&self, ui: &mut egui::Ui) {
        ui.heading("Setting up VB-CABLE...");
        ui.add_space(20.0);
        ui.spinner();
        ui.add_space(12.0);
        let status = self.status.lock().unwrap().clone();
        ui.label(&status);
        ui.add_space(8.0);
        ui.label("An admin prompt may appear — please accept it.");
    }

    fn show_failed_screen(&mut self, ui: &mut egui::Ui, err: &str) {
        ui.heading("Setup Failed");
        ui.add_space(12.0);
        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
        ui.add_space(16.0);

        if ui.button("Retry").clicked() {
            self.start_vbcable_install();
        }
        ui.add_space(8.0);
        if ui
            .link("Skip — I'll set up a virtual cable manually")
            .clicked()
        {
            self.setup_state = SetupState::Ready;
            self.refresh_devices();
        }
    }

    fn show_main_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Microphone Boost");
        ui.add_space(4.0);

        // Input device
        ui.horizontal(|ui| {
            ui.label("Microphone");
            if let Some(name) = self.input_devices.get(self.selected_input) {
                if self.device_profiles.contains_key(name) {
                    ui.label(
                        egui::RichText::new("(saved profile)")
                            .small()
                            .color(egui::Color32::from_rgb(120, 180, 120)),
                    );
                }
            }
        });
        let prev_input = self.selected_input;
        egui::ComboBox::from_id_salt("input_device")
            .width(340.0)
            .selected_text(
                self.input_devices
                    .get(self.selected_input)
                    .cloned()
                    .unwrap_or("No input devices".to_string()),
            )
            .show_ui(ui, |ui| {
                for (i, name) in self.input_devices.iter().enumerate() {
                    ui.selectable_value(&mut self.selected_input, i, name);
                }
            });

        if prev_input != self.selected_input {
            if self.is_recording {
                self.stop_recording();
            }
            // Save boost for old device, load for new one
            if let Some(old_name) = self.input_devices.get(prev_input) {
                let gate = self.noise_gate.lock().unwrap();
                let noise_floor_rms = if gate.is_calibrated() {
                    Some(gate.noise_floor_rms())
                } else {
                    None
                };
                let noise_gate_enabled = gate.enabled;
                drop(gate);
                self.device_profiles.insert(
                    old_name.clone(),
                    profiles::Profile {
                        boost: self.boost,
                        noise_floor_rms,
                        noise_gate_enabled,
                    },
                );
            }
            self.load_profile_for_device();
            profiles::save(&self.device_profiles);

            // Restart pipeline with new device
            self.kill_pipeline();
            self.start_pipeline();
        }

        ui.add_space(8.0);

        // Boost slider (capped at 500%, manual entry for higher)
        let boost_presets = [10, 25, 50, 100, 150, 200, 300, 400, 500];
        let prev_boost = self.boost;
        ui.horizontal(|ui| {
            ui.label("Boost:");
            let drag = ui.add(
                egui::DragValue::new(&mut self.boost)
                    .range(10..=1000)
                    .speed(10)
                    .suffix("%"),
            );
            ui.label(format!("({:.1}x)", self.boost as f32 / 100.0));
            if drag.changed() {
                // Round to nearest 10
                self.boost = ((self.boost + 5) / 10) * 10;
                self.boost = self.boost.clamp(10, 1000);
            }
            if drag.lost_focus() && self.is_active && self.boost != prev_boost {
                self.update_gain();
            }
        });

        ui.add_space(4.0);

        // Slider caps at 500%
        let slider_val = self.boost.clamp(10, 500);
        let mut slider_boost = slider_val;
        ui.push_id("boost_slider", |ui| {
            ui.spacing_mut().slider_width = 340.0;
            let resp = ui.add(
                egui::Slider::new(&mut slider_boost, 10..=500)
                    .show_value(false)
                    .step_by(10.0)
                    .trailing_fill(true),
            );
            if resp.changed() {
                self.boost = slider_boost;
                for &step in &boost_presets {
                    if (self.boost as i32 - step as i32).abs() < 15 {
                        self.boost = step;
                        break;
                    }
                }
            }
            if resp.drag_stopped() && self.is_active {
                self.update_gain();
            }
        });

        ui.add_space(4.0);

        ui.horizontal(|ui| {
            for &preset in &boost_presets {
                let label = format!("{:.1}x", preset as f32 / 100.0);
                if ui.selectable_label(self.boost == preset, &label).clicked() {
                    self.boost = preset;
                    if self.is_active {
                        self.update_gain();
                    }
                }
            }
        });

        ui.add_space(4.0);

        // Auto-calibration section
        match &self.cal_phase {
            CalibrationPhase::Idle => {
                let cal_btn = ui.add_sized(
                    [340.0, 28.0],
                    egui::Button::new(
                        egui::RichText::new("Auto-Calibrate (detect my voice level)")
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(100, 80, 180)),
                );
                if cal_btn.clicked() {
                    self.start_calibration();
                }
            }
            CalibrationPhase::Listening => {
                let elapsed = self
                    .cal_start
                    .map(|s| s.elapsed().as_secs_f32())
                    .unwrap_or(0.0);
                let remaining = (5.0 - elapsed).max(0.0);

                ui.group(|ui| {
                    ui.set_width(340.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 60),
                        "Speak now at your normal volume:",
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(CALIBRATION_PHRASES[self.cal_phrase_idx]).italics(),
                    );
                    ui.add_space(4.0);

                    // Live level meter
                    let live_rms = *self.cal_rms_live.lock().unwrap();
                    let db = if live_rms > 0.0001 {
                        20.0 * live_rms.log10()
                    } else {
                        -80.0
                    };
                    // Map -60dB..0dB to 0..1
                    let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);

                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(340.0, 12.0), egui::Sense::hover());
                    let painter = ui.painter();
                    painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(40, 40, 40));
                    let bar_color = if level > 0.85 {
                        egui::Color32::from_rgb(220, 60, 60)
                    } else if level > 0.6 {
                        egui::Color32::from_rgb(60, 200, 60)
                    } else {
                        egui::Color32::from_rgb(60, 140, 200)
                    };
                    let bar_rect = egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(rect.width() * level, rect.height()),
                    );
                    painter.rect_filled(bar_rect, 3.0, bar_color);

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(format!("{:.1}s remaining", remaining));
                        if ui.small_button("Cancel").clicked() {
                            self.cancel_calibration();
                        }
                    });
                });
            }
            CalibrationPhase::Done {
                boost_pct,
                raw_db,
                boosted_db,
                target_db,
            } => {
                let boost_val = *boost_pct;
                let raw_db = *raw_db;
                let boosted_db = *boosted_db;
                let target_db = *target_db;
                ui.group(|ui| {
                    ui.set_width(340.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(100, 200, 100),
                        format!(
                            "Recommended boost: {:.1}x ({}%)",
                            boost_val as f32 / 100.0,
                            boost_val
                        ),
                    );

                    // Visual level comparison
                    ui.add_space(4.0);
                    let bar_width = 300.0;
                    // Map dBFS: -60..0 -> 0..1
                    let raw_frac = ((raw_db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let boosted_frac = ((boosted_db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let target_frac = ((target_db + 60.0) / 60.0).clamp(0.0, 1.0);

                    // Your voice level
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Your mic ").small());
                        let (rect, _) = ui
                            .allocate_exact_size(egui::vec2(bar_width, 10.0), egui::Sense::hover());
                        let p = ui.painter();
                        p.rect_filled(rect, 2.0, egui::Color32::from_rgb(40, 40, 40));
                        p.rect_filled(
                            egui::Rect::from_min_size(
                                rect.min,
                                egui::vec2(rect.width() * raw_frac, 10.0),
                            ),
                            2.0,
                            egui::Color32::from_rgb(80, 130, 200),
                        );
                        // Target marker line
                        let tx = rect.min.x + rect.width() * target_frac;
                        p.line_segment(
                            [egui::pos2(tx, rect.min.y), egui::pos2(tx, rect.max.y)],
                            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 200, 60)),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("           {:.1} dBFS", raw_db))
                                .small()
                                .color(egui::Color32::from_rgb(80, 130, 200)),
                        );
                    });

                    // Boosted level
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Boosted  ").small());
                        let (rect, _) = ui
                            .allocate_exact_size(egui::vec2(bar_width, 10.0), egui::Sense::hover());
                        let p = ui.painter();
                        p.rect_filled(rect, 2.0, egui::Color32::from_rgb(40, 40, 40));
                        p.rect_filled(
                            egui::Rect::from_min_size(
                                rect.min,
                                egui::vec2(rect.width() * boosted_frac, 10.0),
                            ),
                            2.0,
                            egui::Color32::from_rgb(60, 200, 60),
                        );
                        let tx = rect.min.x + rect.width() * target_frac;
                        p.line_segment(
                            [egui::pos2(tx, rect.min.y), egui::pos2(tx, rect.max.y)],
                            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 200, 60)),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("           {:.1} dBFS", boosted_db))
                                .small()
                                .color(egui::Color32::from_rgb(60, 200, 60)),
                        );
                        ui.label(
                            egui::RichText::new(format!("  | target: {:.1} dBFS", target_db))
                                .small()
                                .color(egui::Color32::from_rgb(255, 200, 60)),
                        );
                    });

                    if boost_val >= 500 {
                        ui.label(
                            egui::RichText::new(
                                "Capped at 5x. Use manual entry above for higher values.",
                            )
                            .small()
                            .color(egui::Color32::from_rgb(180, 180, 120)),
                        );
                    }
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if ui.button("Accept").clicked() {
                            self.boost = boost_val;
                            self.set_mode(AudioMode::Boost);
                            self.save_current_profile();
                            self.cal_phase = CalibrationPhase::Idle;
                        }
                        if ui.button("Re-calibrate").clicked() {
                            self.start_calibration();
                        }
                        if ui.button("Dismiss").clicked() {
                            self.cal_phase = CalibrationPhase::Idle;
                        }
                    });
                });
            }
        }

        ui.add_space(4.0);

        // Noise gate
        if self.ng_calibrating {
            let elapsed = self
                .ng_cal_start
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            let remaining = (3.0 - elapsed).max(0.0);
            ui.group(|ui| {
                ui.set_width(340.0);
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 60),
                    "Stay SILENT — capturing background noise...",
                );
                ui.label(format!("{:.1}s remaining", remaining));
                if ui.small_button("Cancel").clicked() {
                    self.cancel_noise_calibration();
                }
            });
        } else {
            let mut ng_changed = false;
            let mut calibrate_requested = false;
            ui.horizontal(|ui| {
                let mut gate = self.noise_gate.lock().unwrap();
                let is_calibrated = gate.is_calibrated();
                let floor_db = gate.noise_floor_db();

                let prev_enabled = gate.enabled;
                let mut enabled = gate.enabled;
                if ui.checkbox(&mut enabled, "Noise Gate")
                    .on_hover_text("Enable background-noise suppression. First use starts a three-second silent calibration. Applies in Boost mode.")
                    .changed() && enabled && !is_calibrated {
                    calibrate_requested = true;
                }
                gate.enabled = enabled && is_calibrated;
                if gate.enabled != prev_enabled {
                    ng_changed = true;
                }
                drop(gate);

                if is_calibrated {
                    ui.label(
                        egui::RichText::new(format!("floor: {:.0} dB", floor_db))
                            .small()
                            .color(if enabled {
                                egui::Color32::from_rgb(100, 200, 100)
                            } else {
                                egui::Color32::GRAY
                            }),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("(not calibrated)")
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                }
                if ui.small_button("Calibrate").clicked() {
                    self.start_noise_calibration();
                }
            });
            if ng_changed {
                self.save_current_profile();
                self.sync_audio_controls();
            }
            if calibrate_requested {
                self.start_noise_calibration();
            }
        }

        ui.add_space(4.0);

        ui.horizontal(|ui| {
            for mode in [AudioMode::Boost, AudioMode::Bypass, AudioMode::Mute] {
                if ui
                    .selectable_label(self.mode == mode, mode.label())
                    .clicked()
                {
                    self.set_mode(mode);
                    self.save_current_profile();
                }
            }
        });
        ui.label(
            egui::RichText::new(match self.mode {
                AudioMode::Boost if self.settings.peak_protection => {
                    "Noise gate → boost → peak protection"
                }
                AudioMode::Boost => "Noise gate → boost (peak protection off)",
                AudioMode::Bypass => "Original microphone level, without processing",
                AudioMode::Mute => "Silence sent to your virtual microphone",
            })
            .small(),
        );
        if ui.checkbox(&mut self.settings.peak_protection, "Boost protection (peak limiter)")
            .on_hover_text("Prevents boosted peaks from clipping. Recommended on. Applies in Boost mode; turning it off can distort loud audio.")
            .changed() {
            profiles::save_settings(&self.settings);
            self.sync_audio_controls();
        }
        if !self.settings.peak_protection && self.mode == AudioMode::Boost {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Protection off — loud audio may distort",
            );
        }
        let reduction = self.controls.reduction_db.get();
        if self.settings.peak_protection && reduction > 0.1 && self.mode == AudioMode::Boost {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("Peak protection: {:.1} dB reduction", reduction),
            );
        }
        if self.controls.clipped.load(Ordering::Relaxed) > 0 {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                "Microphone clipping detected — lower its input level",
            );
        }
        if !*self.pipeline_active.lock().unwrap() && ui.button("Reconnect audio now").clicked() {
            self.start_pipeline();
        }

        if self.is_active {
            ui.add_space(4.0);
            let cable_name = self
                .output_devices
                .get(self.selected_output)
                .map(|n| n.replace("CABLE Input", "CABLE Output"))
                .unwrap_or("CABLE Output".to_string());
            ui.colored_label(
                egui::Color32::from_rgb(100, 200, 100),
                format!(
                    "In Discord/Teams/etc, select \"{}\" as your mic",
                    cable_name
                ),
            );
        }

        // Live audio waveform visualizer — both waves overlaid
        if *self.pipeline_active.lock().unwrap() {
            ui.add_space(4.0);
            let wave_w = 370.0_f32.min(ui.available_width());
            let wave_h = 60.0;
            let in_color = egui::Color32::from_rgb(60, 140, 220);
            let out_color = egui::Color32::from_rgb(80, 220, 80);

            // Legend
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("---").small().color(in_color));
                ui.label(egui::RichText::new("Input").small());
                ui.label(egui::RichText::new("---").small().color(out_color));
                ui.label(
                    egui::RichText::new(match self.mode {
                        AudioMode::Boost => "Output (boosted)",
                        AudioMode::Bypass => "Output (original)",
                        AudioMode::Mute => "Output (muted)",
                    })
                    .small(),
                );
            });

            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(wave_w, wave_h), egui::Sense::hover());
            // Keep every visualizer shape inside the graph. This is especially
            // important when input and output overlap in 1x passthrough mode.
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 3.0, egui::Color32::from_rgb(20, 20, 25));

            let rms_to_frac = |rms: f32| -> f32 {
                if rms.is_finite() && rms > 0.0001 {
                    ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };

            let make_points = |history: &[f32]| -> Vec<egui::Pos2> {
                let n = history.len();
                if n < 2 {
                    return vec![];
                }
                history
                    .iter()
                    .enumerate()
                    .map(|(i, &rms)| {
                        let x = rect.min.x + (i as f32 / (n - 1) as f32) * rect.width();
                        let frac = rms_to_frac(rms);
                        let y = rect.max.y - frac * rect.height();
                        egui::pos2(x, y)
                    })
                    .collect()
            };

            // Draw filled area between input and output (shows the boost difference)
            let in_pts = make_points(&self.input_history);
            let out_pts = make_points(&self.output_history);

            if self.is_active && in_pts.len() >= 2 && out_pts.len() >= 2 {
                // Fill between the two curves to show boost amount
                let n = in_pts.len().min(out_pts.len());
                let fill_color = egui::Color32::from_rgba_premultiplied(40, 180, 40, 25);
                // Build polygon strips column by column; skip when curves cross
                // to avoid bowtie artifacts from convex_polygon on non-convex quads
                for i in 0..n - 1 {
                    let in_above_l = in_pts[i].y <= out_pts[i].y;
                    let in_above_r = in_pts[i + 1].y <= out_pts[i + 1].y;
                    if in_above_l != in_above_r {
                        // Curves cross in this segment — skip fill to avoid artifacts
                        continue;
                    }
                    // Equal or near-equal curves create a zero-area polygon. Some
                    // graphics backends can tessellate those into screen-spanning
                    // triangles, which is most visible when boost is turned off.
                    let left_gap = (in_pts[i].y - out_pts[i].y).abs();
                    let right_gap = (in_pts[i + 1].y - out_pts[i + 1].y).abs();
                    if left_gap.max(right_gap) < 0.5 {
                        continue;
                    }
                    let quad = vec![in_pts[i], in_pts[i + 1], out_pts[i + 1], out_pts[i]];
                    p.add(egui::Shape::convex_polygon(
                        quad,
                        fill_color,
                        egui::Stroke::NONE,
                    ));
                }
            }

            // Draw both traces even in passthrough mode, but outside the filled-area
            // condition so stopping boost still leaves a useful level display.
            if in_pts.len() >= 2 {
                p.add(egui::Shape::line(
                    in_pts,
                    egui::Stroke::new(1.5_f32, in_color),
                ));
            }
            if out_pts.len() >= 2 {
                p.add(egui::Shape::line(
                    out_pts,
                    egui::Stroke::new(2.0_f32, out_color),
                ));
            }

            // dB scale markers
            for &db in &[-40.0_f32, -20.0, -10.0] {
                let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let y = rect.max.y - frac * rect.height();
                p.line_segment(
                    [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                    egui::Stroke::new(0.5_f32, egui::Color32::from_rgb(50, 50, 55)),
                );
                p.text(
                    egui::pos2(rect.max.x - 22.0, y - 6.0),
                    egui::Align2::LEFT_TOP,
                    format!("{}dB", db as i32),
                    egui::FontId::new(8.0, egui::FontFamily::Monospace),
                    egui::Color32::from_rgb(80, 80, 90),
                );
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // Profiles
        ui.collapsing("Saved Profiles", |ui| {
            let current_device = self.input_devices.get(self.selected_input).cloned();
            let mut to_delete: Option<String> = None;
            let mut to_load: Option<(String, u32)> = None;

            if self.device_profiles.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "No saved profiles yet. Calibrate or start boost to save one.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
            } else {
                let mut names: Vec<String> = self.device_profiles.keys().cloned().collect();
                names.sort();
                for name in &names {
                    let profile = &self.device_profiles[name];
                    let is_current = current_device.as_deref() == Some(name.as_str());
                    ui.horizontal(|ui| {
                        // Shorten long device names
                        let short_name = if name.chars().count() > 30 {
                            format!("{}...", name.chars().take(27).collect::<String>())
                        } else {
                            name.clone()
                        };
                        let label =
                            format!("{} — {:.1}x", short_name, profile.boost as f32 / 100.0);
                        if is_current {
                            ui.label(
                                egui::RichText::new(&label)
                                    .small()
                                    .strong()
                                    .color(egui::Color32::from_rgb(100, 200, 100)),
                            );
                        } else if ui
                            .link(egui::RichText::new(&label).small())
                            .on_hover_text("Click to load this profile's boost level")
                            .clicked()
                        {
                            to_load = Some((name.clone(), profile.boost));
                        }
                        if ui
                            .small_button("x")
                            .on_hover_text("Delete profile")
                            .clicked()
                        {
                            to_delete = Some(name.clone());
                        }
                    });
                }
            }

            // Apply deferred actions
            if let Some(name) = to_delete {
                self.device_profiles.remove(&name);
                profiles::save(&self.device_profiles);
            }
            if let Some((_name, boost)) = to_load {
                self.boost = boost;
                if self.is_active {
                    self.update_gain();
                }
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .small_button("Save current")
                    .on_hover_text("Save boost for current mic")
                    .clicked()
                {
                    self.save_current_profile();
                }
            });
        });

        ui.add_space(4.0);

        // Test recording
        ui.horizontal(|ui| {
            let rec_text = if self.is_recording {
                let elapsed = self
                    .recording_start
                    .map(|s| s.elapsed().as_secs() as u32)
                    .unwrap_or(0);
                format!("Stop ({:02}:{:02})", elapsed / 60, elapsed % 60)
            } else {
                "Record Test".to_string()
            };

            if ui.button(&rec_text).clicked() {
                if self.is_recording {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }

            if ui
                .add_enabled(
                    self.last_recording.is_some() && !self.is_recording,
                    egui::Button::new("Play"),
                )
                .clicked()
            {
                self.play_recording();
            }

            if ui.button("Folder").clicked() {
                self.open_folder();
            }

            if ui.button("Refresh Devices").clicked() {
                self.last_device_check =
                    std::time::Instant::now() - std::time::Duration::from_secs(2);
                self.check_device_changes();
            }
        });

        ui.add_space(4.0);
        let status = self.status.lock().unwrap().clone();
        ui.label(&status);

        ui.collapsing("Preferences", |ui| {
            let mut startup = self.start_with_windows;
            if ui.checkbox(&mut startup, "Start with Windows").changed() {
                match desktop::set_start_with_windows(startup) {
                    Ok(()) => self.start_with_windows = startup,
                    Err(error) => {
                        *self.status.lock().unwrap() = format!("Startup setting failed: {error}")
                    }
                }
            }
            if ui
                .checkbox(&mut self.settings.close_to_tray, "Close window to tray")
                .changed()
            {
                profiles::save_settings(&self.settings);
            }
            ui.label(format!(
                "Remembered startup mode: {}",
                self.settings.startup_mode.label()
            ));
            ui.label("Minimizing keeps audio running in the tray.");
            if ui
                .add_enabled(self.tray.is_some(), egui::Button::new("Hide to tray"))
                .clicked()
            {
                self.hidden = true;
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            if ui.button("Quit Microboost").clicked() {
                if self.is_recording {
                    self.stop_recording();
                }
                self.save_current_profile();
                self.kill_pipeline();
                self.quitting = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        ui.collapsing("Audio diagnostics", |ui| {
            ui.label(format!(
                "Dropped input samples: {}",
                self.controls.dropped.load(Ordering::Relaxed)
            ));
            ui.label(format!(
                "Playback underruns: {}",
                self.controls.underruns.load(Ordering::Relaxed)
            ));
            ui.label(format!(
                "Recording samples missed: {}",
                self.controls.recording_dropped.load(Ordering::Relaxed)
            ));
            if ui.button("Reset meters").clicked() {
                self.controls.clipped.store(0, Ordering::Relaxed);
                self.controls.dropped.store(0, Ordering::Relaxed);
                self.controls.underruns.store(0, Ordering::Relaxed);
                self.controls.recording_dropped.store(0, Ordering::Relaxed);
            }
        });

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!(
                "v{} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("BUILD_TIMESTAMP")
            ))
            .small()
            .color(egui::Color32::from_rgb(90, 90, 100)),
        );
    }
}
