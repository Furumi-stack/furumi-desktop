//! Backend runtime and replaceable service implementations.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use furumi_backend_api::{
    BackendCommand, BackendSnapshot, BuildInfoSnapshot, ConnectedDeviceSnapshot,
    ConnectedDevicesSnapshot, DevicePlaybackRole, DevicePresence, DeviceTrust,
    FederationActivitySnapshot, FederationDebugSnapshot, FederationOperation, LibrarySnapshot,
    PendingPairingSnapshot, PlaybackRepeat, PlaybackStatus, PlaylistSnapshot, RemoteData,
    RequestId, SearchResults, SearchSnapshot, SearchStats, SendCommandError, SettingsSnapshot,
    SimilarityStatusSnapshot, VersionEntrySnapshot,
};
use furumi_domain::{
    Artist, ArtistId, ArtistKey, ArtistRef, Artwork, AudioSource, CatalogSource, ContentId,
    LocalTrackId, Release, ReleaseId, ReleaseKey, Track, TrackKey,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

mod actor_devices;
mod audio;
mod devices;
mod federation;
mod federation_similarity;
mod settings;
mod similarity;
mod streaming;
mod support;

use support::{
    apply_federated_metadata, extrapolated_control_position, federated_audio_directory,
    federation_specs, find_catalog_track, library_snapshot, library_track, local_search_results,
    merge_release_preserving_local, merge_search_results, normalize_device_name,
    playback_state_acknowledges_command, portable_playback_placeholder,
    remote_snapshot_has_authority, runtime_build_info, sanitize_filename, seconds_to_milliseconds,
    selected_track_position, spawn_settings_worker, track_is_liked, track_to_library_fed,
    track_to_playback_track, track_to_synced_fed, unix_time_ms, volume_percent,
};

use settings::SettingsStore;

const COMMAND_CAPACITY: usize = 64;
const INTERNAL_CAPACITY: usize = 32;
const CONTROL_COMMAND_ACK_TIMEOUT: Duration = Duration::from_secs(12);
const CONTROL_POSITION_ACK_TOLERANCE_SECONDS: f64 = 3.0;
const DESKTOP_APPLICATION_ID: &str = "furumi-desktop";
const SIMILARITY_RESULT_LIMIT: usize = 50;

type SimilarityCandidate = (
    Track,
    f32,
    Option<[u8; music_dht::similarity::SIMILARITY_SIGNATURE_BYTES]>,
);

#[derive(Clone)]
pub struct BackendHandle {
    commands: mpsc::Sender<BackendCommand>,
    snapshots: watch::Receiver<BackendSnapshot>,
}

impl BackendHandle {
    /// Enqueues a command without blocking the UI thread.
    ///
    /// # Errors
    ///
    /// Returns [`SendCommandError::Busy`] when the bounded queue is full and
    /// [`SendCommandError::Closed`] after backend shutdown.
    pub fn try_send(&self, command: BackendCommand) -> Result<(), SendCommandError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SendCommandError::Busy,
                mpsc::error::TrySendError::Closed(_) => SendCommandError::Closed,
            })
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<BackendSnapshot> {
        self.snapshots.clone()
    }
}

/// Starts the backend actor and real audio worker.
///
/// # Errors
///
/// Returns an I/O error when the runtime or its owner thread cannot be created.
pub fn spawn_backend() -> Result<BackendHandle, BackendStartupError> {
    // The music library deliberately keeps using furumi_library::default_db_path()
    // below. Everything else belongs to this client and must not reuse the TUI's
    // device identity, settings, federation node, or caches.
    let project_dirs = directories::ProjectDirs::from("cy", "hexor", DESKTOP_APPLICATION_ID)
        .ok_or(BackendStartupError::DataDirectoryUnavailable)?;
    let settings_path = project_dirs.data_local_dir().join("furumi-desktop.sqlite3");
    let federation_data_dir = project_dirs.data_local_dir().join("federation");
    let federation_media_dir = project_dirs.cache_dir().join("federation-media");
    let similarity_model_dir = project_dirs.cache_dir().join("similarity-models");
    let devices_db_path = project_dirs
        .data_local_dir()
        .join("devices")
        .join("sync.sqlite3");
    let library_db_path =
        furumi_library::default_db_path().map_err(BackendStartupError::Library)?;
    let default_library_path = library_db_path.with_file_name("federation-media");
    let settings_store = SettingsStore::open(&settings_path, &default_library_path)?;
    let loaded_settings = settings_store.load()?;
    let catalog = std::sync::Arc::new(
        furumi_library::Library::open(&library_db_path).map_err(BackendStartupError::Library)?,
    );
    let loaded_library = library_snapshot(&catalog).map_err(BackendStartupError::Library)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .enable_time()
        .thread_name("furumi-backend")
        .build()?;
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (snapshot_tx, snapshot_rx) = watch::channel(BackendSnapshot::default());
    let (internal_tx, internal_rx) = mpsc::channel(INTERNAL_CAPACITY);
    let devices = devices::DeviceSync::open(
        &devices_db_path,
        std::sync::Arc::clone(&catalog),
        internal_tx.clone(),
    )
    .map_err(BackendStartupError::Devices)?;
    let audio_events = internal_tx.clone();
    let audio = audio::spawn(move |event| {
        let _ = audio_events.blocking_send(InternalEvent::Audio(event));
    })?;

    thread::Builder::new()
        .name("furumi-backend-runtime".into())
        .spawn(move || {
            runtime.block_on(run_actor(ActorBootstrap {
                command_rx,
                snapshots: snapshot_tx,
                settings_store,
                loaded_settings,
                catalog,
                loaded_library,
                internal_tx,
                internal_rx,
                audio,
                federation_data_dir,
                federation_media_dir,
                similarity_model_dir,
                devices,
            }));
        })?;

    Ok(BackendHandle {
        commands: command_tx,
        snapshots: snapshot_rx,
    })
}

#[derive(Debug)]
pub enum BackendStartupError {
    DataDirectoryUnavailable,
    Io(io::Error),
    Database(rusqlite::Error),
    Library(anyhow::Error),
    Devices(anyhow::Error),
}

impl fmt::Display for BackendStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataDirectoryUnavailable => {
                formatter.write_str("application data directory is unavailable")
            }
            Self::Io(error) => write!(formatter, "backend runtime: {error}"),
            Self::Database(error) => write!(formatter, "settings database: {error}"),
            Self::Library(error) => write!(formatter, "music library: {error:#}"),
            Self::Devices(error) => write!(formatter, "connected devices: {error:#}"),
        }
    }
}

impl std::error::Error for BackendStartupError {}

impl From<io::Error> for BackendStartupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for BackendStartupError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

enum InternalEvent {
    SearchFinished {
        request_id: RequestId,
        results: Result<(SearchResults, SearchStats), String>,
    },
    DetailFinished {
        request_id: RequestId,
        key: ArtistKey,
        name: String,
        results: Result<(SearchResults, SearchStats), String>,
        release: Option<ReleaseKey>,
    },
    FederationStarted(Result<std::sync::Arc<federation::Client>, String>),
    SettingsPersisted(Result<(), String>),
    Audio(audio::Event),
    FederatedStreamReady {
        key: TrackKey,
        reader: streaming::GrowingFileReader,
        mime_type: String,
    },
    FederatedDownloadComplete {
        key: TrackKey,
        path: std::path::PathBuf,
        keep: bool,
        metadata: Option<federation::TrackMetadata>,
    },
    FederatedDownloadFailed {
        key: TrackKey,
        message: String,
    },
    FederatedContentResolved {
        key: TrackKey,
        result: Result<Track, String>,
    },
    QueueArtworkResolved {
        request_id: String,
        keys: Vec<TrackKey>,
        cover_uri: Option<String>,
    },
    HistoryTrackResolved {
        content_id: String,
        result: Result<Track, String>,
    },
    FederationDebugUpdated(FederationDebugSnapshot),
    DevicesChanged,
    DeviceLibraryChanged,
    DeviceNamePublishDue(String),
    DeviceNamePublished(Result<(), String>),
    DeviceOperationFinished(Result<DeviceOperationResult, String>),
    DevicePlaybackSnapshot(music_dht::device_sync::PlaybackSnapshot),
    DevicePlaybackCommand(music_dht::device_sync::PlaybackCommand),
    SimilarityStatus(similarity::SimilarityStatus),
    SimilarityProfileActivated(Option<String>),
    SimilaritySearchFinished {
        source_title: String,
        result: Result<Vec<Track>, String>,
    },
}

enum DeviceOperationResult {
    Invite(String),
    Connected(String),
}

struct Actor {
    state: BackendSnapshot,
    library: LibrarySnapshot,
    snapshots: watch::Sender<BackendSnapshot>,
    internal: mpsc::Sender<InternalEvent>,
    active_search: Option<(RequestId, CancellationToken)>,
    active_detail: Option<RequestId>,
    settings: std_mpsc::Sender<SettingsSnapshot>,
    catalog: std::sync::Arc<furumi_library::Library>,
    audio: audio::Controller,
    federation: Option<std::sync::Arc<federation::Client>>,
    federation_data_dir: std::path::PathBuf,
    federation_media_dir: std::path::PathBuf,
    similarity: std::sync::Arc<similarity::Manager>,
    ephemeral_audio: Option<(TrackKey, std::path::PathBuf)>,
    pending_queue_artwork: HashSet<String>,
    pending_history_resolutions: HashSet<String>,
    federation_debug_pending: bool,
    devices: std::sync::Arc<devices::DeviceSync>,
    device_service: Option<std::sync::Arc<music_dht::MusicDhtService>>,
    device_role: DevicePlaybackRole,
    active_device_id: String,
    active_device_name: String,
    control_anchor: Option<ControlPlaybackAnchor>,
    pending_control: Option<PendingControlState>,
    listen_session: Option<ListenSession>,
}

#[derive(Debug, Clone)]
struct ListenSession {
    id: String,
    track: Track,
    started_at_ms: i64,
}

#[derive(Debug, Clone)]
struct ControlPlaybackAnchor {
    device_id: String,
    state: music_dht::device_sync::PlaybackStateWire,
    observed_at: Instant,
}

#[derive(Debug, Clone)]
struct PendingControlState {
    device_id: String,
    state: music_dht::device_sync::PlaybackStateWire,
    seek: bool,
    sent_at: Instant,
}

struct ActorBootstrap {
    command_rx: mpsc::Receiver<BackendCommand>,
    snapshots: watch::Sender<BackendSnapshot>,
    settings_store: SettingsStore,
    loaded_settings: SettingsSnapshot,
    catalog: std::sync::Arc<furumi_library::Library>,
    loaded_library: LibrarySnapshot,
    internal_tx: mpsc::Sender<InternalEvent>,
    internal_rx: mpsc::Receiver<InternalEvent>,
    audio: audio::Controller,
    federation_data_dir: std::path::PathBuf,
    federation_media_dir: std::path::PathBuf,
    similarity_model_dir: std::path::PathBuf,
    devices: std::sync::Arc<devices::DeviceSync>,
}

struct ActorRuntime {
    actor: Actor,
    command_rx: mpsc::Receiver<BackendCommand>,
    internal_rx: mpsc::Receiver<InternalEvent>,
}

fn initialize_actor(bootstrap: ActorBootstrap) -> ActorRuntime {
    let ActorBootstrap {
        command_rx,
        snapshots,
        settings_store,
        mut loaded_settings,
        catalog,
        loaded_library,
        internal_tx,
        internal_rx,
        audio,
        federation_data_dir,
        federation_media_dir,
        similarity_model_dir,
        devices,
    } = bootstrap;
    let (settings_tx, settings_rx) = std_mpsc::channel();
    spawn_settings_worker(settings_store, settings_rx, internal_tx.clone());
    let library = loaded_library;
    let (active_device_id, mut active_device_name) = devices.identity().unwrap_or_default();
    let mut settings_error = None;
    let configured_device_name = if loaded_settings.device_name.trim().is_empty() {
        active_device_name.clone()
    } else {
        normalize_device_name(&loaded_settings.device_name)
    };
    if configured_device_name != active_device_name
        && let Err(error) = devices.set_device_name(&configured_device_name, None)
    {
        settings_error = Some(format!("device name: {error:#}"));
    } else if !configured_device_name.is_empty() {
        active_device_name.clone_from(&configured_device_name);
    }
    if loaded_settings.device_name != configured_device_name {
        loaded_settings.device_name = configured_device_name;
        let _ = settings_tx.send(loaded_settings.clone());
    }
    let initial_state = BackendSnapshot {
        settings: loaded_settings,
        build_info: runtime_build_info(),
        settings_error,
        ..BackendSnapshot::default()
    };
    let similarity = similarity::Manager::new(
        std::sync::Arc::clone(&catalog),
        internal_tx.clone(),
        initial_state.settings.similarity.clone(),
        similarity_model_dir,
    );
    let mut actor = Actor {
        state: initial_state,
        library,
        snapshots,
        internal: internal_tx,
        active_search: None,
        active_detail: None,
        settings: settings_tx,
        catalog,
        audio,
        federation: None,
        federation_data_dir,
        federation_media_dir,
        similarity,
        ephemeral_audio: None,
        pending_queue_artwork: HashSet::new(),
        pending_history_resolutions: HashSet::new(),
        federation_debug_pending: false,
        devices,
        device_service: None,
        device_role: DevicePlaybackRole::Active,
        active_device_id,
        active_device_name,
        control_anchor: None,
        pending_control: None,
        listen_session: None,
    };
    actor.state.similarity_status = similarity_status_snapshot(&actor.similarity.status());
    if actor.state.settings.similarity.enabled {
        actor.similarity.start();
    }
    actor.refresh_connected_devices();
    ActorRuntime {
        actor,
        command_rx,
        internal_rx,
    }
}

fn similarity_status_snapshot(status: &similarity::SimilarityStatus) -> SimilarityStatusSnapshot {
    SimilarityStatusSnapshot {
        phase: status.phase.label().into(),
        active_profile: status.active_profile.clone(),
        target_profile: status.target_profile.clone(),
        model: status.model.clone(),
        total_tracks: status.total_tracks,
        completed_tracks: status.completed_tracks,
        failed_tracks: status.failed_tracks,
        stored_vectors: status.stored_vectors,
        stored_bytes: status.stored_bytes,
        current_track: status.current_track.clone(),
        error: status.last_error.clone(),
    }
}

async fn run_actor(bootstrap: ActorBootstrap) {
    let ActorRuntime {
        mut actor,
        mut command_rx,
        mut internal_rx,
    } = initialize_actor(bootstrap);
    let mut playback_tick = tokio::time::interval(Duration::from_millis(250));
    playback_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut federation_debug_tick = tokio::time::interval(Duration::from_secs(3));
    federation_debug_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                if matches!(command, BackendCommand::Shutdown) {
                    break;
                }
                actor.handle_command(command);
            }
            event = internal_rx.recv() => {
                if let Some(event) = event {
                    actor.handle_internal(event);
                }
            }
            _ = playback_tick.tick() => actor.tick_playback(),
            _ = federation_debug_tick.tick() => actor.refresh_federation_debug(),
        }
    }
    if let Some((_, token)) = actor.active_search.take() {
        token.cancel();
    }
    actor.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
    actor.audio.stop();
}

impl Actor {
    fn start_federation(&mut self) {
        if !self.state.settings.federation_enabled
            || self.state.settings.network_id.trim().is_empty()
        {
            self.federation = None;
            self.federation_debug_pending = false;
            self.state.federation_debug = FederationDebugSnapshot::default();
            return;
        }
        let data_dir = self.federation_data_dir.clone();
        let media_dir = self.federation_media_dir.clone();
        let network = self.state.settings.network_id.trim().to_owned();
        let similarity = std::sync::Arc::clone(&self.similarity);
        let internal = self.internal.clone();
        self.state.federation_activity = FederationActivitySnapshot {
            operation: FederationOperation::Idle,
            pending: true,
            stats: None,
            error: None,
        };
        self.publish();
        tokio::spawn(async move {
            let result = federation::Client::start(data_dir, media_dir, &network, similarity)
                .await
                .map_err(|error| format!("federation: {error:#}"));
            let _ = internal
                .send(InternalEvent::FederationStarted(result))
                .await;
        });
    }

    fn refresh_federation_debug(&mut self) {
        if self.federation_debug_pending {
            return;
        }
        let Some(client) = self.federation.clone() else {
            return;
        };
        self.federation_debug_pending = true;
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let debug = client.debug_snapshot().await;
            let _ = internal
                .send(InternalEvent::FederationDebugUpdated(debug))
                .await;
        });
    }

    fn resolve_queue_artwork(&mut self) {
        let missing = self
            .state
            .queue
            .items()
            .iter()
            .filter(|item| item.track.cover_uri.is_none())
            .map(|item| item.track.clone())
            .collect::<Vec<_>>();
        let known = missing
            .iter()
            .filter_map(|track| {
                self.known_cover_uri(track)
                    .map(|cover| (track.key.clone(), cover))
            })
            .collect::<Vec<_>>();
        for (key, cover_uri) in known {
            if let Some(mut track) = self
                .state
                .queue
                .items()
                .iter()
                .find(|item| item.track.key.matches(&key))
                .map(|item| item.track.clone())
            {
                track.cover_uri = Some(cover_uri);
                self.state.queue.replace_track(&key, &track);
            }
        }

        let Some(client) = self.federation.clone() else {
            return;
        };
        let mut requests = HashMap::<String, (Track, Vec<TrackKey>)>::new();
        for track in self
            .state
            .queue
            .items()
            .iter()
            .filter(|item| {
                item.track.cover_uri.is_none()
                    && matches!(item.track.audio_source, AudioSource::Federation { .. })
            })
            .map(|item| item.track.clone())
            .collect::<Vec<_>>()
        {
            let Some(request_id) = Self::queue_artwork_request_id(&track) else {
                continue;
            };
            requests
                .entry(request_id)
                .and_modify(|(_, keys)| keys.push(track.key.clone()))
                .or_insert_with(|| (track.clone(), vec![track.key.clone()]));
        }
        for (request_id, (track, keys)) in requests {
            if !self.pending_queue_artwork.insert(request_id.clone()) {
                continue;
            }
            let internal = self.internal.clone();
            let client = client.clone();
            tokio::spawn(async move {
                let cover_uri = client
                    .artwork_for_track(&track)
                    .await
                    .map(|path| path.to_string_lossy().into_owned());
                let _ = internal
                    .send(InternalEvent::QueueArtworkResolved {
                        request_id,
                        keys,
                        cover_uri,
                    })
                    .await;
            });
        }
    }

    fn queue_artwork_request_id(track: &Track) -> Option<String> {
        let peer = match &track.audio_source {
            AudioSource::Federation { peer_id, .. } if !peer_id.is_empty() => peer_id.as_str(),
            _ => track.key.federation_id()?.0,
        };
        let artist = track
            .artists
            .first()
            .map(|artist| artist.name.as_str())
            .or_else(|| (!track.artist.is_empty()).then_some(track.artist.as_str()))?;
        (!track.release.is_empty()).then(|| {
            format!(
                "{peer}|{}|{}",
                music_dht::normalize_name(artist),
                music_dht::normalize_name(&track.release)
            )
        })
    }

    fn known_cover_uri(&self, track: &Track) -> Option<String> {
        let cover_from_release = |release: &Release| {
            let same_release = release.key == track.release_id
                || (music_dht::normalize_name(&release.title)
                    == music_dht::normalize_name(&track.release)
                    && release.tracks.iter().any(|candidate| {
                        candidate.same_catalog_track(track)
                            || candidate
                                .artists
                                .iter()
                                .any(|artist| track.artists.contains(artist))
                    }));
            same_release
                .then(|| release.artwork.uri.clone())
                .flatten()
                .or_else(|| {
                    release
                        .tracks
                        .iter()
                        .find(|candidate| candidate.same_catalog_track(track))
                        .and_then(|candidate| candidate.cover_uri.clone())
                })
        };
        self.library
            .featured_releases
            .iter()
            .chain(self.state.search.results.releases.iter())
            .find_map(cover_from_release)
            .or_else(|| {
                let request_id = Self::queue_artwork_request_id(track)?;
                self.state
                    .queue
                    .items()
                    .iter()
                    .find(|item| {
                        item.track.cover_uri.is_some()
                            && Self::queue_artwork_request_id(&item.track).as_deref()
                                == Some(request_id.as_str())
                    })
                    .and_then(|item| item.track.cover_uri.clone())
            })
            .or_else(|| {
                self.library
                    .playlists
                    .iter()
                    .flat_map(|playlist| playlist.tracks.iter())
                    .chain(self.library.recently_played.iter())
                    .chain(self.state.search.results.tracks.iter())
                    .find(|candidate| candidate.same_catalog_track(track))
                    .and_then(|candidate| candidate.cover_uri.clone())
            })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "central exhaustive actor command dispatcher"
    )]
    fn handle_command(&mut self, command: BackendCommand) {
        match command {
            BackendCommand::Initialize => {
                if let Ok(library) = library_snapshot(&self.catalog) {
                    self.library = library;
                }
                self.state.library = RemoteData::Ready(self.library.clone());
                self.publish();
                self.start_federation();
            }
            BackendCommand::TogglePlayback => match self.state.playback.status {
                PlaybackStatus::Stopped => self.play_current(),
                PlaybackStatus::Paused => {
                    self.state.playback.status = PlaybackStatus::Playing;
                    if self.device_role == DevicePlaybackRole::Active {
                        self.audio.resume();
                    } else {
                        self.send_control_state(false);
                    }
                    self.publish();
                }
                PlaybackStatus::Playing => {
                    self.state.playback.status = PlaybackStatus::Paused;
                    if self.device_role == DevicePlaybackRole::Active {
                        self.audio.pause();
                    } else {
                        self.send_control_state(false);
                    }
                    self.publish();
                }
            },
            BackendCommand::Play => match self.state.playback.status {
                PlaybackStatus::Stopped => self.play_current(),
                PlaybackStatus::Paused => {
                    self.state.playback.status = PlaybackStatus::Playing;
                    if self.device_role == DevicePlaybackRole::Active {
                        self.audio.resume();
                    } else {
                        self.send_control_state(false);
                    }
                    self.publish();
                }
                PlaybackStatus::Playing => {}
            },
            BackendCommand::Pause => {
                if self.state.playback.status == PlaybackStatus::Playing {
                    self.state.playback.status = PlaybackStatus::Paused;
                    if self.device_role == DevicePlaybackRole::Active {
                        self.audio.pause();
                    } else {
                        self.send_control_state(false);
                    }
                    self.publish();
                }
            }
            BackendCommand::Stop => {
                self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
                if self.device_role == DevicePlaybackRole::Active {
                    self.audio.stop();
                }
                self.remove_ephemeral_audio();
                self.state.playback.status = PlaybackStatus::Stopped;
                self.state.playback.position_seconds = 0.0;
                self.send_control_state(true);
                self.publish();
            }
            BackendCommand::Seek { position_seconds } => {
                self.state.playback.position_seconds =
                    position_seconds.clamp(0.0, self.state.playback.duration_seconds.max(0.0));
                if self.device_role == DevicePlaybackRole::Active {
                    self.audio.seek(Duration::from_secs_f64(
                        self.state.playback.position_seconds,
                    ));
                } else {
                    self.send_control_state(true);
                }
                self.publish();
            }
            BackendCommand::SetVolume { volume } => {
                self.state.playback.volume = volume.clamp(0.0, 1.0);
                if self.device_role == DevicePlaybackRole::Active {
                    self.audio.set_volume(self.state.playback.volume);
                } else {
                    self.send_control_state(false);
                }
                self.publish();
            }
            BackendCommand::ToggleShuffle => {
                self.state.playback.shuffle = !self.state.playback.shuffle;
                if self.state.playback.shuffle {
                    self.state.queue.shuffle_upcoming();
                } else {
                    self.state.queue.restore_upcoming_order();
                }
                self.send_control_state(false);
                self.publish();
            }
            BackendCommand::CycleRepeat => {
                self.state.playback.repeat = self.state.playback.repeat.next();
                self.send_control_state(false);
                self.publish();
            }
            BackendCommand::PlayRelease { release_id, start } => {
                if let Some(release) = self.release(&release_id).cloned() {
                    self.state.queue.replace_context(release.tracks, start);
                    self.shuffle_new_context();
                    self.resolve_queue_artwork();
                    self.play_current();
                }
            }
            BackendCommand::PlayTrack { track } => {
                if let Some(track) = self.track(&track).cloned() {
                    self.state.queue.replace_context(vec![track], 0);
                    self.shuffle_new_context();
                    self.resolve_queue_artwork();
                    self.play_current();
                }
            }
            BackendCommand::PlayQueueItem { item_id } => {
                if self.state.queue.select_item(item_id).is_some() {
                    self.play_current();
                }
            }
            BackendCommand::MoveQueueItem {
                item_id,
                target_index,
            } => {
                if self.state.queue.move_item(item_id, target_index) {
                    self.send_control_state(false);
                    self.publish();
                }
            }
            BackendCommand::RemoveQueueItem { item_id } => {
                if let Some(removed_current) = self.state.queue.remove_item(item_id) {
                    if removed_current {
                        self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
                        self.remove_ephemeral_audio();
                        if self.state.queue.current().is_some() {
                            self.play_current();
                        } else {
                            self.audio.stop();
                            self.state.playback.status = PlaybackStatus::Stopped;
                            self.state.playback.position_seconds = 0.0;
                            self.state.playback.duration_seconds = 0.0;
                            self.send_control_state(true);
                            self.publish();
                        }
                    } else {
                        self.send_control_state(false);
                        self.publish();
                    }
                }
            }
            BackendCommand::PlayContext { tracks, selected } => {
                let tracks = self.resolve_tracks(&tracks);
                if !tracks.is_empty() {
                    let start = selected_track_position(&tracks, &selected);
                    self.state.queue.replace_context(tracks, start);
                    self.shuffle_new_context();
                    self.resolve_queue_artwork();
                    self.play_current();
                }
            }
            BackendCommand::AddNext { tracks } => {
                let tracks = self.resolve_tracks(&tracks);
                self.state.queue.add_next(tracks);
                self.resolve_queue_artwork();
                self.send_control_state(false);
                self.publish();
            }
            BackendCommand::AddToEnd { tracks } => {
                let tracks = self.resolve_tracks(&tracks);
                self.state.queue.add_to_end(tracks);
                self.resolve_queue_artwork();
                self.send_control_state(false);
                self.publish();
            }
            BackendCommand::ToggleLike { track } => self.toggle_like(&track),
            BackendCommand::CreatePlaylist { title } => {
                let title = title.trim();
                if title.is_empty() {
                    return;
                }
                match self.catalog.create_playlist(title) {
                    Ok(playlist) => {
                        if let Err(error) = self.devices.record_playlist_created(playlist.id, title)
                        {
                            self.state.settings_error = Some(format!("playlist sync: {error:#}"));
                        }
                        self.refresh_library();
                    }
                    Err(error) => {
                        self.state.settings_error = Some(format!("create playlist: {error:#}"));
                        self.publish();
                    }
                }
            }
            BackendCommand::RenamePlaylist { playlist_id, title } => {
                let title = title.trim();
                if title.is_empty() {
                    return;
                }
                let result = self
                    .catalog
                    .update_playlist(playlist_id, title, None)
                    .and_then(|()| self.devices.record_playlist_renamed(playlist_id, title));
                if let Err(error) = result {
                    self.state.settings_error = Some(format!("rename playlist: {error:#}"));
                }
                self.refresh_library();
            }
            BackendCommand::DeletePlaylist { playlist_id } => {
                let result = self
                    .catalog
                    .playlist_sync_id(playlist_id)
                    .and_then(|sync_id| {
                        self.catalog.delete_playlist(playlist_id)?;
                        if let Some(sync_id) = sync_id {
                            self.devices.record_playlist_deleted(sync_id)?;
                        }
                        Ok(())
                    });
                if let Err(error) = result {
                    self.state.settings_error = Some(format!("delete playlist: {error:#}"));
                }
                self.refresh_library();
            }
            BackendCommand::AddToPlaylist {
                playlist_id,
                tracks,
            } => self.add_to_playlist(playlist_id, &tracks),
            BackendCommand::RemoveFromPlaylist {
                playlist_id,
                tracks,
            } => self.remove_from_playlist(playlist_id, &tracks),
            BackendCommand::CreateDeviceInvite => self.create_device_invite(),
            BackendCommand::ConnectDevice { invite } => self.connect_device(invite),
            BackendCommand::AnswerDevicePairing {
                request_id,
                accept,
                use_requester_group,
            } => {
                if let Err(error) =
                    self.devices
                        .answer_pairing(&request_id, accept, use_requester_group)
                {
                    self.state.connected_devices.error = Some(format!("pairing: {error:#}"));
                }
                self.refresh_connected_devices();
                self.publish();
            }
            BackendCommand::SelectPlaybackDevice { device_id } => {
                self.select_playback_device(&device_id);
            }
            BackendCommand::Next => {
                if self.advance_queue(false) {
                    self.play_current();
                }
            }
            BackendCommand::Previous => {
                if self.state.playback.position_seconds > 5.0 {
                    self.state.playback.position_seconds = 0.0;
                    if self.device_role == DevicePlaybackRole::Active {
                        self.audio.seek(Duration::ZERO);
                    } else {
                        self.send_control_state(true);
                    }
                    self.publish();
                } else if self.state.queue.previous().is_some() {
                    self.play_current();
                }
            }
            BackendCommand::Search { request_id, query } => self.start_search(request_id, query),
            BackendCommand::SearchSimilar { track } => self.start_similarity_search(&track),
            BackendCommand::ClearSimilarity => self.similarity.clear(),
            BackendCommand::CancelSearch { request_id } => self.cancel_search(request_id),
            BackendCommand::LoadArtist {
                request_id,
                key,
                name,
            } => self.load_detail(request_id, key, name, None),
            BackendCommand::LoadRelease {
                request_id,
                key,
                artist_key,
                artist_name,
            } => self.load_detail(request_id, artist_key, artist_name, Some(key)),
            BackendCommand::UpdateSettings(mut settings) => {
                settings.similarity = settings.similarity.normalized();
                let federation_changed = self.state.settings.federation_enabled
                    != settings.federation_enabled
                    || self.state.settings.network_id != settings.network_id;
                let device_name_changed = self.state.settings.device_name != settings.device_name;
                let similarity_changed = self.state.settings.similarity != settings.similarity;
                let pending_device_name = settings.device_name.clone();
                self.state.settings = settings.clone();
                self.state.settings_error = None;
                self.publish();
                if self.settings.send(settings).is_err() {
                    self.state.settings_error = Some("settings storage is unavailable".into());
                    self.publish();
                }
                if federation_changed {
                    self.federation = None;
                    self.start_federation();
                }
                if device_name_changed {
                    self.apply_local_device_name(&pending_device_name);
                    self.schedule_device_name_publish(pending_device_name);
                }
                if similarity_changed {
                    self.similarity.apply(&self.state.settings.similarity);
                }
            }
            BackendCommand::Shutdown => {}
        }
    }

    fn start_search(&mut self, request_id: RequestId, query: String) {
        self.active_detail = None;
        if let Some((_, token)) = self.active_search.take() {
            token.cancel();
        }
        let token = CancellationToken::new();
        self.active_search = Some((request_id, token.clone()));
        let local = local_search_results(&self.catalog, &query).unwrap_or_default();
        self.state.search = SearchSnapshot {
            request_id: Some(request_id),
            results: local,
            federation_pending: self.state.settings.federation_enabled,
            stats: None,
            error: None,
        };
        self.state.federation_activity = FederationActivitySnapshot {
            operation: FederationOperation::Search,
            pending: self.state.settings.federation_enabled,
            stats: None,
            error: None,
        };
        self.publish();

        let federation = self.federation.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = token.cancelled() => {}
                () = async {} => {
                    let results = if let Some(federation) = federation {
                        federation.search(&query).await.map_err(|error| error.to_string())
                    } else {
                        Ok((SearchResults::default(), SearchStats::default()))
                    };
                    let _ = internal.send(InternalEvent::SearchFinished { request_id, results }).await;
                }
            }
        });
    }

    fn cancel_search(&mut self, request_id: RequestId) {
        let matches = self
            .active_search
            .as_ref()
            .is_some_and(|(active, _)| *active == request_id);
        if matches {
            if let Some((_, token)) = self.active_search.take() {
                token.cancel();
            }
            self.state.search = SearchSnapshot::default();
            self.publish();
        }
    }

    fn load_detail(
        &mut self,
        request_id: RequestId,
        key: ArtistKey,
        name: String,
        release: Option<ReleaseKey>,
    ) {
        if !self.state.settings.federation_enabled || name.trim().is_empty() {
            return;
        }
        if let Some((_, token)) = self.active_search.take() {
            token.cancel();
            self.state.search.federation_pending = false;
        }
        self.active_detail = Some(request_id);
        self.state.federation_activity = FederationActivitySnapshot {
            operation: if release.is_some() {
                FederationOperation::Release
            } else {
                FederationOperation::Artist
            },
            pending: true,
            stats: None,
            error: None,
        };
        self.publish();
        let federation = self.federation.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let results = match federation {
                Some(client) => client
                    .artist_card(&name)
                    .await
                    .map_err(|error| error.to_string()),
                None => Err("federation is still starting".into()),
            };
            let _ = internal
                .send(InternalEvent::DetailFinished {
                    request_id,
                    key,
                    name,
                    results,
                    release,
                })
                .await;
        });
    }

    #[allow(
        clippy::too_many_lines,
        reason = "central exhaustive internal event dispatcher"
    )]
    fn handle_internal(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::SearchFinished {
                request_id,
                results,
            } => {
                if self
                    .active_search
                    .as_ref()
                    .is_some_and(|(active, token)| *active == request_id && !token.is_cancelled())
                {
                    self.active_search = None;
                    self.state.search.federation_pending = false;
                    match results {
                        Ok((remote, stats)) => {
                            merge_search_results(&mut self.state.search.results, remote);
                            self.state.federation_activity.pending = false;
                            self.state.federation_activity.stats = Some(stats.clone());
                            self.state.search.stats = Some(SearchStats {
                                tracks: self.state.search.results.tracks.len(),
                                artists: self.state.search.results.artists.len(),
                                ..stats
                            });
                        }
                        Err(message) => {
                            self.state.search.error = Some(message.clone());
                            self.state.federation_activity.pending = false;
                            self.state.federation_activity.error = Some(message);
                        }
                    }
                    self.publish();
                }
            }
            InternalEvent::DetailFinished {
                request_id,
                key,
                name,
                results,
                release,
            } => {
                if self.active_detail == Some(request_id) {
                    self.active_detail = None;
                    match results {
                        Ok((mut remote, stats)) => {
                            let local_release_baseline = release
                                .as_ref()
                                .and_then(|selected| self.release(selected))
                                .cloned();
                            let selected_name = music_dht::normalize_name(&name);
                            for artist in &mut remote.artists {
                                if music_dht::normalize_name(&artist.name) == selected_name {
                                    artist.key = key.clone();
                                }
                            }
                            for item in &mut remote.releases {
                                for artist in &mut item.artists {
                                    if music_dht::normalize_name(&artist.name) == selected_name {
                                        artist.key = key.clone();
                                    }
                                }
                                for track in &mut item.tracks {
                                    for artist in track
                                        .artists
                                        .iter_mut()
                                        .chain(track.featured_artists.iter_mut())
                                    {
                                        if music_dht::normalize_name(&artist.name) == selected_name
                                        {
                                            artist.key = key.clone();
                                        }
                                    }
                                }
                                if let Some(selected) = &release
                                    && music_dht::normalize_name(&item.title)
                                        == self.release(selected).map_or(String::new(), |r| {
                                            music_dht::normalize_name(&r.title)
                                        })
                                {
                                    item.key = selected.clone();
                                    for track in &mut item.tracks {
                                        track.release_id = selected.clone();
                                    }
                                }
                            }
                            for track in &mut remote.tracks {
                                for artist in track
                                    .artists
                                    .iter_mut()
                                    .chain(track.featured_artists.iter_mut())
                                {
                                    if music_dht::normalize_name(&artist.name) == selected_name {
                                        artist.key = key.clone();
                                    }
                                }
                            }
                            if let Some(local) = local_release_baseline {
                                if let Some(existing) = self
                                    .state
                                    .search
                                    .results
                                    .releases
                                    .iter_mut()
                                    .find(|candidate| candidate.key == local.key)
                                {
                                    let previous = std::mem::replace(existing, local);
                                    merge_release_preserving_local(existing, previous);
                                } else {
                                    self.state.search.results.releases.push(local);
                                }
                            }
                            merge_search_results(&mut self.state.search.results, remote);
                            self.state.federation_activity.pending = false;
                            self.state.federation_activity.stats = Some(stats);
                        }
                        Err(message) => {
                            self.state.search.error = Some(message.clone());
                            self.state.federation_activity.pending = false;
                            self.state.federation_activity.error = Some(message);
                        }
                    }
                    self.publish();
                }
            }
            InternalEvent::FederationStarted(result) => {
                match result {
                    Ok(client) => {
                        let service = client.service();
                        for alpn in [
                            music_dht::device_sync::SYNC_ALPN_V1,
                            music_dht::device_sync::SYNC_ALPN_V2,
                        ] {
                            match service.stream_acceptor(alpn) {
                                Ok(acceptor) => {
                                    tokio::spawn(devices::serve(
                                        acceptor,
                                        std::sync::Arc::clone(&self.devices),
                                        std::sync::Arc::clone(&service),
                                    ));
                                }
                                Err(error) => {
                                    self.state.connected_devices.error =
                                        Some(format!("device listener: {error}"));
                                }
                            }
                        }
                        tokio::spawn(devices::sync_loop(
                            std::sync::Arc::clone(&self.devices),
                            std::sync::Arc::clone(&service),
                        ));
                        self.device_service = Some(service);
                        self.publish_device_name(self.state.settings.device_name.clone());
                        if let Ok(specs) = federation_specs(&self.catalog) {
                            let publisher = client.clone();
                            tokio::spawn(async move {
                                let _ = publisher.publish(specs).await;
                            });
                        }
                        self.federation = Some(client);
                        self.resolve_history_tracks();
                        self.state.federation_activity.pending = false;
                        self.resolve_queue_artwork();
                        self.refresh_federation_debug();
                    }
                    Err(message) => {
                        self.state.settings_error = Some(message.clone());
                        self.state.federation_activity.pending = false;
                        self.state.federation_activity.error = Some(message.clone());
                        self.state.federation_debug = FederationDebugSnapshot {
                            error: Some(message),
                            ..FederationDebugSnapshot::default()
                        };
                    }
                }
                self.refresh_connected_devices();
                self.publish();
            }
            InternalEvent::SettingsPersisted(result) => {
                self.state.settings_error = result.err();
                self.publish();
            }
            InternalEvent::Audio(event) => self.handle_audio_event(event),
            InternalEvent::FederatedStreamReady {
                key,
                reader,
                mime_type,
            } => {
                if self
                    .state
                    .queue
                    .current()
                    .is_some_and(|item| item.track.key.matches(&key))
                {
                    self.audio
                        .play_stream(Box::new(reader), mime_type, self.state.playback.volume);
                }
            }
            InternalEvent::FederatedDownloadComplete {
                key,
                path,
                keep,
                metadata,
            } => {
                if keep {
                    if let Err(error) =
                        furumi_library::import::read_file(&path).and_then(|mut import| {
                            apply_federated_metadata(&mut import, metadata);
                            furumi_library::import::upsert_track(&self.catalog, &import).map(|_| ())
                        })
                    {
                        self.state.playback_error =
                            Some(format!("track played but could not be imported: {error:#}"));
                    } else if let Ok(library) = library_snapshot(&self.catalog) {
                        self.library = library.clone();
                        self.state.library = RemoteData::Ready(library);
                        self.reconcile_downloaded_tracks(&key, &path);
                        if let (Some(client), Ok(specs)) =
                            (self.federation.clone(), federation_specs(&self.catalog))
                        {
                            tokio::spawn(async move {
                                let _ = client.publish(specs).await;
                            });
                        }
                    }
                } else if let Some((_, old)) = self.ephemeral_audio.replace((key, path)) {
                    let _ = std::fs::remove_file(old);
                }
                self.publish();
            }
            InternalEvent::FederatedDownloadFailed { key, message } => {
                if self
                    .state
                    .queue
                    .current()
                    .is_some_and(|item| item.track.key.matches(&key))
                {
                    self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
                    self.state.playback.status = PlaybackStatus::Stopped;
                    self.state.playback_error = Some(message);
                    self.publish();
                }
            }
            InternalEvent::DevicesChanged => {
                self.refresh_connected_devices();
                self.publish();
            }
            InternalEvent::DeviceNamePublishDue(candidate) => {
                if self.state.settings.device_name != candidate {
                    return;
                }
                let normalized = normalize_device_name(&candidate);
                if self.state.settings.device_name != normalized {
                    self.state.settings.device_name.clone_from(&normalized);
                    if self.settings.send(self.state.settings.clone()).is_err() {
                        self.state.settings_error = Some("settings storage is unavailable".into());
                    }
                }
                self.apply_local_device_name(&normalized);
                self.publish_device_name(normalized);
                self.publish();
            }
            InternalEvent::DeviceNamePublished(result) => {
                if let Err(message) = result {
                    self.state.connected_devices.error = Some(message);
                }
                self.refresh_connected_devices();
                self.publish();
            }
            InternalEvent::FederatedContentResolved { key, result } => match result {
                Ok(track) => {
                    let is_current = self
                        .state
                        .queue
                        .current()
                        .is_some_and(|item| item.track.key.matches(&key));
                    self.state.queue.replace_track(&key, &track);
                    self.resolve_queue_artwork();
                    if is_current {
                        self.play_current();
                    } else {
                        self.publish();
                    }
                }
                Err(message) => {
                    self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
                    self.state.playback.status = PlaybackStatus::Stopped;
                    self.state.playback_error = Some(message);
                    self.publish();
                }
            },
            InternalEvent::QueueArtworkResolved {
                request_id,
                keys,
                cover_uri,
            } => {
                self.pending_queue_artwork.remove(&request_id);
                if let Some(cover_uri) = cover_uri {
                    for key in keys {
                        if let Some(mut track) = self
                            .state
                            .queue
                            .items()
                            .iter()
                            .find(|item| item.track.key.matches(&key))
                            .map(|item| item.track.clone())
                        {
                            track.cover_uri = Some(cover_uri.clone());
                            self.state.queue.replace_track(&key, &track);
                        }
                    }
                    self.resolve_queue_artwork();
                    self.publish();
                }
            }
            InternalEvent::HistoryTrackResolved { content_id, result } => {
                self.pending_history_resolutions.remove(&content_id);
                if let Ok(mut resolved) = result {
                    let mut changed = false;
                    for track in &mut self.library.recently_played {
                        if track
                            .key
                            .content_id()
                            .is_some_and(|id| id.as_str() == content_id)
                        {
                            resolved.liked = track.liked;
                            *track = resolved.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        self.state.library = RemoteData::Ready(self.library.clone());
                        self.publish();
                    }
                }
            }
            InternalEvent::FederationDebugUpdated(debug) => {
                self.federation_debug_pending = false;
                if self.state.federation_debug != debug {
                    self.state.federation_debug = debug;
                    self.publish();
                }
            }
            InternalEvent::DeviceLibraryChanged => {
                if let Ok(library) = library_snapshot(&self.catalog) {
                    self.library = library.clone();
                    self.state.library = RemoteData::Ready(library);
                    self.reconcile_likes();
                    self.similarity.start();
                    self.resolve_history_tracks();
                }
                self.refresh_connected_devices();
                self.publish();
            }
            InternalEvent::DeviceOperationFinished(result) => {
                self.state.connected_devices.busy = false;
                match result {
                    Ok(DeviceOperationResult::Invite(invite)) => {
                        self.state.connected_devices.invite = Some(invite);
                        self.state.connected_devices.error = None;
                    }
                    Ok(DeviceOperationResult::Connected(message)) => {
                        self.state.connected_devices.invite = None;
                        self.state.connected_devices.last_sync = Some(message);
                        self.state.connected_devices.error = None;
                    }
                    Err(message) => self.state.connected_devices.error = Some(message),
                }
                self.refresh_connected_devices();
                self.publish();
            }
            InternalEvent::DevicePlaybackSnapshot(snapshot) => {
                self.apply_remote_playback_snapshot(snapshot);
            }
            InternalEvent::DevicePlaybackCommand(command) => {
                self.apply_device_playback_command(command);
            }
            InternalEvent::SimilarityStatus(status) => {
                self.state.similarity_status = similarity_status_snapshot(&status);
                self.publish();
            }
            InternalEvent::SimilarityProfileActivated(active_profile) => {
                self.state.settings.similarity.active_profile = active_profile;
                let _ = self.settings.send(self.state.settings.clone());
                self.publish();
            }
            InternalEvent::SimilaritySearchFinished {
                source_title,
                result,
            } => {
                self.state.similarity_search.pending = false;
                self.state.similarity_search.source_title = source_title;
                match result {
                    Ok(mut tracks) => {
                        let liked = self
                            .catalog
                            .liked_content_ids()
                            .unwrap_or_default()
                            .into_iter()
                            .chain(self.catalog.fed_like_ids().unwrap_or_default())
                            .collect::<HashSet<_>>();
                        for track in &mut tracks {
                            track.liked = track_is_liked(track, &liked);
                        }
                        self.state.similarity_search.results = tracks;
                        self.state.similarity_search.error = None;
                    }
                    Err(error) => {
                        self.state.similarity_search.results.clear();
                        self.state.similarity_search.error = Some(error);
                    }
                }
                self.publish();
            }
        }
    }

    fn start_similarity_search(&mut self, key: &TrackKey) {
        let Some(source) = self.track(key).cloned() else {
            self.state.similarity_search.error = Some("Track is unavailable".into());
            self.publish();
            return;
        };
        if !self.state.settings.similarity.enabled {
            self.state.similarity_search.error =
                Some("Enable Similarity search in Settings first".into());
            self.state.similarity_search.results.clear();
            self.publish();
            return;
        }
        let Some(track_id) = source.key.local_id().map(LocalTrackId::get) else {
            self.state.similarity_search.error =
                Some("Similarity search currently starts from a local track".into());
            self.state.similarity_search.results.clear();
            self.publish();
            return;
        };
        self.state
            .similarity_search
            .source_title
            .clone_from(&source.title);
        self.state.similarity_search.results.clear();
        self.state.similarity_search.error = None;
        self.state.similarity_search.pending = true;
        self.publish();
        let manager = std::sync::Arc::clone(&self.similarity);
        let federation = self.federation.clone();
        let settings = self.state.settings.similarity.clone();
        let internal = self.internal.clone();
        let source_title = source.title;
        tokio::spawn(async move {
            let local = tokio::task::spawn_blocking(move || manager.search_track(track_id, 50))
                .await
                .map_err(|error| format!("similarity worker failed: {error}"))
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let result = match local {
                Ok((local, query)) => {
                    let mut scored = local
                        .into_iter()
                        .map(|found| {
                            (
                                library_track(found.track, ""),
                                found.score,
                                Some(found.embedding_signature),
                            )
                        })
                        .collect::<Vec<_>>();
                    if settings.federation_consent
                        && let Some(federation) = federation
                        && let Ok(remote) = federation
                            .search_similar(
                                query,
                                50,
                                settings.minimum_score,
                                settings.max_tracks_per_artist,
                            )
                            .await
                    {
                        scored.extend(
                            remote
                                .into_iter()
                                .map(|hit| (hit.track, hit.score, hit.embedding_signature)),
                        );
                    }
                    Ok(rank_similarity_candidates(
                        scored,
                        settings.max_tracks_per_artist,
                    ))
                }
                Err(error) => Err(error),
            };
            let _ = internal
                .send(InternalEvent::SimilaritySearchFinished {
                    source_title,
                    result,
                })
                .await;
        });
    }

    fn apply_remote_playback_snapshot(
        &mut self,
        snapshot: music_dht::device_sync::PlaybackSnapshot,
    ) {
        if !remote_snapshot_has_authority(
            self.device_role,
            &self.active_device_id,
            self.state.playback.status,
            self.state.queue.items().is_empty(),
            &snapshot,
        ) {
            return;
        }

        let controls_sender = self.device_role == DevicePlaybackRole::Control;

        if controls_sender
            && let Some(pending) = self
                .pending_control
                .as_ref()
                .filter(|pending| pending.device_id == snapshot.device_id)
        {
            let acknowledged = playback_state_acknowledges_command(
                &pending.state,
                &snapshot.state,
                pending.seek,
                pending.sent_at.elapsed(),
            );
            if !acknowledged && pending.sent_at.elapsed() < CONTROL_COMMAND_ACK_TIMEOUT {
                // This snapshot was produced before the active device applied
                // our latest command. Letting it through would briefly restore
                // the previous track or seek position in the reactive state.
                return;
            }
            self.pending_control = None;
        }

        if self.device_role == DevicePlaybackRole::Active {
            self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
        }
        self.device_role = DevicePlaybackRole::Control;
        self.active_device_id.clone_from(&snapshot.device_id);
        self.active_device_name.clone_from(&snapshot.device_name);
        self.control_anchor = Some(ControlPlaybackAnchor {
            device_id: snapshot.device_id,
            state: snapshot.state.clone(),
            observed_at: Instant::now(),
        });
        self.audio.stop();
        self.apply_device_playback_state(&snapshot.state, false, false);
        self.refresh_connected_devices();
        self.publish();
    }

    fn control_position(&self) -> Option<f64> {
        let (state, elapsed) = self
            .pending_control
            .as_ref()
            .filter(|pending| pending.device_id == self.active_device_id)
            .map(|pending| (&pending.state, pending.sent_at.elapsed()))
            .or_else(|| {
                self.control_anchor
                    .as_ref()
                    .filter(|anchor| anchor.device_id == self.active_device_id)
                    .map(|anchor| (&anchor.state, anchor.observed_at.elapsed()))
            })?;
        let position = extrapolated_control_position(state, elapsed);
        Some(if self.state.playback.duration_seconds > 0.0 {
            position.min(self.state.playback.duration_seconds)
        } else {
            position
        })
    }

    fn tick_playback(&mut self) {
        if matches!(self.state.playback.status, PlaybackStatus::Stopped) {
            return;
        }
        if self.device_role == DevicePlaybackRole::Control {
            if let Some(position) = self.control_position() {
                if (position - self.state.playback.position_seconds).abs() < 0.01 {
                    return;
                }
                self.state.playback.position_seconds = position;
                self.publish();
            }
            return;
        }
        let position = self
            .audio
            .shared
            .position_seconds()
            .clamp(0.0, self.state.playback.duration_seconds.max(0.0));
        if (position - self.state.playback.position_seconds).abs() >= 0.01 {
            self.state.playback.position_seconds = position;
            self.publish();
        }
    }

    fn handle_audio_event(&mut self, event: audio::Event) {
        match event {
            audio::Event::Started => {
                self.state.playback.status = PlaybackStatus::Playing;
                self.state.playback_error = None;
                self.publish();
            }
            audio::Event::Finished => {
                self.finish_listen(music_dht::device_sync::ListenEndReason::Finished);
                self.remove_ephemeral_audio();
                if self.advance_queue(true) {
                    self.play_current();
                } else {
                    self.state.playback.status = PlaybackStatus::Stopped;
                    self.state.playback.position_seconds =
                        self.state.playback.duration_seconds.max(0.0);
                    self.publish();
                }
            }
            audio::Event::Failed(message) => {
                self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
                self.state.playback.status = PlaybackStatus::Stopped;
                self.state.playback_error = Some(message);
                self.publish();
            }
        }
    }

    fn play_current(&mut self) {
        self.remove_ephemeral_audio();
        let Some(track) = self.state.queue.current().map(|item| item.track.clone()) else {
            self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
            self.state.playback.status = PlaybackStatus::Stopped;
            self.publish();
            return;
        };
        self.begin_listen(&track);
        self.state.playback.position_seconds = 0.0;
        self.state.playback.duration_seconds = track.duration_seconds.max(0.0);
        self.state.playback_error = None;
        if self.device_role == DevicePlaybackRole::Control {
            self.state.playback.status = PlaybackStatus::Playing;
            self.send_control_state(true);
            self.publish();
            return;
        }
        match track.audio_source.clone() {
            AudioSource::LocalFile(path) => {
                self.state.playback.status = PlaybackStatus::Playing;
                self.audio.play(path, self.state.playback.volume);
            }
            AudioSource::Federation {
                peer_id,
                content_id,
            } => {
                self.start_federated_playback(track, peer_id, content_id);
                return;
            }
        }
        self.publish();
    }

    fn begin_listen(&mut self, track: &Track) {
        if self.device_role != DevicePlaybackRole::Active {
            return;
        }
        if let Some(session) = &mut self.listen_session
            && session.track.key.matches(&track.key)
        {
            session.track = track.clone();
            return;
        }
        self.finish_listen(music_dht::device_sync::ListenEndReason::Replaced);
        if track.key.content_id().is_some() {
            self.listen_session = Some(ListenSession {
                id: devices::DeviceSync::new_listen_id(),
                track: track.clone(),
                started_at_ms: unix_time_ms(),
            });
        }
    }

    fn finish_listen(&mut self, ended_reason: music_dht::device_sync::ListenEndReason) {
        let Some(session) = self.listen_session.take() else {
            return;
        };
        let listened_ms = if ended_reason == music_dht::device_sync::ListenEndReason::Finished {
            seconds_to_milliseconds(session.track.duration_seconds)
        } else {
            seconds_to_milliseconds(self.state.playback.position_seconds)
        };
        if let Err(error) = self.devices.record_listen(
            session.id,
            &session.track,
            session.started_at_ms,
            listened_ms,
            ended_reason,
        ) {
            self.state.settings_error = Some(format!("listening history: {error:#}"));
        }
    }

    fn shuffle_new_context(&mut self) {
        if self.state.playback.shuffle {
            self.state.queue.shuffle_upcoming();
        }
    }

    fn advance_queue(&mut self, natural_finish: bool) -> bool {
        if natural_finish && self.state.playback.repeat == PlaybackRepeat::One {
            return self.state.queue.current().is_some();
        }
        if self.state.queue.advance().is_some() {
            return true;
        }
        self.state.playback.repeat == PlaybackRepeat::All
            && self.state.queue.select_index(0).is_some()
    }

    fn start_federated_playback(&mut self, track: Track, peer_id: String, content_id: ContentId) {
        let Some(client) = self.federation.clone() else {
            self.finish_listen(music_dht::device_sync::ListenEndReason::Stopped);
            self.state.playback.status = PlaybackStatus::Stopped;
            self.state.playback_error = Some("federation is still starting".into());
            self.publish();
            return;
        };
        self.audio.stop();
        self.state.playback.status = PlaybackStatus::Stopped;
        if let Some((_, item_id)) = track.key.federation_id() {
            self.spawn_federated_stream(client, &track, peer_id, item_id.to_owned());
        } else {
            let key = track.key;
            let internal = self.internal.clone();
            tokio::spawn(async move {
                let result = client
                    .track_by_content_id(content_id.as_str())
                    .await
                    .map_err(|error| format!("federated playback lookup failed: {error:#}"));
                let _ = internal
                    .send(InternalEvent::FederatedContentResolved { key, result })
                    .await;
            });
        }
        self.publish();
    }

    fn spawn_federated_stream(
        &self,
        client: std::sync::Arc<federation::Client>,
        track: &Track,
        peer_id: String,
        item_id: String,
    ) {
        let key = track.key.clone();
        let internal = self.internal.clone();
        let keep = self.state.settings.save_federated_on_listen;
        let directory = federated_audio_directory(
            &self.state.settings.library_path,
            keep,
            &self.federation_media_dir,
        );
        let stem = format!("fed-{}", sanitize_filename(&track.title));
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::channel(4);
            let forward = internal.clone();
            let event_key = key.clone();
            let relay = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    let event = match event {
                        federation::StreamEvent::Ready(reader, mime_type) => {
                            InternalEvent::FederatedStreamReady {
                                key: event_key.clone(),
                                reader,
                                mime_type,
                            }
                        }
                        federation::StreamEvent::Complete(path, metadata) => {
                            InternalEvent::FederatedDownloadComplete {
                                key: event_key.clone(),
                                path,
                                keep,
                                metadata: *metadata,
                            }
                        }
                        federation::StreamEvent::Failed(message) => {
                            InternalEvent::FederatedDownloadFailed {
                                key: event_key.clone(),
                                message,
                            }
                        }
                    };
                    let _ = forward.send(event).await;
                }
            });
            if let Err(error) = client
                .stream_track(&peer_id, &item_id, &directory, &stem, tx.clone())
                .await
            {
                let _ = tx
                    .send(federation::StreamEvent::Failed(format!(
                        "federated playback failed: {error:#}"
                    )))
                    .await;
            }
            drop(tx);
            let _ = relay.await;
        });
    }

    fn remove_ephemeral_audio(&mut self) {
        if let Some((_, path)) = self.ephemeral_audio.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn reconcile_downloaded_tracks(
        &mut self,
        downloaded_key: &TrackKey,
        downloaded_path: &std::path::Path,
    ) {
        let local_tracks = self
            .library
            .featured_releases
            .iter()
            .flat_map(|release| release.tracks.iter())
            .cloned()
            .collect::<Vec<_>>();
        let downloaded_local = local_tracks.iter().find(|track| {
            matches!(&track.audio_source, AudioSource::LocalFile(path) if path == downloaded_path)
        }).cloned();
        for local in &local_tracks {
            for track in &mut self.state.search.results.tracks {
                if track.key.matches(&local.key) {
                    *track = local.clone();
                }
            }
            for track in self
                .state
                .search
                .results
                .releases
                .iter_mut()
                .flat_map(|release| release.tracks.iter_mut())
            {
                if track.key.matches(&local.key) {
                    *track = local.clone();
                }
            }
            self.state.queue.replace_matching_track(local);
        }
        if let Some(local) = downloaded_local {
            for track in &mut self.state.search.results.tracks {
                if track.key.matches(downloaded_key) {
                    *track = local.clone();
                }
            }
            for track in self
                .state
                .search
                .results
                .releases
                .iter_mut()
                .flat_map(|release| release.tracks.iter_mut())
            {
                if track.key.matches(downloaded_key) {
                    *track = local.clone();
                }
            }
            self.state.queue.replace_track(downloaded_key, &local);
        }
    }

    fn release(&self, id: &ReleaseKey) -> Option<&Release> {
        self.library
            .featured_releases
            .iter()
            .chain(self.state.search.results.releases.iter())
            .find(|release| &release.key == id)
    }

    fn track(&self, key: &TrackKey) -> Option<&Track> {
        self.state
            .queue
            .items()
            .iter()
            .map(|item| &item.track)
            .find(|track| track.key.matches(key))
            .or_else(|| find_catalog_track(&self.library, &self.state.search.results, key))
            .or_else(|| {
                self.state
                    .similarity_search
                    .results
                    .iter()
                    .find(|track| track.key.matches(key))
            })
    }

    fn resolve_tracks(&self, keys: &[TrackKey]) -> Vec<Track> {
        keys.iter()
            .filter_map(|key| self.track(key).cloned())
            .collect()
    }

    fn refresh_library(&mut self) {
        match library_snapshot(&self.catalog) {
            Ok(library) => {
                self.library = library.clone();
                self.state.library = RemoteData::Ready(library);
                self.reconcile_likes();
                self.resolve_history_tracks();
            }
            Err(error) => {
                self.state.settings_error = Some(format!("library: {error:#}"));
            }
        }
        self.publish();
    }

    fn resolve_history_tracks(&mut self) {
        let Some(client) = self.federation.clone() else {
            return;
        };
        let unresolved = self
            .library
            .recently_played
            .iter()
            .filter(|track| track.key.local_id().is_none() && track.cover_uri.is_none())
            .filter_map(|track| track.key.content_id().map(|id| id.as_str().to_owned()))
            .collect::<HashSet<_>>();
        for content_id in unresolved {
            if !self.pending_history_resolutions.insert(content_id.clone()) {
                continue;
            }
            let internal = self.internal.clone();
            let client = client.clone();
            tokio::spawn(async move {
                let result = client
                    .track_by_content_id(&content_id)
                    .await
                    .map_err(|error| format!("history track lookup failed: {error:#}"));
                let _ = internal
                    .send(InternalEvent::HistoryTrackResolved { content_id, result })
                    .await;
            });
        }
    }

    fn reconcile_likes(&mut self) {
        let liked = self
            .catalog
            .liked_content_ids()
            .unwrap_or_default()
            .into_iter()
            .chain(self.catalog.fed_like_ids().unwrap_or_default())
            .collect::<HashSet<_>>();
        for track in self.state.search.results.tracks.iter_mut().chain(
            self.state
                .search
                .results
                .releases
                .iter_mut()
                .flat_map(|release| release.tracks.iter_mut()),
        ) {
            track.liked = track_is_liked(track, &liked);
        }
        for track in &mut self.state.similarity_search.results {
            track.liked = track_is_liked(track, &liked);
        }
        let replacements = self
            .library
            .featured_releases
            .iter()
            .flat_map(|release| release.tracks.iter())
            .chain(self.library.recently_played.iter())
            .chain(
                self.library
                    .playlists
                    .iter()
                    .flat_map(|playlist| playlist.tracks.iter()),
            )
            .cloned()
            .collect::<Vec<_>>();
        for track in replacements {
            self.state.queue.replace_matching_track(&track);
        }
    }

    fn toggle_like(&mut self, key: &TrackKey) {
        let Some(track) = self.track(key).cloned() else {
            return;
        };
        let Some(content_id) = track.key.content_id().map(|id| id.as_str().to_owned()) else {
            self.state.settings_error = Some("This track has no stable content ID".into());
            self.publish();
            return;
        };
        let result = if self
            .catalog
            .track_id_by_content_id(&content_id)
            .ok()
            .flatten()
            .is_some()
        {
            self.catalog
                .toggle_like_by_content_id(&content_id)
                .map(|liked| (liked, None))
        } else if let Some(fed) = track_to_library_fed(&track) {
            self.catalog
                .toggle_fed_like(&fed)
                .map(|liked| (liked, track_to_synced_fed(&track)))
        } else {
            Err(anyhow::anyhow!(
                "track cannot be stored as a federated like"
            ))
        };
        match result {
            Ok((liked, fed)) => {
                if let Err(error) = self.devices.record_like(&content_id, liked, fed) {
                    self.state.settings_error = Some(format!("like sync: {error:#}"));
                }
                self.refresh_library();
            }
            Err(error) => {
                self.state.settings_error = Some(format!("like: {error:#}"));
                self.publish();
            }
        }
    }

    fn add_to_playlist(&mut self, playlist_id: i64, keys: &[TrackKey]) {
        let tracks = self.resolve_tracks(keys);
        let local_ids = tracks
            .iter()
            .filter_map(|track| track.key.local_id().map(LocalTrackId::get))
            .collect::<Vec<_>>();
        let remote = tracks
            .iter()
            .filter_map(track_to_library_fed)
            .collect::<Vec<_>>();
        let result = (|| -> anyhow::Result<()> {
            self.catalog
                .add_tracks_to_playlist(playlist_id, &local_ids)?;
            self.catalog
                .add_fed_tracks_to_playlist(playlist_id, &remote)?;
            self.devices
                .record_playlist_tracks_added(playlist_id, &local_ids)?;
            let mut synced = Vec::new();
            for track in &tracks {
                let Some(content_id) = track.key.content_id().map(|id| id.as_str().to_owned())
                else {
                    continue;
                };
                let Some(fed) = track_to_synced_fed(track) else {
                    continue;
                };
                let position = self
                    .catalog
                    .playlist_content_position(playlist_id, &content_id)?
                    .unwrap_or(0);
                synced.push((content_id, position, fed));
            }
            self.devices
                .record_playlist_fed_added(playlist_id, &synced)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.state.settings_error = Some(format!("add to playlist: {error:#}"));
        }
        self.refresh_library();
    }

    fn remove_from_playlist(&mut self, playlist_id: i64, keys: &[TrackKey]) {
        let tracks = self.resolve_tracks(keys);
        let local_ids = tracks
            .iter()
            .filter_map(|track| track.key.local_id().map(LocalTrackId::get))
            .collect::<Vec<_>>();
        let content_ids = tracks
            .iter()
            .filter_map(|track| track.key.content_id().map(|id| id.as_str().to_owned()))
            .collect::<Vec<_>>();
        let result = (|| -> anyhow::Result<()> {
            self.catalog
                .remove_tracks_from_playlist(playlist_id, &local_ids)?;
            self.catalog
                .remove_content_ids_from_playlist(playlist_id, &content_ids)?;
            self.devices
                .record_playlist_removed(playlist_id, &content_ids)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.state.settings_error = Some(format!("remove from playlist: {error:#}"));
        }
        self.refresh_library();
    }

    fn publish(&mut self) {
        self.publish_device_playback();
        self.state.revision = self.state.revision.saturating_add(1);
        self.snapshots.send_replace(self.state.clone());
    }
}

fn rank_similarity_candidates(
    mut candidates: Vec<SimilarityCandidate>,
    max_tracks_per_artist: usize,
) -> Vec<Track> {
    const MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE: u32 = 8;

    candidates.sort_by(|left, right| right.1.total_cmp(&left.1));
    let mut content = HashSet::new();
    let mut signatures = Vec::new();
    let mut artist_counts = HashMap::<String, usize>::new();
    let mut tracks = Vec::new();
    for (track, _, signature) in candidates {
        let identity = track
            .key
            .content_id()
            .map_or_else(|| format!("{:?}", track.key), |id| id.as_str().to_owned());
        if !content.insert(identity) {
            continue;
        }
        if signature.is_some_and(|candidate| {
            signatures.iter().any(|existing| {
                music_dht::similarity::signature_distance(&candidate, existing)
                    <= MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE
            })
        }) {
            continue;
        }
        let artist = track.artists.first().map_or_else(
            || music_dht::normalize_name(&track.artist),
            |artist| music_dht::normalize_name(&artist.name),
        );
        let count = artist_counts.entry(artist.clone()).or_default();
        if !artist.is_empty() && *count >= max_tracks_per_artist.clamp(1, SIMILARITY_RESULT_LIMIT) {
            continue;
        }
        *count += 1;
        if let Some(signature) = signature {
            signatures.push(signature);
        }
        tracks.push(track);
        if tracks.len() >= SIMILARITY_RESULT_LIMIT {
            break;
        }
    }
    tracks
}

#[cfg(test)]
mod tests;
