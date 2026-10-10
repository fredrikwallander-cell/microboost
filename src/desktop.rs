use crossbeam_queue::ArrayQueue;
use eframe::egui;
use microboost::processing::AudioMode;
use std::sync::Arc;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
    RegKey,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

pub fn startup_command(executable: &std::path::Path) -> String {
    format!("\"{}\" --tray", executable.display())
}

pub fn starts_with_windows() -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|key| key.get_value::<String, _>("Microboost"))
        .map(|command| command == startup_command(&executable))
        .unwrap_or(false)
}

/// This function is invoked only by the user's Start with Windows checkbox.
pub fn set_start_with_windows(enabled: bool) -> Result<(), String> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
        .map_err(|e| e.to_string())?;
    if enabled {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value("Microboost", &startup_command(&executable))
            .map_err(|e| e.to_string())
    } else {
        match key.delete_value("Microboost") {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[derive(Clone, Copy)]
pub enum TrayAction {
    Show,
    Mode(AudioMode),
    Quit,
}

pub struct DesktopTray {
    icon: TrayIcon,
    pub actions: Arc<ArrayQueue<TrayAction>>,
    last_mode: Option<AudioMode>,
}

impl DesktopTray {
    pub fn new(ctx: egui::Context) -> Result<Self, String> {
        let menu = Menu::new();
        let show = MenuItem::new("Show Microboost", true, None);
        let boost = MenuItem::new("Boost", true, None);
        let bypass = MenuItem::new("Bypass (original microphone)", true, None);
        let mute = MenuItem::new("Mute", true, None);
        let quit = MenuItem::new("Quit Microboost", true, None);
        menu.append_items(&[
            &show,
            &PredefinedMenuItem::separator(),
            &boost,
            &bypass,
            &mute,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Microboost")
            .with_icon(Self::make_icon(AudioMode::Boost)?)
            .build()
            .map_err(|e| e.to_string())?;
        let actions = Arc::new(ArrayQueue::new(32));
        let events = actions.clone();
        let (show, boost, bypass, mute, quit) = (
            show.id().clone(),
            boost.id().clone(),
            bypass.id().clone(),
            mute.id().clone(),
            quit.id().clone(),
        );
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == show {
                Some(TrayAction::Show)
            } else if event.id == boost {
                Some(TrayAction::Mode(AudioMode::Boost))
            } else if event.id == bypass {
                Some(TrayAction::Mode(AudioMode::Bypass))
            } else if event.id == mute {
                Some(TrayAction::Mode(AudioMode::Mute))
            } else if event.id == quit {
                Some(TrayAction::Quit)
            } else {
                None
            };
            if let Some(action) = action {
                let _ = events.push(action);
                ctx.request_repaint();
            }
        }));
        Ok(Self {
            icon,
            actions,
            last_mode: None,
        })
    }

    fn make_icon(mode: AudioMode) -> Result<Icon, String> {
        let color = match mode {
            AudioMode::Boost => [60, 190, 95, 255],
            AudioMode::Bypass => [70, 150, 230, 255],
            AudioMode::Mute => [220, 70, 70, 255],
        };
        let mut rgba = vec![0_u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let microphone = (12..20).contains(&x) && (4..21).contains(&y);
                let stand = ((8..24).contains(&x) && y == 22)
                    || ((15..17).contains(&x) && (21..28).contains(&y))
                    || ((11..21).contains(&x) && (27..29).contains(&y));
                if microphone || stand {
                    rgba[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4].copy_from_slice(&color);
                }
            }
        }
        Icon::from_rgba(rgba, 32, 32).map_err(|e| e.to_string())
    }

    pub fn update_mode(&mut self, mode: AudioMode) {
        if self.last_mode == Some(mode) {
            return;
        }
        if let Ok(icon) = Self::make_icon(mode) {
            let _ = self.icon.set_icon(Some(icon));
        }
        let _ = self
            .icon
            .set_tooltip(Some(format!("Microboost — {}", mode.label())));
        self.last_mode = Some(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_paths_with_spaces_are_quoted() {
        assert_eq!(
            startup_command(std::path::Path::new(r"C:\My Apps\Microboost.exe")),
            r#""C:\My Apps\Microboost.exe" --tray"#
        );
    }
}
