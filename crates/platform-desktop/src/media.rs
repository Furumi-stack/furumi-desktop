//! Cross-platform OS media session backed by souvlaki.

use std::time::{Duration, Instant};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Toggle,
    Play,
    Pause,
    Next,
    Previous,
    Stop,
}

pub struct MediaSession {
    controls: MediaControls,
    metadata: Option<(String, String, String, u64)>,
    playback: Option<(bool, bool)>,
    last_position_update: Option<Instant>,
}

impl MediaSession {
    pub fn new(on_command: impl Fn(MediaCommand) + Send + 'static) -> Option<Self> {
        let dbus_name = platform_dbus_name();
        let mut controls = MediaControls::new(PlatformConfig {
            display_name: "Furumi Desktop",
            dbus_name: &dbus_name,
            hwnd: platform_window(),
        })
        .ok()?;
        controls
            .attach(move |event| {
                let command = match event {
                    MediaControlEvent::Toggle => MediaCommand::Toggle,
                    MediaControlEvent::Play => MediaCommand::Play,
                    MediaControlEvent::Pause => MediaCommand::Pause,
                    MediaControlEvent::Next => MediaCommand::Next,
                    MediaControlEvent::Previous => MediaCommand::Previous,
                    MediaControlEvent::Stop => MediaCommand::Stop,
                    _ => return,
                };
                on_command(command);
            })
            .ok()?;
        Some(Self {
            controls,
            metadata: None,
            playback: None,
            last_position_update: None,
        })
    }

    pub fn update_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration_seconds: f64,
    ) {
        let duration_ms = duration_millis(duration_seconds);
        let next = (
            title.to_owned(),
            artist.to_owned(),
            album.to_owned(),
            duration_ms,
        );
        if self.metadata.as_ref() == Some(&next) {
            return;
        }
        let _ = self.controls.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: Some(album),
            duration: (duration_ms > 0).then(|| Duration::from_millis(duration_ms)),
            cover_url: None,
        });
        self.metadata = Some(next);
    }

    pub fn update_playback(&mut self, playing: bool, paused: bool, position_seconds: f64) {
        let next = (playing, paused);
        let state_changed = self.playback != Some(next);
        if !state_changed
            && self
                .last_position_update
                .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        let progress = Some(MediaPosition(Duration::from_millis(duration_millis(
            position_seconds,
        ))));
        let state = if !playing {
            MediaPlayback::Stopped
        } else if paused {
            MediaPlayback::Paused { progress }
        } else {
            MediaPlayback::Playing { progress }
        };
        let _ = self.controls.set_playback(state);
        self.playback = Some(next);
        self.last_position_update = Some(Instant::now());
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        let _ = self.controls.detach();
    }
}

fn duration_millis(seconds: f64) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        u64::try_from(Duration::from_secs_f64(seconds).as_millis()).unwrap_or(u64::MAX)
    } else {
        0
    }
}

fn platform_dbus_name() -> String {
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    {
        return format!("cy.hexor.furumi.desktop.instance{}", std::process::id());
    }
    #[allow(unreachable_code)]
    "cy.hexor.furumi.desktop".into()
}

#[cfg(not(windows))]
fn platform_window() -> Option<*mut std::ffi::c_void> {
    None
}

#[cfg(windows)]
fn platform_window() -> Option<*mut std::ffi::c_void> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, WNDCLASSW,
    };
    unsafe {
        let class_name: Vec<u16> = "furumi_desktop_media\0".encode_utf16().collect();
        let instance = GetModuleHandleW(std::ptr::null());
        let mut class: WNDCLASSW = core::mem::zeroed();
        class.lpfnWndProc = Some(DefWindowProcW);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return None;
        }
        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        (!window.is_null()).then_some(window.cast())
    }
}

#[cfg(test)]
mod tests {
    use super::duration_millis;

    #[test]
    fn invalid_media_durations_are_safe() {
        assert_eq!(duration_millis(f64::NAN), 0);
        assert_eq!(duration_millis(-1.0), 0);
        assert_eq!(duration_millis(1.25), 1_250);
    }
}
