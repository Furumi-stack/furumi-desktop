//! Federation lifecycle, DHT search and catalog artwork fetching.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use furumi_backend_api::{FederationDebugSnapshot, SearchResults, SearchStats};
use furumi_domain::{
    Artist, ArtistKey, ArtistRef, Artwork, AudioSource, CatalogSource, ContentId, Release,
    ReleaseKey, Track, TrackKey,
};
use music_dht::catalog::{
    CATALOG_ALPN, CatalogArtist, CatalogImageHeader, CatalogRequest, CatalogResponse,
};
use music_dht::similarity_dht::SimilarityDht;
use music_dht::similarity_lsh::SIMILARITY_DHT_ALPN;
use music_dht::{
    EndpointId, ItemKind, ItemSpec, LibraryItem, MusicDhtConfig, MusicDhtService, NetworkId,
    RendezvousConfig,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _};

pub const AUDIO_ALPN: &[u8] = b"furumi-fd/audio/1";
pub const AUDIO_PROTOCOL_VERSION: u16 = 1;
const STREAM_BUFFER: u64 = 2 * 1024 * 1024;

#[derive(Serialize)]
struct AudioRequest {
    item_id: String,
    offset: u64,
    want_cover: bool,
    metadata_only: bool,
}
#[derive(Deserialize)]
struct AudioHeader {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    total_size: u64,
    #[serde(default)]
    cover_size: u64,
    #[serde(default)]
    artist_image_size: u64,
    #[serde(default)]
    metadata: Option<TrackMetadata>,
}
#[derive(Clone, Default, Deserialize)]
pub struct TrackMetadata {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub featured_artists: Vec<String>,
    #[serde(default)]
    pub album_artists: Vec<String>,
    #[serde(default)]
    pub release_title: String,
    #[serde(default)]
    pub release_type: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_seconds: Option<f64>,
    pub audio_format: Option<String>,
    pub audio_bitrate: Option<i32>,
    pub audio_sample_rate: Option<i32>,
    pub audio_bit_depth: Option<i32>,
}
pub enum StreamEvent {
    Ready(crate::streaming::GrowingFileReader, String),
    Complete(PathBuf, Box<Option<TrackMetadata>>),
    Failed(String),
}

const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const IMAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

pub struct Client {
    service: Arc<MusicDhtService>,
    similarity_dht: Arc<SimilarityDht>,
    media_dir: PathBuf,
}

impl Client {
    pub async fn start(
        data_dir: PathBuf,
        media_dir: PathBuf,
        network: &str,
        similarity: Arc<crate::similarity::Manager>,
    ) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(&data_dir).await?;
        tokio::fs::create_dir_all(&media_dir).await?;
        let similarity_routing_path = data_dir.join("similarity-routing.sqlite3");
        let config = MusicDhtConfig::builder()
            .data_dir(&data_dir)
            .network_id(NetworkId::from_name(network))
            .rendezvous(RendezvousConfig::default())
            .stream_protocol(CATALOG_ALPN)
            .stream_protocol(AUDIO_ALPN)
            .stream_protocol(music_dht::device_sync::SYNC_ALPN_V1)
            .stream_protocol(music_dht::device_sync::SYNC_ALPN_V2)
            .schema_independent_stream_protocol(crate::federation_similarity::SIMILARITY_ALPN)
            .schema_independent_stream_protocol(SIMILARITY_DHT_ALPN)
            .build()
            .context("invalid federation configuration")?;
        let (service, mut events) = MusicDhtService::start(config)
            .await
            .context("starting federation node")?;
        tokio::spawn(async move { while events.recv().await.is_some() {} });
        let service = Arc::new(service);
        let similarity_dht = SimilarityDht::open(Arc::clone(&service), similarity_routing_path)
            .await
            .context("starting similarity routing overlay")?;
        let routing_acceptor = service
            .stream_acceptor(SIMILARITY_DHT_ALPN)
            .context("starting similarity routing listener")?;
        tokio::spawn(Arc::clone(&similarity_dht).serve(routing_acceptor));
        tokio::spawn(Arc::clone(&similarity_dht).maintenance());
        tokio::spawn(crate::federation_similarity::sync_routes(
            Arc::clone(&similarity_dht),
            Arc::clone(&similarity),
        ));
        let similarity_acceptor = service
            .stream_acceptor(crate::federation_similarity::SIMILARITY_ALPN)
            .context("starting similarity listener")?;
        tokio::spawn(crate::federation_similarity::serve(
            similarity_acceptor,
            similarity,
            service.endpoint_id(),
        ));
        Ok(Arc::new(Self {
            service,
            similarity_dht,
            media_dir,
        }))
    }

    pub fn service(&self) -> Arc<MusicDhtService> {
        Arc::clone(&self.service)
    }

    pub async fn debug_snapshot(&self) -> FederationDebugSnapshot {
        let stored_dht_records = self.service.dht_record_count().await.ok();
        let published_items = self
            .service
            .list_local_items()
            .await
            .map_or(0, |items| items.len());
        FederationDebugSnapshot {
            running: true,
            endpoint_id: self.service.endpoint_id().to_string(),
            dht_node_id: self.service.node_id().to_string(),
            connected_peers: self.service.connected_peers().len(),
            known_contacts: self.service.known_peers().len(),
            stored_dht_records,
            published_items,
            error: None,
        }
    }

    /// Resolves album artwork for a queue item independently of the screen
    /// from which the track was enqueued.
    pub async fn artwork_for_track(&self, track: &Track) -> Option<PathBuf> {
        let peer_id = match &track.audio_source {
            AudioSource::Federation { peer_id, .. } if !peer_id.is_empty() => peer_id.as_str(),
            _ => track.key.federation_id()?.0,
        };
        let owner = peer_id.parse::<EndpointId>().ok()?;
        let artist = track
            .artists
            .first()
            .map(|artist| artist.name.as_str())
            .or_else(|| (!track.artist.is_empty()).then_some(track.artist.as_str()))?;
        if track.release.is_empty() {
            return None;
        }
        self.cached_image(
            owner,
            artist,
            Some(&track.release),
            &format!("release-{peer_id}-{artist}-{}", track.release),
        )
        .await
    }

    pub async fn search(&self, query: &str) -> Result<(SearchResults, SearchStats)> {
        let outcome = self.service.search_network(query).await?;
        let own = self.service.endpoint_id();
        let mut results = convert_items(&outcome.network_results, own);
        self.fetch_artwork(&mut results).await;
        let stats = SearchStats {
            tracks: results.tracks.len(),
            artists: results.artists.len(),
            peers_queried: outcome.queried_nodes,
            duration_ms: u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX),
        };
        Ok((results, stats))
    }

    pub async fn search_similar(
        &self,
        query: crate::similarity::QueryVector,
        limit: usize,
        minimum_score: f32,
        max_tracks_per_artist: usize,
    ) -> Result<Vec<crate::federation_similarity::ScoredTrack>> {
        let mut hits = crate::federation_similarity::search(
            Arc::clone(&self.service),
            Arc::clone(&self.similarity_dht),
            query,
            limit,
            minimum_score,
            max_tracks_per_artist,
        )
        .await?;
        let mut results = SearchResults {
            tracks: hits.iter().map(|hit| hit.track.clone()).collect(),
            ..SearchResults::default()
        };
        self.fetch_artwork(&mut results).await;
        for (hit, with_artwork) in hits.iter_mut().zip(results.tracks) {
            hit.track = with_artwork;
        }
        Ok(hits)
    }

    /// Resolves a portable queue entry when another connected device only
    /// knows its stable audio content id.
    pub async fn track_by_content_id(&self, content_id: &str) -> Result<Track> {
        let outcome = self.service.search_content_id(content_id).await?;
        let own = self.service.endpoint_id();
        let items = outcome
            .local_results
            .into_iter()
            .chain(outcome.network_results)
            .collect::<Vec<_>>();
        let mut track = convert_items(&items, own)
            .tracks
            .into_iter()
            .next()
            .context("no federation peer currently publishes this track")?;
        if track.cover_uri.is_none()
            && let Some(path) = self.artwork_for_track(&track).await
        {
            track.cover_uri = Some(path.to_string_lossy().into_owned());
        }
        Ok(track)
    }

    pub async fn publish(&self, specs: Vec<ItemSpec>) -> Result<()> {
        self.service.sync_library(specs).await?;
        Ok(())
    }

    pub async fn artist_card(&self, name: &str) -> Result<(SearchResults, SearchStats)> {
        let started = std::time::Instant::now();
        let outcome = self.service.search_network(name).await?;
        let own = self.service.endpoint_id();
        let normalized = music_dht::normalize_name(name);
        let owners = outcome
            .network_results
            .iter()
            .filter(|item| {
                item.owner != own
                    && ((item.kind == ItemKind::Artist && item.normalized_name == normalized)
                        || item
                            .artist_names
                            .iter()
                            .chain(item.featured_artist_names.iter())
                            .any(|artist| music_dht::normalize_name(artist) == normalized))
            })
            .map(|item| item.owner)
            .collect::<HashSet<_>>();
        let mut catalogs = Vec::new();
        for owner in owners {
            if let Ok(Ok(catalog)) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                fetch_catalog(&self.service, owner, name),
            )
            .await
            {
                catalogs.push((owner.to_string(), catalog));
            }
        }
        let mut result = card_results(name, catalogs);
        self.fetch_artwork(&mut result).await;
        let stats = SearchStats {
            tracks: result.tracks.len(),
            artists: result.artists.len(),
            peers_queried: outcome.queried_nodes,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        Ok((result, stats))
    }

    pub async fn stream_track(
        &self,
        peer: &str,
        item_id: &str,
        directory: &std::path::Path,
        stem: &str,
        events: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let owner: EndpointId = peer.parse().context("invalid peer id")?;
        tokio::fs::create_dir_all(directory).await?;
        let mut stream = self.service.open_stream(owner, AUDIO_ALPN).await?;
        let mut request = serde_json::to_vec(&AudioRequest {
            item_id: item_id.into(),
            offset: 0,
            want_cover: false,
            metadata_only: false,
        })?;
        request.push(b'\n');
        stream.send.write_all(&request).await?;
        stream.send.finish()?;
        let header: AudioHeader = serde_json::from_slice(&read_line(&mut stream.recv).await?)?;
        anyhow::ensure!(
            header.ok,
            "{}",
            header.error.unwrap_or_else(|| "peer refused audio".into())
        );
        anyhow::ensure!(
            header.cover_size == 0 && header.artist_image_size == 0,
            "unexpected image data in audio stream"
        );
        let extension = match header.mime_type.as_str() {
            "audio/mpeg" => "mp3",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/ogg" => "ogg",
            "audio/opus" => "opus",
            "audio/wav" | "audio/x-wav" => "wav",
            "audio/mp4" | "audio/x-m4a" => "m4a",
            "audio/aac" => "aac",
            _ => "bin",
        };
        let final_path = directory.join(format!("{stem}.{extension}"));
        let part_path = directory.join(format!(".{stem}.{extension}.part"));
        let mut file = tokio::fs::File::create(&part_path).await?;
        let (reader, writer) = crate::streaming::growing_file(&part_path)?;
        let mut reader = Some(reader);
        let mut started = false;
        let mut received = 0u64;
        let threshold = header.total_size.clamp(1, STREAM_BUFFER);
        let mut chunk = vec![0; 64 * 1024];
        while let Some(n) = stream.recv.read(&mut chunk).await? {
            file.write_all(&chunk[..n]).await?;
            received += n as u64;
            writer.add_available(n as u64);
            if !started
                && received >= threshold
                && let Some(reader) = reader.take()
            {
                let _ = events
                    .send(StreamEvent::Ready(reader, header.mime_type.clone()))
                    .await;
                started = true;
            }
        }
        if !started
            && received > 0
            && let Some(reader) = reader.take()
        {
            let _ = events
                .send(StreamEvent::Ready(reader, header.mime_type.clone()))
                .await;
        }
        writer.finish();
        file.flush().await?;
        drop(file);
        anyhow::ensure!(
            header.total_size == 0 || received == header.total_size,
            "incomplete audio download: {received}/{}",
            header.total_size
        );
        tokio::fs::rename(&part_path, &final_path).await?;
        let _ = events
            .send(StreamEvent::Complete(final_path, Box::new(header.metadata)))
            .await;
        Ok(())
    }

    async fn fetch_artwork(&self, results: &mut SearchResults) {
        for artist in &mut results.artists {
            let CatalogSource::Federation { peer_id } = &artist.source else {
                continue;
            };
            let Ok(owner) = peer_id.parse::<EndpointId>() else {
                continue;
            };
            if let Some(path) = self
                .cached_image(
                    owner,
                    &artist.name,
                    None,
                    &format!("artist-{peer_id}-{}", artist.name),
                )
                .await
            {
                artist.artwork.uri = Some(path.to_string_lossy().into_owned());
            }
        }
        for release in &mut results.releases {
            let CatalogSource::Federation { peer_id } = &release.source else {
                continue;
            };
            let Some(artist) = release.artists.first() else {
                continue;
            };
            let Ok(owner) = peer_id.parse::<EndpointId>() else {
                continue;
            };
            if let Some(path) = self
                .cached_image(
                    owner,
                    &artist.name,
                    Some(&release.title),
                    &format!("release-{peer_id}-{}-{}", artist.name, release.title),
                )
                .await
            {
                let uri = path.to_string_lossy().into_owned();
                release.artwork.uri = Some(uri.clone());
                for track in &mut release.tracks {
                    if track.cover_uri.is_none() {
                        track.cover_uri = Some(uri.clone());
                    }
                }
                for track in &mut results.tracks {
                    if track.release == release.title
                        && track
                            .artists
                            .first()
                            .is_some_and(|candidate| candidate.name == artist.name)
                    {
                        track.cover_uri = Some(uri.clone());
                    }
                }
            }
        }
        for track in &mut results.tracks {
            if track.cover_uri.is_some() || track.release.is_empty() {
                continue;
            }
            let AudioSource::Federation { peer_id, .. } = &track.audio_source else {
                continue;
            };
            let Some(artist) = track.artists.first() else {
                continue;
            };
            let Ok(owner) = peer_id.parse::<EndpointId>() else {
                continue;
            };
            if let Some(path) = self
                .cached_image(
                    owner,
                    &artist.name,
                    Some(&track.release),
                    &format!("release-{peer_id}-{}-{}", artist.name, track.release),
                )
                .await
            {
                track.cover_uri = Some(path.to_string_lossy().into_owned());
            }
        }
    }

    async fn cached_image(
        &self,
        owner: EndpointId,
        artist: &str,
        release: Option<&str>,
        cache_key: &str,
    ) -> Option<PathBuf> {
        let mut hasher = DefaultHasher::new();
        cache_key.hash(&mut hasher);
        let base = self.media_dir.join(format!("{:016x}", hasher.finish()));
        for extension in ["jpg", "png", "webp", "gif", "bmp"] {
            let path = base.with_extension(extension);
            if path.is_file() {
                return Some(path);
            }
        }
        let fetched = tokio::time::timeout(
            IMAGE_TIMEOUT,
            fetch_image(&self.service, owner, artist, release),
        )
        .await
        .ok()?
        .ok()?;
        let (bytes, extension) = fetched?;
        let path = base.with_extension(extension);
        tokio::fs::write(&path, bytes).await.ok()?;
        Some(path)
    }
}

async fn fetch_catalog(
    service: &MusicDhtService,
    owner: EndpointId,
    artist: &str,
) -> Result<CatalogArtist> {
    let mut stream = service.open_stream(owner, CATALOG_ALPN).await?;
    let mut request = serde_json::to_vec(&CatalogRequest {
        artist: artist.into(),
        ..CatalogRequest::default()
    })?;
    request.push(b'\n');
    stream.send.write_all(&request).await?;
    stream.send.finish()?;
    let mut payload = Vec::new();
    stream
        .recv
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut payload)
        .await?;
    anyhow::ensure!(
        payload.len() <= 4 * 1024 * 1024,
        "catalog response is too large"
    );
    let response: CatalogResponse = serde_json::from_slice(&payload)?;
    anyhow::ensure!(
        response.ok,
        "{}",
        response.error.unwrap_or_else(|| "catalog rejected".into())
    );
    response.artist.context("empty artist catalog")
}

#[allow(
    clippy::too_many_lines,
    reason = "wire catalog conversion is clearest as one pass"
)]
fn card_results(name: &str, catalogs: Vec<(String, CatalogArtist)>) -> SearchResults {
    let mut results = SearchResults::default();
    if let Some((peer, _)) = catalogs.first() {
        results.artists.push(Artist {
            key: ArtistKey::Federation {
                peer_id: peer.clone(),
                id: music_dht::normalize_name(name),
            },
            source: CatalogSource::Federation {
                peer_id: peer.clone(),
            },
            name: name.into(),
            artwork: Artwork::default(),
            release_count: 0,
            track_count: 0,
        });
    }
    let mut release_slots = HashMap::<String, usize>::new();
    for (peer, catalog) in catalogs {
        let artist_key = ArtistKey::Federation {
            peer_id: peer.clone(),
            id: music_dht::normalize_name(name),
        };
        for remote in catalog.releases {
            let normalized = music_dht::normalize_name(&remote.title);
            let slot = *release_slots.entry(normalized.clone()).or_insert_with(|| {
                results.releases.push(Release {
                    key: ReleaseKey::Federation {
                        peer_id: peer.clone(),
                        id: format!("name:{normalized}"),
                    },
                    source: CatalogSource::Federation {
                        peer_id: peer.clone(),
                    },
                    title: remote.title.clone(),
                    artists: vec![ArtistRef {
                        key: artist_key.clone(),
                        name: name.into(),
                    }],
                    featured_artists: Vec::new(),
                    release_type: remote.release_type.clone(),
                    year: remote.year,
                    artwork: Artwork::default(),
                    tracks: Vec::new(),
                });
                results.releases.len() - 1
            });
            let release = &mut results.releases[slot];
            for item in remote.tracks {
                let duplicate = release.tracks.iter().any(|track| {
                    music_dht::normalize_name(&track.title)
                        == music_dht::normalize_name(&item.title)
                });
                if duplicate || item.item_id.is_empty() {
                    continue;
                }
                let content_id = item
                    .content_id
                    .as_deref()
                    .and_then(|id| ContentId::parse(id).ok());
                let artists = artist_refs(&peer, &item.artists);
                let featured = artist_refs(&peer, &item.featured_artists);
                let track = Track {
                    key: TrackKey::federation(
                        peer.clone(),
                        item.item_id.clone(),
                        content_id.clone(),
                    ),
                    title: item.title,
                    artist: artist_line(&item.artists, &item.featured_artists),
                    artists,
                    featured_artists: featured,
                    release: release.title.clone(),
                    release_id: release.key.clone(),
                    duration_seconds: item.duration_seconds.unwrap_or_default(),
                    track_number: item
                        .track_number
                        .and_then(|value| u32::try_from(value).ok()),
                    disc_number: item.disc_number.and_then(|value| u32::try_from(value).ok()),
                    cover_uri: release.artwork.uri.clone(),
                    audio_format: None,
                    audio_bitrate_kbps: None,
                    audio_sample_rate_hz: None,
                    audio_bit_depth: None,
                    file_size_bytes: None,
                    liked: false,
                    audio_source: AudioSource::Federation {
                        peer_id: peer.clone(),
                        content_id: content_id.unwrap_or_else(|| {
                            ContentId::parse(format!("b3:{}", "0".repeat(64)))
                                .expect("valid fallback")
                        }),
                    },
                };
                release.tracks.push(track.clone());
                results.tracks.push(track);
            }
        }
        for appearance in catalog.appears_on {
            if appearance.track.item_id.is_empty() {
                continue;
            }
            let content_id = appearance
                .track
                .content_id
                .as_deref()
                .and_then(|id| ContentId::parse(id).ok());
            let release_key = ReleaseKey::Federation {
                peer_id: peer.clone(),
                id: format!(
                    "name:{}",
                    music_dht::normalize_name(&appearance.release_title)
                ),
            };
            results.tracks.push(Track {
                key: TrackKey::federation(
                    peer.clone(),
                    appearance.track.item_id.clone(),
                    content_id.clone(),
                ),
                title: appearance.track.title,
                artist: artist_line(
                    &appearance.track.artists,
                    &appearance.track.featured_artists,
                ),
                artists: artist_refs(&peer, &appearance.track.artists),
                featured_artists: artist_refs(&peer, &appearance.track.featured_artists),
                release: appearance.release_title,
                release_id: release_key,
                duration_seconds: appearance.track.duration_seconds.unwrap_or_default(),
                track_number: appearance
                    .track
                    .track_number
                    .and_then(|value| u32::try_from(value).ok()),
                disc_number: appearance
                    .track
                    .disc_number
                    .and_then(|value| u32::try_from(value).ok()),
                cover_uri: None,
                audio_format: None,
                audio_bitrate_kbps: None,
                audio_sample_rate_hz: None,
                audio_bit_depth: None,
                file_size_bytes: None,
                liked: false,
                audio_source: AudioSource::Federation {
                    peer_id: peer.clone(),
                    content_id: content_id.unwrap_or_else(|| {
                        ContentId::parse(format!("b3:{}", "0".repeat(64))).expect("valid fallback")
                    }),
                },
            });
        }
    }
    for release in &mut results.releases {
        populate_release_contributors(release);
    }
    if let Some(artist) = results.artists.first_mut() {
        artist.release_count = results.releases.len();
        artist.track_count = results.tracks.len();
    }
    results
}

fn populate_release_contributors(release: &mut Release) {
    let mut known = release
        .artists
        .iter()
        .chain(release.featured_artists.iter())
        .map(|artist| music_dht::normalize_name(&artist.name))
        .collect::<HashSet<_>>();
    for artist in release
        .tracks
        .iter()
        .flat_map(|track| track.artists.iter().chain(track.featured_artists.iter()))
    {
        if known.insert(music_dht::normalize_name(&artist.name)) {
            release.featured_artists.push(artist.clone());
        }
    }
}

fn convert_items(items: &[LibraryItem], own: EndpointId) -> SearchResults {
    let mut results = SearchResults::default();
    let mut artists = HashMap::<(String, String), Artist>::new();
    let mut releases = HashSet::<(String, String)>::new();
    let mut tracks = HashSet::<(String, String)>::new();
    for item in items.iter().filter(|item| item.owner != own) {
        let peer = item.owner.to_string();
        let source = CatalogSource::Federation {
            peer_id: peer.clone(),
        };
        let refs = artist_refs(&peer, &item.artist_names);
        let featured = artist_refs(&peer, &item.featured_artist_names);
        for name in item
            .artist_names
            .iter()
            .chain(item.featured_artist_names.iter())
        {
            note_artist(&mut artists, &peer, name);
        }
        match item.kind {
            ItemKind::Artist => {
                note_artist(&mut artists, &peer, &item.name);
            }
            ItemKind::Release => {
                if releases.insert((peer.clone(), item.id.to_string())) {
                    results.releases.push(Release {
                        key: ReleaseKey::Federation {
                            peer_id: peer.clone(),
                            id: item.id.to_string(),
                        },
                        source,
                        title: item.name.clone(),
                        artists: refs,
                        featured_artists: Vec::new(),
                        release_type: item
                            .release_type
                            .clone()
                            .unwrap_or_else(|| "release".into()),
                        year: item.year,
                        artwork: Artwork::default(),
                        tracks: Vec::new(),
                    });
                }
            }
            ItemKind::Track => {
                if !tracks.insert((peer.clone(), item.id.to_string())) {
                    continue;
                }
                let Some(content_id) = item
                    .content_id
                    .as_deref()
                    .and_then(|value| ContentId::parse(value).ok())
                else {
                    continue;
                };
                let release = item.release_title.clone().unwrap_or_default();
                results.tracks.push(Track {
                    key: TrackKey::federation(
                        peer.clone(),
                        item.id.to_string(),
                        Some(content_id.clone()),
                    ),
                    title: item.name.clone(),
                    artist: artist_line(&item.artist_names, &item.featured_artist_names),
                    artists: refs.clone(),
                    featured_artists: featured,
                    release: release.clone(),
                    release_id: ReleaseKey::Federation {
                        peer_id: peer.clone(),
                        id: format!("name:{}", music_dht::normalize_name(&release)),
                    },
                    duration_seconds: item.duration_seconds.unwrap_or_default(),
                    track_number: item
                        .track_number
                        .and_then(|value| u32::try_from(value).ok()),
                    disc_number: item.disc_number.and_then(|value| u32::try_from(value).ok()),
                    cover_uri: None,
                    audio_format: None,
                    audio_bitrate_kbps: None,
                    audio_sample_rate_hz: None,
                    audio_bit_depth: None,
                    file_size_bytes: None,
                    liked: false,
                    audio_source: AudioSource::Federation {
                        peer_id: peer,
                        content_id,
                    },
                });
            }
        }
    }
    results.artists = artists.into_values().collect();
    results
        .artists
        .sort_by(|left, right| left.name.cmp(&right.name));
    results
}

fn note_artist(artists: &mut HashMap<(String, String), Artist>, peer: &str, name: &str) {
    let normalized = music_dht::normalize_name(name);
    if normalized.is_empty() {
        return;
    }
    artists
        .entry((peer.to_owned(), normalized.clone()))
        .or_insert_with(|| Artist {
            key: ArtistKey::Federation {
                peer_id: peer.to_owned(),
                id: normalized,
            },
            source: CatalogSource::Federation {
                peer_id: peer.to_owned(),
            },
            name: name.to_owned(),
            artwork: Artwork::default(),
            release_count: 0,
            track_count: 0,
        });
}

fn artist_refs(peer: &str, names: &[String]) -> Vec<ArtistRef> {
    names
        .iter()
        .map(|name| ArtistRef {
            key: ArtistKey::Federation {
                peer_id: peer.to_owned(),
                id: music_dht::normalize_name(name),
            },
            name: name.clone(),
        })
        .collect()
}

fn artist_line(main: &[String], featured: &[String]) -> String {
    let mut line = main.join(", ");
    if !featured.is_empty() {
        if !line.is_empty() {
            line.push_str(" feat. ");
        }
        line.push_str(&featured.join(", "));
    }
    line
}

async fn fetch_image(
    service: &MusicDhtService,
    owner: EndpointId,
    artist: &str,
    release: Option<&str>,
) -> Result<Option<(Vec<u8>, &'static str)>> {
    let mut stream = service.open_stream(owner, CATALOG_ALPN).await?;
    let request = CatalogRequest {
        artist: artist.to_owned(),
        want: Some(
            if release.is_some() {
                "release_cover"
            } else {
                "artist_image"
            }
            .into(),
        ),
        release: release.map(str::to_owned),
        cursor: None,
        limit: None,
    };
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.send.write_all(&line).await?;
    stream.send.finish()?;
    let header: CatalogImageHeader = serde_json::from_slice(&read_line(&mut stream.recv).await?)?;
    if !header.ok || header.size == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        header.size <= MAX_IMAGE_BYTES,
        "federated image exceeds size limit"
    );
    let image_size =
        usize::try_from(header.size).context("image is too large for this platform")?;
    let mut bytes = vec![0; image_size];
    stream.recv.read_exact(&mut bytes).await?;
    let extension = match header.mime_type.as_str() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "jpg",
    };
    Ok(Some((bytes, extension)))
}

async fn read_line(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            return Ok(line);
        }
        anyhow::ensure!(line.len() < 64 * 1024, "catalog header is too large");
        line.push(byte);
    }
}
