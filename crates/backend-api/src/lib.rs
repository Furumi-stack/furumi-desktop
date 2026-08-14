//! Transport-independent contract between the frontend application and backend.

use std::fmt;

use furumi_domain::{Artist, Queue, QueueItemId, Release, ReleaseKey, Track, TrackKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaybackRepeat {
    #[default]
    Off,
    One,
    All,
}

impl PlaybackRepeat {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackSnapshot {
    pub status: PlaybackStatus,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: PlaybackRepeat,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            volume: 0.72,
            shuffle: false,
            repeat: PlaybackRepeat::Off,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum RemoteData<T> {
    #[default]
    NotRequested,
    Loading {
        request_id: RequestId,
    },
    Ready(T),
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibrarySnapshot {
    pub artists: Vec<Artist>,
    pub featured_releases: Vec<Release>,
    pub recently_played: Vec<Track>,
    pub playlists: Vec<PlaylistSnapshot>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaylistSnapshot {
    pub id: i64,
    pub title: String,
    pub is_likes: bool,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DevicePlaybackRole {
    #[default]
    Active,
    Control,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectedDeviceSnapshot {
    pub id: String,
    pub name: String,
    pub client_version: String,
    pub is_self: bool,
    pub presence: DevicePresence,
    pub trust: DeviceTrust,
    pub is_active: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DevicePresence {
    #[default]
    Offline,
    Online,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeviceTrust {
    #[default]
    Trusted,
    Revoked,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingPairingSnapshot {
    pub request_id: String,
    pub device_id: String,
    pub name: String,
    pub client_version: String,
    pub requester_group_id: Option<String>,
    pub requester_group_active_devices: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectedDevicesSnapshot {
    pub this_device_id: String,
    pub this_device_name: String,
    pub group_id: String,
    pub role: DevicePlaybackRole,
    pub active_device_id: String,
    pub active_device_name: String,
    pub devices: Vec<ConnectedDeviceSnapshot>,
    pub pending_pairings: Vec<PendingPairingSnapshot>,
    pub invite: Option<String>,
    pub busy: bool,
    pub last_sync: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub releases: Vec<Release>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchStats {
    pub tracks: usize,
    pub artists: usize,
    pub peers_queried: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchSnapshot {
    pub request_id: Option<RequestId>,
    pub results: SearchResults,
    pub federation_pending: bool,
    pub stats: Option<SearchStats>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FederationOperation {
    #[default]
    Idle,
    Search,
    Artist,
    Release,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FederationActivitySnapshot {
    pub operation: FederationOperation,
    pub pending: bool,
    pub stats: Option<SearchStats>,
    pub error: Option<String>,
}

/// Lightweight live diagnostics for the federation node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FederationDebugSnapshot {
    pub running: bool,
    pub endpoint_id: String,
    pub dht_node_id: String,
    pub connected_peers: usize,
    pub known_contacts: usize,
    pub stored_dht_records: Option<usize>,
    pub published_items: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionEntrySnapshot {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildInfoSnapshot {
    pub software: Vec<VersionEntrySnapshot>,
    pub protocols: Vec<VersionEntrySnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsSnapshot {
    pub network_id: String,
    pub device_name: String,
    pub library_path: String,
    pub federation_enabled: bool,
    pub save_federated_on_listen: bool,
    pub language: String,
    pub similarity: SimilaritySettingsSnapshot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimilaritySettingsSnapshot {
    pub enabled: bool,
    pub model: String,
    pub profile: String,
    pub workers: usize,
    pub minimum_score: f32,
    pub max_tracks_per_artist: usize,
    pub federation_consent: bool,
    pub active_profile: Option<String>,
}

impl Default for SimilaritySettingsSnapshot {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "discogs-effnet-bsdynamic-1".into(),
            profile: "furumi-full-track-v1".into(),
            workers: std::thread::available_parallelism()
                .map_or(1, |count| (count.get() / 2).clamp(1, 4)),
            minimum_score: 0.70,
            max_tracks_per_artist: 5,
            federation_consent: false,
            active_profile: None,
        }
    }
}

impl SimilaritySettingsSnapshot {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if self.model.trim().is_empty() {
            self.model = "discogs-effnet-bsdynamic-1".into();
        }
        if self.profile.trim().is_empty() {
            self.profile = "furumi-full-track-v1".into();
        }
        self.workers = self.workers.clamp(1, 16);
        if !self.minimum_score.is_finite() {
            self.minimum_score = 0.70;
        }
        self.minimum_score = self.minimum_score.clamp(0.0, 1.0);
        self.max_tracks_per_artist = self.max_tracks_per_artist.clamp(1, 50);
        self
    }
}

impl Default for SettingsSnapshot {
    fn default() -> Self {
        Self {
            network_id: "furumi".into(),
            device_name: String::new(),
            // The backend resolves this from the platform's shared Furumi data
            // directory before publishing its first authoritative snapshot.
            library_path: String::new(),
            federation_enabled: true,
            save_federated_on_listen: true,
            language: "English".into(),
            similarity: SimilaritySettingsSnapshot::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimilarityStatusSnapshot {
    pub phase: String,
    pub active_profile: Option<String>,
    pub target_profile: Option<String>,
    pub model: String,
    pub total_tracks: usize,
    pub completed_tracks: usize,
    pub failed_tracks: usize,
    pub stored_vectors: usize,
    pub stored_bytes: u64,
    pub current_track: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimilaritySearchSnapshot {
    pub source_title: String,
    pub results: Vec<Track>,
    pub pending: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BackendSnapshot {
    pub revision: u64,
    pub library: RemoteData<LibrarySnapshot>,
    pub playback: PlaybackSnapshot,
    pub queue: Queue,
    pub search: SearchSnapshot,
    pub federation_activity: FederationActivitySnapshot,
    pub federation_debug: FederationDebugSnapshot,
    pub build_info: BuildInfoSnapshot,
    pub connected_devices: ConnectedDevicesSnapshot,
    pub settings: SettingsSnapshot,
    pub similarity_status: SimilarityStatusSnapshot,
    pub similarity_search: SimilaritySearchSnapshot,
    pub playback_error: Option<String>,
    pub settings_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackendCommand {
    Initialize,
    TogglePlayback,
    Play,
    Pause,
    Stop,
    Seek {
        position_seconds: f64,
    },
    SetVolume {
        volume: f32,
    },
    ToggleShuffle,
    CycleRepeat,
    PlayRelease {
        release_id: ReleaseKey,
        start: usize,
    },
    PlayTrack {
        track: TrackKey,
    },
    PlayQueueItem {
        item_id: QueueItemId,
    },
    SearchSimilar {
        track: TrackKey,
    },
    ClearSimilarity,
    MoveQueueItem {
        item_id: QueueItemId,
        target_index: usize,
    },
    RemoveQueueItem {
        item_id: QueueItemId,
    },
    PlayContext {
        tracks: Vec<TrackKey>,
        selected: TrackKey,
    },
    AddNext {
        tracks: Vec<TrackKey>,
    },
    AddToEnd {
        tracks: Vec<TrackKey>,
    },
    ToggleLike {
        track: TrackKey,
    },
    CreatePlaylist {
        title: String,
    },
    RenamePlaylist {
        playlist_id: i64,
        title: String,
    },
    DeletePlaylist {
        playlist_id: i64,
    },
    AddToPlaylist {
        playlist_id: i64,
        tracks: Vec<TrackKey>,
    },
    RemoveFromPlaylist {
        playlist_id: i64,
        tracks: Vec<TrackKey>,
    },
    CreateDeviceInvite,
    ConnectDevice {
        invite: String,
    },
    AnswerDevicePairing {
        request_id: String,
        accept: bool,
        use_requester_group: bool,
    },
    SelectPlaybackDevice {
        device_id: String,
    },
    Next,
    Previous,
    Search {
        request_id: RequestId,
        query: String,
    },
    CancelSearch {
        request_id: RequestId,
    },
    LoadArtist {
        request_id: RequestId,
        key: furumi_domain::ArtistKey,
        name: String,
    },
    LoadRelease {
        request_id: RequestId,
        key: ReleaseKey,
        artist_key: furumi_domain::ArtistKey,
        artist_name: String,
    },
    UpdateSettings(SettingsSnapshot),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendCommandError {
    Busy,
    Closed,
}

impl fmt::Display for SendCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("backend command queue is full"),
            Self::Closed => formatter.write_str("backend is unavailable"),
        }
    }
}

impl std::error::Error for SendCommandError {}
