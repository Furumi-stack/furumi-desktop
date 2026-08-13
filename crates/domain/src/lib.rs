//! Pure Furumi domain types and rules.

use std::fmt;
use std::path::PathBuf;

macro_rules! local_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(i64);

        impl $name {
            #[must_use]
            pub const fn new(value: i64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> i64 {
                self.0
            }
        }
    };
}

local_id!(ArtistId);
local_id!(ReleaseId);
local_id!(LocalTrackId);

/// Origin of catalog metadata. UI and application code render the same
/// entities regardless of which provider supplied them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogSource {
    Local,
    Federation { peer_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtistKey {
    Local(ArtistId),
    Federation { peer_id: String, id: String },
}

impl ArtistKey {
    #[must_use]
    pub const fn local(id: ArtistId) -> Self {
        Self::Local(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReleaseKey {
    Local(ReleaseId),
    Federation { peer_id: String, id: String },
}

impl ReleaseKey {
    #[must_use]
    pub const fn local(id: ReleaseId) -> Self {
        Self::Local(id)
    }
}

/// Artwork resolved by a catalog provider. Federation can initially publish
/// `None`, fetch the image into its cache, and then replace the snapshot with
/// the same entity and a local URI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Artwork {
    pub uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistRef {
    pub key: ArtistKey,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub key: ArtistKey,
    pub source: CatalogSource,
    pub name: String,
    pub artwork: Artwork,
    pub release_count: usize,
    pub track_count: usize,
}

/// Stable Frid/Furumi audio identity (`b3:<64 lowercase hex>`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId(String);

impl ContentId {
    /// Parses and normalizes an audio content identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidContentId`] unless the value is `b3:` followed by 64
    /// hexadecimal characters.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidContentId> {
        let value = value.into().to_ascii_lowercase();
        let hash = value.strip_prefix("b3:").ok_or(InvalidContentId)?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(InvalidContentId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidContentId;

impl fmt::Display for InvalidContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("content ID must be b3:<64 hex characters>")
    }
}

impl std::error::Error for InvalidContentId {}

/// A track can be known by its local database ID, stable content ID, or both.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackKey {
    local_id: Option<LocalTrackId>,
    content_id: Option<ContentId>,
    federation_identity: Option<(String, String)>,
}

impl TrackKey {
    #[must_use]
    pub const fn local(id: LocalTrackId) -> Self {
        Self {
            local_id: Some(id),
            content_id: None,
            federation_identity: None,
        }
    }

    #[must_use]
    pub const fn new(local_id: LocalTrackId, content_id: ContentId) -> Self {
        Self {
            local_id: Some(local_id),
            content_id: Some(content_id),
            federation_identity: None,
        }
    }

    #[must_use]
    pub const fn remote(content_id: ContentId) -> Self {
        Self {
            local_id: None,
            content_id: Some(content_id),
            federation_identity: None,
        }
    }

    #[must_use]
    pub fn federation(peer_id: String, item_id: String, content_id: Option<ContentId>) -> Self {
        Self {
            local_id: None,
            content_id,
            federation_identity: Some((peer_id, item_id)),
        }
    }

    #[must_use]
    pub const fn local_id(&self) -> Option<LocalTrackId> {
        self.local_id
    }

    #[must_use]
    pub const fn content_id(&self) -> Option<&ContentId> {
        self.content_id.as_ref()
    }

    #[must_use]
    pub fn federation_id(&self) -> Option<(&str, &str)> {
        self.federation_identity
            .as_ref()
            .map(|(peer, item)| (peer.as_str(), item.as_str()))
    }

    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.content_id
            .as_ref()
            .zip(other.content_id.as_ref())
            .is_some_and(|(left, right)| left == right)
            || self
                .local_id
                .zip(other.local_id)
                .is_some_and(|(left, right)| left == right)
            || self
                .federation_identity
                .as_ref()
                .zip(other.federation_identity.as_ref())
                .is_some_and(|(left, right)| left == right)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub key: TrackKey,
    pub title: String,
    pub artist: String,
    pub artists: Vec<ArtistRef>,
    pub featured_artists: Vec<ArtistRef>,
    pub release: String,
    pub release_id: ReleaseKey,
    pub duration_seconds: f64,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub cover_uri: Option<String>,
    pub audio_format: Option<String>,
    pub audio_bitrate_kbps: Option<u32>,
    pub audio_sample_rate_hz: Option<u32>,
    pub audio_bit_depth: Option<u32>,
    pub file_size_bytes: Option<u64>,
    /// Whether this content is present in the listener's shared likes.
    pub liked: bool,
    pub audio_source: AudioSource,
}

impl Track {
    /// Returns whether two provider records describe the same logical track.
    /// Stable content identity wins, with album metadata as a fallback for
    /// catalogs that did not publish a content ID.
    #[must_use]
    pub fn same_catalog_track(&self, other: &Self) -> bool {
        if self.key.matches(&other.key) {
            return true;
        }
        if normalized_catalog_text(&self.title) != normalized_catalog_text(&other.title) {
            return false;
        }
        let left_release = normalized_catalog_text(&self.release);
        let right_release = normalized_catalog_text(&other.release);
        if !left_release.is_empty() && !right_release.is_empty() && left_release != right_release {
            return false;
        }
        if let (Some(left), Some(right)) = (self.track_number, other.track_number) {
            return left == right
                && self.disc_number.unwrap_or(1) == other.disc_number.unwrap_or(1);
        }
        self.duration_seconds > 0.0
            && other.duration_seconds > 0.0
            && (self.duration_seconds - other.duration_seconds).abs() < 2.0
    }
}

fn normalized_catalog_text(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Playback location resolved by a catalog provider. Local files go directly
/// to the audio engine; federated sources are streamed or materialized first
/// and then handed to the same engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSource {
    LocalFile(PathBuf),
    Federation {
        peer_id: String,
        content_id: ContentId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub key: ReleaseKey,
    pub source: CatalogSource,
    pub title: String,
    pub artists: Vec<ArtistRef>,
    pub featured_artists: Vec<ArtistRef>,
    pub release_type: String,
    pub year: Option<i32>,
    pub artwork: Artwork,
    pub tracks: Vec<Track>,
}

impl Release {
    #[must_use]
    pub fn artist_line(&self) -> String {
        self.artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[must_use]
    pub fn is_album(&self) -> bool {
        self.release_type.eq_ignore_ascii_case("album")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueItemId(u64);

impl QueueItemId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueItem {
    pub id: QueueItemId,
    pub track: Track,
}

/// Logical queue shared with the other Furumi players.
///
/// `play_next_end` is the exclusive end of the stable FIFO block created by
/// successive "play next" commands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Queue {
    items: Vec<QueueItem>,
    current: Option<usize>,
    play_next_end: Option<usize>,
    original_order: Option<Vec<TrackKey>>,
    next_item_id: u64,
}

impl Queue {
    #[must_use]
    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    #[must_use]
    pub const fn current_index(&self) -> Option<usize> {
        self.current
    }

    #[must_use]
    pub fn current(&self) -> Option<&QueueItem> {
        self.current.and_then(|index| self.items.get(index))
    }

    /// Selects a concrete queue occurrence without rebuilding the queue.
    pub fn select_item(&mut self, id: QueueItemId) -> Option<&QueueItem> {
        self.current = self.items.iter().position(|item| item.id == id);
        self.play_next_end = None;
        self.current()
    }

    pub fn replace_context(&mut self, tracks: Vec<Track>, start: usize) {
        let mut items = Vec::with_capacity(tracks.len());
        for track in tracks {
            items.push(self.new_item(track));
        }
        self.items = items;
        self.current = (!self.items.is_empty()).then(|| start.min(self.items.len() - 1));
        self.play_next_end = None;
        self.original_order = None;
    }

    pub fn add_to_end(&mut self, tracks: impl IntoIterator<Item = Track>) {
        let items: Vec<_> = tracks
            .into_iter()
            .map(|track| self.new_item(track))
            .collect();
        self.items.extend(items);
    }

    pub fn add_next(&mut self, tracks: impl IntoIterator<Item = Track>) {
        let items: Vec<_> = tracks
            .into_iter()
            .map(|track| self.new_item(track))
            .collect();
        if items.is_empty() {
            return;
        }
        let insertion = self.current.map_or(0, |current| {
            self.play_next_end
                .filter(|end| *end > current && *end <= self.items.len())
                .unwrap_or((current + 1).min(self.items.len()))
        });
        let count = items.len();
        self.items.splice(insertion..insertion, items);
        if self.current.is_some_and(|current| insertion <= current) {
            self.current = self.current.map(|current| current + count);
        }
        self.play_next_end = Some(insertion + count);
    }

    pub fn replace_matching_track(&mut self, replacement: &Track) {
        for item in &mut self.items {
            if item.track.key.matches(&replacement.key) {
                item.track = replacement.clone();
            }
        }
    }

    pub fn replace_track(&mut self, key: &TrackKey, replacement: &Track) {
        for item in &mut self.items {
            if item.track.key.matches(key) {
                item.track = replacement.clone();
            }
        }
    }

    pub fn advance(&mut self) -> Option<&QueueItem> {
        let next = match self.current {
            None if !self.items.is_empty() => 0,
            Some(current) if current + 1 < self.items.len() => current + 1,
            _ => return None,
        };
        self.current = Some(next);
        self.normalize_play_next_block();
        self.current()
    }

    pub fn previous(&mut self) -> Option<&QueueItem> {
        let previous = self.current?.saturating_sub(1);
        self.current = Some(previous);
        self.normalize_play_next_block();
        self.current()
    }

    /// Selects a position directly, preserving the existing queue context.
    pub fn select_index(&mut self, index: usize) -> Option<&QueueItem> {
        if index >= self.items.len() {
            return None;
        }
        self.current = Some(index);
        self.play_next_end = None;
        self.current()
    }

    /// Randomizes only the part of the queue that has not played yet.
    ///
    /// The original order is retained so disabling shuffle can restore it.
    pub fn shuffle_upcoming(&mut self) {
        let start = self
            .current
            .map_or(0, |current| (current + 1).min(self.items.len()));
        if self.items.len().saturating_sub(start) < 2 {
            return;
        }
        if self.original_order.is_none() {
            self.remember_order();
        }
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |duration| {
                duration.as_secs() ^ u64::from(duration.subsec_nanos())
            })
            | 1;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let tail = &mut self.items[start..];
        for index in (1..tail.len()).rev() {
            let bound = u64::try_from(index + 1).unwrap_or(u64::MAX);
            let shuffled = usize::try_from(random() % bound).unwrap_or(0);
            tail.swap(index, shuffled);
        }
        self.play_next_end = None;
    }

    /// Remembers the current catalog order before another device sends a
    /// physically shuffled queue through the connected-devices protocol.
    pub fn remember_order(&mut self) {
        if self.original_order.is_none() {
            self.original_order = Some(
                self.items
                    .iter()
                    .map(|item| item.track.key.clone())
                    .collect(),
            );
        }
    }

    /// Replaces a synchronized queue without losing its pre-shuffle order.
    pub fn replace_shuffled_context(&mut self, tracks: Vec<Track>, start: usize) {
        let original_order = self.original_order.take();
        self.replace_context(tracks, start);
        self.original_order = original_order;
    }

    /// Restores the unplayed queue tail to its order before shuffle.
    pub fn restore_upcoming_order(&mut self) {
        let Some(order) = self.original_order.take() else {
            return;
        };
        let start = self
            .current
            .map_or(0, |current| (current + 1).min(self.items.len()));
        self.items[start..].sort_by_key(|item| {
            order
                .iter()
                .position(|original| original.matches(&item.track.key))
                .unwrap_or(usize::MAX)
        });
        self.play_next_end = None;
    }

    fn new_item(&mut self, track: Track) -> QueueItem {
        self.next_item_id = self.next_item_id.saturating_add(1);
        QueueItem {
            id: QueueItemId::new(self.next_item_id),
            track,
        }
    }

    fn normalize_play_next_block(&mut self) {
        if self.play_next_end.is_some_and(|end| {
            self.current.is_none_or(|current| end <= current) || end > self.items.len()
        }) {
            self.play_next_end = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64) -> Track {
        Track {
            key: TrackKey::local(LocalTrackId::new(id)),
            title: id.to_string(),
            artist: "artist".into(),
            artists: vec![ArtistRef {
                key: ArtistKey::local(ArtistId::new(1)),
                name: "artist".into(),
            }],
            featured_artists: Vec::new(),
            release: "release".into(),
            release_id: ReleaseKey::local(ReleaseId::new(1)),
            duration_seconds: 180.0,
            track_number: Some(u32::try_from(id).unwrap()),
            disc_number: Some(1),
            cover_uri: None,
            audio_format: Some("FLAC".into()),
            audio_bitrate_kbps: Some(900),
            audio_sample_rate_hz: Some(44_100),
            audio_bit_depth: Some(16),
            file_size_bytes: None,
            liked: false,
            audio_source: AudioSource::LocalFile(PathBuf::from(format!("track-{id}.flac"))),
        }
    }

    #[test]
    fn sequential_play_next_additions_form_a_fifo_block() {
        let mut queue = Queue::default();
        queue.replace_context((10..=13).map(track).collect(), 0);
        queue.add_next([track(1)]);
        queue.add_next([track(2)]);
        queue.add_next([track(3)]);

        let ids: Vec<_> = queue
            .items()
            .iter()
            .map(|item| item.track.key.local_id().unwrap().get())
            .collect();
        assert_eq!(ids, vec![10, 1, 2, 3, 11, 12, 13]);
    }

    #[test]
    fn replacing_context_keeps_order_and_starts_at_selected_track() {
        let mut queue = Queue::default();
        queue.replace_context((10..=13).map(track).collect(), 2);

        let ids: Vec<_> = queue
            .items()
            .iter()
            .map(|item| item.track.key.local_id().unwrap().get())
            .collect();
        assert_eq!(ids, vec![10, 11, 12, 13]);
        assert_eq!(queue.current_index(), Some(2));
        assert_eq!(
            queue.current().unwrap().track.key.local_id().unwrap().get(),
            12
        );
    }

    #[test]
    fn shuffle_preserves_current_track_and_can_restore_the_tail() {
        let mut queue = Queue::default();
        queue.replace_context((1..=8).map(track).collect(), 2);
        let current = queue.current().unwrap().id;

        queue.shuffle_upcoming();

        assert_eq!(queue.current().unwrap().id, current);
        let mut shuffled_tail: Vec<_> = queue.items()[3..]
            .iter()
            .map(|item| item.track.key.local_id().unwrap().get())
            .collect();
        shuffled_tail.sort_unstable();
        assert_eq!(shuffled_tail, vec![4, 5, 6, 7, 8]);

        queue.restore_upcoming_order();
        let restored: Vec<_> = queue
            .items()
            .iter()
            .map(|item| item.track.key.local_id().unwrap().get())
            .collect();
        assert_eq!(restored, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn catalog_track_matching_falls_back_to_album_position() {
        let local = track(1);
        let content_id = ContentId::parse(format!("b3:{}", "a".repeat(64))).unwrap();
        let mut federated = local.clone();
        federated.key = TrackKey::federation(
            "peer".into(),
            "remote-item".into(),
            Some(content_id.clone()),
        );
        federated.audio_source = AudioSource::Federation {
            peer_id: "peer".into(),
            content_id,
        };

        assert!(local.same_catalog_track(&federated));

        federated.track_number = Some(2);
        assert!(!local.same_catalog_track(&federated));
    }

    #[test]
    fn content_id_matches_furumi_wire_format() {
        let id = ContentId::parse(format!("B3:{}", "A".repeat(64))).unwrap();
        assert_eq!(id.as_str(), format!("b3:{}", "a".repeat(64)));
        assert!(ContentId::parse("track-1").is_err());
    }

    #[test]
    fn catalog_keys_keep_local_and_federated_entities_distinct() {
        let local = ReleaseKey::local(ReleaseId::new(7));
        let remote = ReleaseKey::Federation {
            peer_id: "peer-a".into(),
            id: "7".into(),
        };

        assert_ne!(local, remote);

        let content = ContentId::parse(format!("b3:{}", "a".repeat(64))).unwrap();
        let first = TrackKey::federation("peer-a".into(), "item-1".into(), Some(content.clone()));
        let same_audio_elsewhere =
            TrackKey::federation("peer-b".into(), "item-9".into(), Some(content));
        let other = TrackKey::federation("peer-a".into(), "item-2".into(), None);
        assert!(first.matches(&same_audio_elsewhere));
        assert!(!first.matches(&other));
    }
}
