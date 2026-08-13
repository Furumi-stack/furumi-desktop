//! Deterministic frontend state, reducers and UI projections.

use furumi_backend_api::{
    BackendCommand, BackendSnapshot, PlaybackRepeat, PlaybackStatus, RequestId,
};
use furumi_domain::{ArtistKey, QueueItemId, ReleaseKey, TrackKey};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Locale {
    #[default]
    En,
}

#[derive(Debug, Clone, Copy)]
pub struct Strings {
    pub app_name: &'static str,
    pub home: &'static str,
    pub search: &'static str,
    pub library: &'static str,
    pub queue: &'static str,
    pub recently_played: &'static str,
    pub made_for_listening: &'static str,
    pub empty_queue: &'static str,
    pub search_placeholder: &'static str,
}

pub const EN: Strings = Strings {
    app_name: "Furumi",
    home: "Home",
    search: "Search",
    library: "Your library",
    queue: "Queue",
    recently_played: "Recently played",
    made_for_listening: "Made for listening",
    empty_queue: "Your queue is empty",
    search_placeholder: "Artists, albums or tracks",
};

impl Locale {
    #[must_use]
    pub const fn strings(self) -> &'static Strings {
        match self {
            Self::En => &EN,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Home,
    Search,
    Library,
    Artist(ArtistKey),
    Release(ReleaseKey, Option<ArtistKey>),
    Playlist(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendState {
    pub screen: Screen,
    pub navigation_history: Vec<Screen>,
    pub navigation_forward: Vec<Screen>,
    pub queue_open: bool,
    pub settings_open: bool,
    pub devices_open: bool,
    pub search_query: String,
    pub locale: Locale,
    pub track_info: Option<TrackKey>,
    pub playlist_picker_track: Option<TrackKey>,
}

impl Default for FrontendState {
    fn default() -> Self {
        Self {
            screen: Screen::Home,
            navigation_history: Vec::new(),
            navigation_forward: Vec::new(),
            queue_open: false,
            settings_open: false,
            devices_open: false,
            search_query: String::new(),
            locale: Locale::En,
            track_info: None,
            playlist_picker_track: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppState {
    pub frontend: FrontendState,
    pub backend: BackendSnapshot,
    pub next_request_id: u64,
    pub transient_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    Navigate(Screen),
    Back,
    Forward,
    ToggleQueue,
    ToggleSettings,
    ToggleDevices,
    TogglePlayback,
    Next,
    Previous,
    ToggleShuffle,
    CycleRepeat,
    Seek(f64),
    SetVolume(f32),
    PlayRelease {
        release_id: ReleaseKey,
        start: usize,
    },
    PlayTrack(TrackKey),
    PlayQueueItem(QueueItemId),
    PlayContext {
        tracks: Vec<TrackKey>,
        selected: TrackKey,
    },
    AddNext(Vec<TrackKey>),
    AddToEnd(Vec<TrackKey>),
    ToggleLike(TrackKey),
    ShowPlaylistPicker(TrackKey),
    ClosePlaylistPicker,
    CreatePlaylist(String),
    AddToPlaylist {
        playlist_id: i64,
        tracks: Vec<TrackKey>,
    },
    CreateDeviceInvite,
    ConnectDevice(String),
    AnswerDevicePairing {
        request_id: String,
        accept: bool,
        use_requester_group: bool,
    },
    SelectPlaybackDevice(String),
    SearchChanged(String),
    NetworkIdChanged(String),
    DeviceNameChanged(String),
    LibraryPathChanged(String),
    FederationChanged(bool),
    SaveFederatedOnListenChanged(bool),
    LanguageChanged(String),
    ShowTrackInfo(TrackKey),
    CloseTrackInfo,
    DismissError,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    BackendSnapshot(Box<BackendSnapshot>),
    CommandRejected(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send(BackendCommand),
}

#[allow(
    clippy::too_many_lines,
    reason = "central exhaustive reducer keeps state transitions together"
)]
pub fn reduce_action(state: &mut AppState, action: UiAction) -> Vec<Effect> {
    match action {
        UiAction::Navigate(screen) => {
            if state.frontend.screen != screen {
                state
                    .frontend
                    .navigation_history
                    .push(state.frontend.screen.clone());
                state.frontend.navigation_forward.clear();
            }
            state.frontend.screen = screen.clone();
            state.next_request_id = state.next_request_id.saturating_add(1);
            let request_id = RequestId::new(state.next_request_id);
            match screen {
                Screen::Artist(key) => find_artist(state, &key).map_or_else(Vec::new, |artist| {
                    send(BackendCommand::LoadArtist {
                        request_id,
                        key,
                        name: artist.name.clone(),
                    })
                }),
                Screen::Release(key, preferred) => {
                    find_release(state, &key).map_or_else(Vec::new, |release| {
                        let selected_artist = preferred
                            .as_ref()
                            .and_then(|preferred| {
                                release
                                    .artists
                                    .iter()
                                    .find(|artist| &artist.key == preferred)
                                    .map(|artist| (artist.key.clone(), artist.name.clone()))
                                    .or_else(|| {
                                        find_artist(state, preferred)
                                            .map(|artist| (artist.key.clone(), artist.name.clone()))
                                    })
                            })
                            .or_else(|| {
                                release
                                    .artists
                                    .first()
                                    .map(|artist| (artist.key.clone(), artist.name.clone()))
                            });
                        selected_artist.map_or_else(Vec::new, |(artist_key, artist_name)| {
                            send(BackendCommand::LoadRelease {
                                request_id,
                                key,
                                artist_key,
                                artist_name,
                            })
                        })
                    })
                }
                _ => Vec::new(),
            }
        }
        UiAction::Back => {
            if let Some(screen) = state.frontend.navigation_history.pop() {
                state
                    .frontend
                    .navigation_forward
                    .push(state.frontend.screen.clone());
                state.frontend.screen = screen;
            }
            Vec::new()
        }
        UiAction::Forward => {
            if let Some(screen) = state.frontend.navigation_forward.pop() {
                state
                    .frontend
                    .navigation_history
                    .push(state.frontend.screen.clone());
                state.frontend.screen = screen;
            }
            Vec::new()
        }
        UiAction::ToggleQueue => {
            state.frontend.queue_open = !state.frontend.queue_open;
            Vec::new()
        }
        UiAction::ToggleSettings => {
            state.frontend.settings_open = !state.frontend.settings_open;
            if state.frontend.settings_open {
                state.frontend.devices_open = false;
            }
            Vec::new()
        }
        UiAction::ToggleDevices => {
            state.frontend.devices_open = !state.frontend.devices_open;
            if state.frontend.devices_open {
                state.frontend.settings_open = false;
            }
            Vec::new()
        }
        UiAction::TogglePlayback => send(BackendCommand::TogglePlayback),
        UiAction::Next => send(BackendCommand::Next),
        UiAction::Previous => send(BackendCommand::Previous),
        UiAction::ToggleShuffle => send(BackendCommand::ToggleShuffle),
        UiAction::CycleRepeat => send(BackendCommand::CycleRepeat),
        UiAction::Seek(position_seconds) => send(BackendCommand::Seek { position_seconds }),
        UiAction::SetVolume(volume) => send(BackendCommand::SetVolume { volume }),
        UiAction::PlayRelease { release_id, start } => {
            send(BackendCommand::PlayRelease { release_id, start })
        }
        UiAction::PlayTrack(track) => send(BackendCommand::PlayTrack { track }),
        UiAction::PlayQueueItem(item_id) => send(BackendCommand::PlayQueueItem { item_id }),
        UiAction::PlayContext { tracks, selected } => {
            send(BackendCommand::PlayContext { tracks, selected })
        }
        UiAction::AddNext(tracks) => send(BackendCommand::AddNext { tracks }),
        UiAction::AddToEnd(tracks) => send(BackendCommand::AddToEnd { tracks }),
        UiAction::ToggleLike(track) => send(BackendCommand::ToggleLike { track }),
        UiAction::ShowPlaylistPicker(track) => {
            state.frontend.playlist_picker_track = Some(track);
            Vec::new()
        }
        UiAction::ClosePlaylistPicker => {
            state.frontend.playlist_picker_track = None;
            Vec::new()
        }
        UiAction::CreatePlaylist(title) => send(BackendCommand::CreatePlaylist { title }),
        UiAction::AddToPlaylist {
            playlist_id,
            tracks,
        } => {
            state.frontend.playlist_picker_track = None;
            send(BackendCommand::AddToPlaylist {
                playlist_id,
                tracks,
            })
        }
        UiAction::CreateDeviceInvite => send(BackendCommand::CreateDeviceInvite),
        UiAction::ConnectDevice(invite) => send(BackendCommand::ConnectDevice { invite }),
        UiAction::AnswerDevicePairing {
            request_id,
            accept,
            use_requester_group,
        } => send(BackendCommand::AnswerDevicePairing {
            request_id,
            accept,
            use_requester_group,
        }),
        UiAction::SelectPlaybackDevice(device_id) => {
            send(BackendCommand::SelectPlaybackDevice { device_id })
        }
        UiAction::SearchChanged(query) => {
            let previous = active_search_request(&state.backend);
            state.frontend.search_query.clone_from(&query);
            if state.frontend.screen != Screen::Search {
                state
                    .frontend
                    .navigation_history
                    .push(state.frontend.screen.clone());
                state.frontend.navigation_forward.clear();
                state.frontend.screen = Screen::Search;
            }
            state.next_request_id = state.next_request_id.saturating_add(1);
            let request_id = RequestId::new(state.next_request_id);
            let mut effects = Vec::with_capacity(2);
            if let Some(previous) = previous {
                effects.push(Effect::Send(BackendCommand::CancelSearch {
                    request_id: previous,
                }));
            }
            effects.push(Effect::Send(BackendCommand::Search { request_id, query }));
            effects
        }
        UiAction::NetworkIdChanged(value) => {
            state.backend.settings.network_id = value;
            send(BackendCommand::UpdateSettings(
                state.backend.settings.clone(),
            ))
        }
        UiAction::DeviceNameChanged(value) => {
            state.backend.settings.device_name = value;
            send(BackendCommand::UpdateSettings(
                state.backend.settings.clone(),
            ))
        }
        UiAction::LibraryPathChanged(value) => {
            state.backend.settings.library_path = value;
            send(BackendCommand::UpdateSettings(
                state.backend.settings.clone(),
            ))
        }
        UiAction::FederationChanged(enabled) => {
            state.backend.settings.federation_enabled = enabled;
            send(BackendCommand::UpdateSettings(
                state.backend.settings.clone(),
            ))
        }
        UiAction::SaveFederatedOnListenChanged(enabled) => {
            state.backend.settings.save_federated_on_listen = enabled;
            send(BackendCommand::UpdateSettings(
                state.backend.settings.clone(),
            ))
        }
        UiAction::LanguageChanged(language) => {
            state.frontend.locale = Locale::En;
            state.backend.settings.language = language;
            send(BackendCommand::UpdateSettings(
                state.backend.settings.clone(),
            ))
        }
        UiAction::ShowTrackInfo(track) => {
            state.frontend.track_info = Some(track);
            Vec::new()
        }
        UiAction::CloseTrackInfo => {
            state.frontend.track_info = None;
            Vec::new()
        }
        UiAction::DismissError => {
            state.transient_error = None;
            Vec::new()
        }
    }
}

fn find_artist<'a>(state: &'a AppState, key: &ArtistKey) -> Option<&'a furumi_domain::Artist> {
    match &state.backend.library {
        furumi_backend_api::RemoteData::Ready(l) => Some(l),
        _ => None,
    }
    .into_iter()
    .flat_map(|l| l.artists.iter())
    .chain(state.backend.search.results.artists.iter())
    .find(|a| &a.key == key)
}

fn find_release<'a>(state: &'a AppState, key: &ReleaseKey) -> Option<&'a furumi_domain::Release> {
    match &state.backend.library {
        furumi_backend_api::RemoteData::Ready(l) => Some(l),
        _ => None,
    }
    .into_iter()
    .flat_map(|l| l.featured_releases.iter())
    .chain(state.backend.search.results.releases.iter())
    .find(|r| &r.key == key)
}

pub fn reduce_event(state: &mut AppState, event: AppEvent) {
    match event {
        AppEvent::BackendSnapshot(snapshot) => {
            if snapshot.revision >= state.backend.revision {
                state.backend = *snapshot;
            }
        }
        AppEvent::CommandRejected(message) => state.transient_error = Some(message),
    }
}

fn active_search_request(snapshot: &BackendSnapshot) -> Option<RequestId> {
    snapshot
        .search
        .federation_pending
        .then_some(snapshot.search.request_id)
        .flatten()
}

fn send(command: BackendCommand) -> Vec<Effect> {
    vec![Effect::Send(command)]
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerProjection {
    pub has_track: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub elapsed: String,
    pub duration: String,
    pub progress: f32,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: PlaybackRepeat,
}

impl AppState {
    #[must_use]
    pub fn player_projection(&self) -> PlayerProjection {
        let current = self.backend.queue.current().map(|item| &item.track);
        let duration = self.backend.playback.duration_seconds.max(0.0);
        let elapsed = self.backend.playback.position_seconds.clamp(0.0, duration);
        PlayerProjection {
            has_track: current.is_some(),
            playing: self.backend.playback.status == PlaybackStatus::Playing,
            title: current.map_or_else(|| "Nothing playing".into(), |track| track.title.clone()),
            artist: current.map_or_else(String::new, |track| track.artist.clone()),
            elapsed: duration_label(elapsed),
            duration: duration_label(duration),
            progress: progress_ratio(elapsed, duration),
            volume: self.backend.playback.volume,
            shuffle: self.backend.playback.shuffle,
            repeat: self.backend.playback.repeat,
        }
    }
}

#[must_use]
pub fn duration_label(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let total = Duration::from_secs_f64(seconds).as_secs();
    format!("{}:{:02}", total / 60, total % 60)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the clamped unit interval is the precision exposed by Slint"
)]
fn progress_ratio(elapsed: f64, duration: f64) -> f32 {
    if duration > 0.0 {
        (elapsed / duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use furumi_backend_api::{SearchResults, SearchSnapshot};
    use furumi_domain::{Artist, ArtistRef, Artwork, CatalogSource, Release, ReleaseId};

    #[test]
    fn a_new_search_cancels_the_active_request() {
        let mut state = AppState::default();
        state.backend.search = SearchSnapshot {
            request_id: Some(RequestId::new(9)),
            federation_pending: true,
            ..SearchSnapshot::default()
        };

        let effects = reduce_action(&mut state, UiAction::SearchChanged("ambient".into()));
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects[0],
            Effect::Send(BackendCommand::CancelSearch {
                request_id: RequestId::new(9)
            })
        );
        assert!(matches!(
            effects[1],
            Effect::Send(BackendCommand::Search { .. })
        ));
    }

    #[test]
    fn stale_backend_snapshots_are_ignored() {
        let mut state = AppState::default();
        state.backend.revision = 8;
        reduce_event(
            &mut state,
            AppEvent::BackendSnapshot(Box::new(BackendSnapshot {
                revision: 7,
                ..BackendSnapshot::default()
            })),
        );
        assert_eq!(state.backend.revision, 8);
    }

    #[test]
    fn detail_navigation_returns_to_the_previous_screen() {
        let mut state = AppState::default();
        let artist = ArtistKey::Federation {
            peer_id: "peer-a".into(),
            id: "artist-a".into(),
        };

        reduce_action(&mut state, UiAction::Navigate(Screen::Artist(artist)));
        reduce_action(&mut state, UiAction::Back);

        assert_eq!(state.frontend.screen, Screen::Home);
        assert!(state.frontend.navigation_history.is_empty());
    }

    #[test]
    fn forward_navigation_restores_the_screen_left_by_back() {
        let mut state = AppState::default();
        let artist = ArtistKey::Federation {
            peer_id: "peer-a".into(),
            id: "artist-a".into(),
        };
        let artist_screen = Screen::Artist(artist);

        reduce_action(&mut state, UiAction::Navigate(artist_screen.clone()));
        reduce_action(&mut state, UiAction::Back);
        reduce_action(&mut state, UiAction::Forward);

        assert_eq!(state.frontend.screen, artist_screen);
        assert!(state.frontend.navigation_forward.is_empty());
        assert_eq!(state.frontend.navigation_history, vec![Screen::Home]);
    }

    #[test]
    fn release_lookup_keeps_the_preferred_artist_name_and_key_together() {
        let mut state = AppState::default();
        let pasha_key = ArtistKey::Local(furumi_domain::ArtistId::new(1));
        let kunteynir_key = ArtistKey::Local(furumi_domain::ArtistId::new(2));
        let release_key = ReleaseKey::Local(ReleaseId::new(7));
        let artists = [
            (kunteynir_key.clone(), "KUNTEYNIR"),
            (pasha_key.clone(), "Паша Техник"),
        ];
        state.backend.search.results = SearchResults {
            artists: artists
                .iter()
                .map(|(key, name)| Artist {
                    key: key.clone(),
                    source: CatalogSource::Local,
                    name: (*name).into(),
                    artwork: Artwork::default(),
                    release_count: 1,
                    track_count: 1,
                })
                .collect(),
            releases: vec![Release {
                key: release_key.clone(),
                source: CatalogSource::Local,
                title: "Порядочный".into(),
                artists: artists
                    .iter()
                    .map(|(key, name)| ArtistRef {
                        key: key.clone(),
                        name: (*name).into(),
                    })
                    .collect(),
                featured_artists: Vec::new(),
                release_type: "album".into(),
                year: None,
                artwork: Artwork::default(),
                tracks: Vec::new(),
            }],
            tracks: Vec::new(),
        };

        let effects = reduce_action(
            &mut state,
            UiAction::Navigate(Screen::Release(release_key, Some(pasha_key.clone()))),
        );

        assert_eq!(
            effects,
            vec![Effect::Send(BackendCommand::LoadRelease {
                request_id: RequestId::new(1),
                key: ReleaseKey::Local(ReleaseId::new(7)),
                artist_key: pasha_key,
                artist_name: "Паша Техник".into(),
            })]
        );
    }
}
