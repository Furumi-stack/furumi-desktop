//! Slint presentation and the adapter to the application/backend state flow.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

use furumi_application::{
    AppEvent, AppState, Effect, Screen, UiAction, duration_label, reduce_action, reduce_event,
};
use furumi_backend::BackendHandle;
use furumi_backend_api::{
    BackendCommand, DevicePlaybackRole, DevicePresence, DeviceTrust, FederationOperation,
    PlaybackRepeat, PlaybackStatus, RemoteData,
};
use furumi_domain::{
    Artist, ArtistKey, ArtistRef, AudioSource, CatalogSource, ContentId, LocalTrackId, QueueItemId,
    Release, ReleaseKey, Track, TrackKey,
};
use furumi_platform_desktop::{MediaCommand, MediaSession};
use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};

slint::include_modules!();

mod render;
use render::{
    breadcrumb_screen, contributor_lines, find_artist_key, find_release_key, model,
    parse_track_key, release_artist_credits, render, render_catalog, render_current_track,
    render_playback, render_queue, render_search, render_shell, render_track_info,
    selected_release, track_context,
};
thread_local! {
    /// Decoded images belong to the Slint UI thread. Keeping them here makes
    /// every state projection after the first load a memory-only operation.
    static ARTWORK_CACHE: RefCell<HashMap<PathBuf, Option<Image>>> = RefCell::new(HashMap::new());
    static MEDIA_SESSION: RefCell<Option<MediaSession>> = const { RefCell::new(None) };
}

/// Runs the desktop presentation until its main window closes.
///
/// # Errors
///
/// Returns a Slint platform error when the window or event loop cannot start.
pub fn run(backend: &BackendHandle) -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;
    let state = Arc::new(Mutex::new(AppState::default()));

    // Winit completes NSApplication initialization only after entering the
    // event loop. AppKit discards a Dock icon assigned before that point.
    #[cfg(target_os = "macos")]
    slint::Timer::single_shot(std::time::Duration::ZERO, || {
        furumi_platform_desktop::set_application_icon();
    });

    let media_backend = backend.clone();
    MEDIA_SESSION.with_borrow_mut(|session| {
        *session = MediaSession::new(move |command| {
            let backend_command = match command {
                MediaCommand::Toggle => BackendCommand::TogglePlayback,
                MediaCommand::Play => BackendCommand::Play,
                MediaCommand::Pause => BackendCommand::Pause,
                MediaCommand::Next => BackendCommand::Next,
                MediaCommand::Previous => BackendCommand::Previous,
                MediaCommand::Stop => BackendCommand::Stop,
            };
            let _ = media_backend.try_send(backend_command);
        });
    });

    bind_callbacks(&window, &state, backend);
    subscribe_to_backend(&window, &state, backend);
    with_state(&state, |state| render(&window, state));
    if let Err(error) = backend.try_send(BackendCommand::Initialize) {
        dispatch_event(
            &window,
            &state,
            AppEvent::CommandRejected(error.to_string()),
        );
    }

    let result = window.run();
    MEDIA_SESSION.with_borrow_mut(Option::take);
    let _ = backend.try_send(BackendCommand::Shutdown);
    result
}

fn bind_callbacks(window: &AppWindow, state: &Arc<Mutex<AppState>>, backend: &BackendHandle) {
    bind_catalog_callbacks(window, state, backend);
    window.on_navigate({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |target| {
            let screen = match target.as_str() {
                "search" => Screen::Search,
                "library" => Screen::Library,
                _ => Screen::Home,
            };
            dispatch_action(&window, &state, &backend, UiAction::Navigate(screen));
        }
    });
    window.on_toggle_queue({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::ToggleQueue)
    });
    window.on_toggle_settings({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::ToggleSettings)
    });
    bind_player_callbacks(window, state, backend);
    window.on_search_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |query| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::SearchChanged(query.to_string()),
            );
        }
    });
    window.on_track_action({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |action, key| {
            let Some(key) = parse_track_key(&key) else {
                return;
            };
            let action = match action.as_str() {
                "like" => UiAction::ToggleLike(key),
                "play-next" => UiAction::AddNext(vec![key]),
                "add-end" => UiAction::AddToEnd(vec![key]),
                "information" => UiAction::ShowTrackInfo(key),
                "playlist" => UiAction::ShowPlaylistPicker(key),
                // The remaining presentation actions terminate here until
                // their backend capabilities are introduced.
                _ => return,
            };
            dispatch_action(&window, &state, &backend, action);
        }
    });
    bind_playlist_callbacks(window, state, backend);
    bind_device_callbacks(window, state, backend);
    bind_settings_callbacks(window, state, backend);
    window.on_dismiss_error({
        let window = window.as_weak();
        let state = Arc::clone(state);
        move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            with_state(&state, |state| {
                let _ = reduce_action(state, UiAction::DismissError);
                render(&window, state);
            });
        }
    });
    window.on_close_track_info({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::CloseTrackInfo)
    });
}

fn bind_playlist_callbacks(
    window: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    backend: &BackendHandle,
) {
    window.on_open_playlist({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |id| {
            if let Ok(id) = id.parse::<i64>() {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::Navigate(Screen::Playlist(id)),
                );
            }
        }
    });
    window.on_create_playlist({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |title| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::CreatePlaylist(title.to_string()),
            );
        }
    });
    window.on_add_track_to_playlist({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |playlist_id| {
            let Ok(playlist_id) = playlist_id.parse::<i64>() else {
                return;
            };
            let track = with_state(&state, |state| state.frontend.playlist_picker_track.clone());
            if let Some(track) = track {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::AddToPlaylist {
                        playlist_id,
                        tracks: vec![track],
                    },
                );
            }
        }
    });
    window.on_close_playlist_picker({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::ClosePlaylistPicker)
    });
    window.on_play_queue_item({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |id| {
            if let Ok(id) = id.parse::<u64>() {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::PlayQueueItem(QueueItemId::new(id)),
                );
            }
        }
    });
}

fn bind_catalog_callbacks(
    window: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    backend: &BackendHandle,
) {
    window.on_layout_detail_contributors({
        let window = window.as_weak();
        let state = Arc::clone(state);
        move |width| {
            let lines = with_state(&state, |state| {
                let Some(release) = selected_release(state) else {
                    return Vec::new();
                };
                let preferred = match &state.frontend.screen {
                    Screen::Release(_, preferred) => preferred.as_ref(),
                    _ => None,
                };
                let (_, contributors) = release_artist_credits(&release, preferred);
                contributor_lines(&contributors, width)
            });
            if let Some(window) = window.upgrade() {
                window.set_detail_contributor_lines(model(lines));
            }
        }
    });
    window.on_go_back({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::Back)
    });
    window.on_go_forward({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::Forward)
    });
    window.on_navigate_breadcrumb({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |target| {
            let screen = with_state(&state, |state| breadcrumb_screen(state, &target));
            if let Some(screen) = screen {
                dispatch_action(&window, &state, &backend, UiAction::Navigate(screen));
            }
        }
    });
    window.on_open_artist({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |key| {
            let artist = with_state(&state, |state| find_artist_key(state, &key));
            if let Some(key) = artist {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::Navigate(Screen::Artist(key)),
                );
            }
        }
    });
    window.on_open_release({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |key| {
            let (release, preferred_artist) = with_state(&state, |state| {
                let preferred_artist = match &state.frontend.screen {
                    Screen::Artist(key) => Some(key.clone()),
                    _ => None,
                };
                (find_release_key(state, &key), preferred_artist)
            });
            if let Some(key) = release {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::Navigate(Screen::Release(key, preferred_artist)),
                );
            }
        }
    });
}

fn bind_player_callbacks(
    window: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    backend: &BackendHandle,
) {
    window.on_toggle_playback({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::TogglePlayback)
    });
    window.on_next({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::Next)
    });
    window.on_previous({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::Previous)
    });
    window.on_toggle_shuffle({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::ToggleShuffle)
    });
    window.on_cycle_repeat({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::CycleRepeat)
    });
    window.on_seek({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |fraction| {
            let duration = with_state(&state, |state| state.backend.playback.duration_seconds);
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::Seek(f64::from(fraction) * duration),
            );
        }
    });
    window.on_set_volume({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |volume| {
            dispatch_action(&window, &state, &backend, UiAction::SetVolume(volume));
        }
    });
    window.on_play_release({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |key| {
            let release_id = with_state(&state, |state| find_release_key(state, &key));
            if let Some(release_id) = release_id {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::PlayRelease {
                        release_id,
                        start: 0,
                    },
                );
            }
        }
    });
    window.on_play_track({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |key| {
            if let Some(key) = parse_track_key(&key) {
                dispatch_action(&window, &state, &backend, UiAction::PlayTrack(key));
            }
        }
    });
    window.on_play_track_context({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |context, selected| {
            let Some(selected) = parse_track_key(&selected) else {
                return;
            };
            let tracks = with_state(&state, |state| track_context(state, context.as_str()));
            if tracks.iter().any(|track| track.matches(&selected)) {
                dispatch_action(
                    &window,
                    &state,
                    &backend,
                    UiAction::PlayContext { tracks, selected },
                );
            }
        }
    });
}

fn bind_settings_callbacks(
    window: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    backend: &BackendHandle,
) {
    window.on_network_id_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |value| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::NetworkIdChanged(value.to_string()),
            );
        }
    });
    window.on_device_name_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |value| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::DeviceNameChanged(value.to_string()),
            );
        }
    });
    window.on_library_path_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |value| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::LibraryPathChanged(value.to_string()),
            );
        }
    });
    bind_library_picker(window, state, backend);
    window.on_federation_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |enabled| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::FederationChanged(enabled),
            );
        }
    });
    window.on_save_federated_on_listen_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |enabled| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::SaveFederatedOnListenChanged(enabled),
            );
        }
    });
    window.on_language_changed({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |language| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::LanguageChanged(language.to_string()),
            );
        }
    });
}

fn bind_library_picker(window: &AppWindow, state: &Arc<Mutex<AppState>>, backend: &BackendHandle) {
    window.on_choose_library_path({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || {
            let initial = with_state(&state, |state| {
                PathBuf::from(&state.backend.settings.library_path)
            });
            let task_window = window.clone();
            let task_state = Arc::clone(&state);
            let task_backend = backend.clone();
            let result = thread::Builder::new()
                .name("furumi-folder-picker".into())
                .spawn(move || {
                    let selected =
                        furumi_platform_desktop::choose_library_directory(Some(&initial));
                    let Some(selected) = selected else {
                        return;
                    };
                    let selected = selected.to_string_lossy().into_owned();
                    let _ = task_window.upgrade_in_event_loop(move |window| {
                        dispatch_action(
                            &window.as_weak(),
                            &task_state,
                            &task_backend,
                            UiAction::LibraryPathChanged(selected),
                        );
                    });
                });
            if let Err(error) = result
                && let Some(window) = window.upgrade()
            {
                dispatch_event(
                    &window,
                    &state,
                    AppEvent::CommandRejected(format!("folder picker: {error}")),
                );
            }
        }
    });
}

fn bind_device_callbacks(
    window: &AppWindow,
    state: &Arc<Mutex<AppState>>,
    backend: &BackendHandle,
) {
    window.on_create_device_invite({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move || dispatch_action(&window, &state, &backend, UiAction::CreateDeviceInvite)
    });
    window.on_connect_device({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |invite| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::ConnectDevice(invite.to_string()),
            );
        }
    });
    window.on_answer_pairing({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |request_id, accept, use_requester_group| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::AnswerDevicePairing {
                    request_id: request_id.to_string(),
                    accept,
                    use_requester_group,
                },
            );
        }
    });
    window.on_select_device({
        let window = window.as_weak();
        let state = Arc::clone(state);
        let backend = backend.clone();
        move |device_id| {
            dispatch_action(
                &window,
                &state,
                &backend,
                UiAction::SelectPlaybackDevice(device_id.to_string()),
            );
        }
    });
}

fn subscribe_to_backend(window: &AppWindow, state: &Arc<Mutex<AppState>>, backend: &BackendHandle) {
    let mut snapshots = backend.subscribe();
    let bridge_window = window.as_weak();
    let bridge_state = Arc::clone(state);
    let result = thread::Builder::new()
        .name("furumi-ui-state-bridge".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    report_background_error(
                        &bridge_window,
                        bridge_state,
                        format!("UI state bridge: {error}"),
                    );
                    return;
                }
            };
            runtime.block_on(async move {
                while snapshots.changed().await.is_ok() {
                    let snapshot = snapshots.borrow_and_update().clone();
                    let state = Arc::clone(&bridge_state);
                    let result = bridge_window.upgrade_in_event_loop(move |window| {
                        dispatch_event(
                            &window,
                            &state,
                            AppEvent::BackendSnapshot(Box::new(snapshot)),
                        );
                    });
                    if result.is_err() {
                        break;
                    }
                }
            });
        });
    if let Err(error) = result {
        dispatch_event(
            window,
            state,
            AppEvent::CommandRejected(format!("UI state bridge: {error}")),
        );
    }
}

fn report_background_error(
    window: &slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    message: String,
) {
    let window = window.clone();
    let _ = window.upgrade_in_event_loop(move |window| {
        dispatch_event(&window, &state, AppEvent::CommandRejected(message));
    });
}

fn dispatch_action(
    window: &slint::Weak<AppWindow>,
    state: &Arc<Mutex<AppState>>,
    backend: &BackendHandle,
    action: UiAction,
) {
    let Some(window) = window.upgrade() else {
        return;
    };
    let effects = with_state(state, |state| {
        let effects = reduce_action(state, action);
        render(&window, state);
        effects
    });
    for Effect::Send(command) in effects {
        if let Err(error) = backend.try_send(command) {
            dispatch_event(&window, state, AppEvent::CommandRejected(error.to_string()));
        }
    }
}

fn dispatch_event(window: &AppWindow, state: &Arc<Mutex<AppState>>, event: AppEvent) {
    with_state(state, |state| {
        let previous = state.backend.clone();
        reduce_event(state, event);
        let shell_changed = previous.federation_activity != state.backend.federation_activity
            || previous.federation_debug != state.backend.federation_debug
            || previous.connected_devices != state.backend.connected_devices
            || previous.settings != state.backend.settings
            || previous.playback_error != state.backend.playback_error
            || previous.settings_error != state.backend.settings_error;
        let catalog_changed = previous.library != state.backend.library
            || previous.search != state.backend.search
            || previous.queue != state.backend.queue;
        let queue_changed = previous.queue != state.backend.queue;
        if shell_changed {
            render_shell(window, state);
        }
        if catalog_changed {
            render_catalog(window, state);
            render_search(window, state);
            render_track_info(window, state);
        }
        if queue_changed {
            render_queue(window, state);
            render_current_track(window, state);
        }
        render_playback(window, state);
    });
}

fn with_state<T>(state: &Arc<Mutex<AppState>>, operation: impl FnOnce(&mut AppState) -> T) -> T {
    let mut guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    operation(&mut guard)
}
