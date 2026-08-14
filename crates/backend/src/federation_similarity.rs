//! Local-index adapter and DHT-routed peer client for Furumi similarity.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use furumi_domain::{ArtistKey, ArtistRef, AudioSource, ContentId, ReleaseKey, Track, TrackKey};
use futures_util::stream::{self, StreamExt as _};
use music_dht::similarity::{self as wire, SimilarityHit, SimilarityRequest, SimilarityResponse};
use music_dht::similarity_dht::SimilarityDht;
use music_dht::{EndpointId, ItemId, ItemKind, MusicDhtService, PeerTicket, StreamAcceptor};

use crate::similarity::{Manager, QueryVector};

pub use music_dht::similarity::SIMILARITY_ALPN;

const MAX_QUERY_PEERS: usize = 48;
const QUERY_CONCURRENCY: usize = 8;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const ROUTING_TIMEOUT: Duration = Duration::from_secs(5);
const ROUTE_SYNC_INTERVAL: Duration = Duration::from_secs(30);
const MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE: u32 = 8;

pub struct ScoredTrack {
    pub track: Track,
    pub score: f32,
    pub embedding_signature: Option<[u8; wire::SIMILARITY_SIGNATURE_BYTES]>,
}

pub async fn serve(mut acceptor: StreamAcceptor, manager: Arc<Manager>, own: EndpointId) {
    while let Some(stream) = acceptor.accept().await {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = serve_one(stream, manager, own).await;
        });
    }
}

async fn serve_one(
    mut stream: music_dht::ByteStream,
    manager: Arc<Manager>,
    own: EndpointId,
) -> Result<()> {
    let request = wire::read_request(&mut stream).await?;
    let response = if manager.network_allowed() {
        let matches = tokio::task::spawn_blocking(move || {
            manager.search_vector_for_peer(&request.profile_id, &request.vector, request.limit)
        })
        .await
        .context("local similarity task failed")
        .and_then(|result| result);
        match matches {
            Ok(matches) => SimilarityResponse::success(
                matches
                    .into_iter()
                    .filter_map(|found| {
                        let track = found.track;
                        let hit = SimilarityHit {
                            score: found.score,
                            item_id: hex_encode(
                                ItemId::derive(
                                    &own,
                                    ItemKind::Track,
                                    &format!("track:{}", track.id),
                                )
                                .as_bytes(),
                            ),
                            title: track.title,
                            artist_names: track
                                .artists
                                .into_iter()
                                .map(|artist| artist.name)
                                .collect(),
                            featured_artist_names: track
                                .featured_artists
                                .into_iter()
                                .map(|artist| artist.name)
                                .collect(),
                            year: track.release_year,
                            duration_seconds: Some(
                                crate::support::seconds_to_milliseconds(track.duration_seconds)
                                    / 1_000,
                            ),
                            content_id: track.content_id,
                            release_title: Some(track.release_title),
                            track_number: track.track_number,
                            disc_number: track.disc_number,
                            embedding_signature: Some(found.embedding_signature),
                        };
                        hit.validate().is_ok().then_some(hit)
                    })
                    .collect(),
            )?,
            Err(error) => {
                SimilarityResponse::refused(format!("similarity query is unavailable: {error:#}"))?
            }
        }
    } else {
        SimilarityResponse::refused("similarity federation is disabled or has no privacy consent")?
    };
    wire::write_response(&mut stream, &response).await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded peer fan-out and ranking policy"
)]
pub async fn search(
    service: Arc<MusicDhtService>,
    routing: Arc<SimilarityDht>,
    query: QueryVector,
    limit: usize,
    minimum_score: f32,
    max_tracks_per_artist: usize,
) -> Result<Vec<ScoredTrack>> {
    let own = service.endpoint_id();
    let routed = tokio::time::timeout(
        ROUTING_TIMEOUT,
        routing.find_peers(&query.profile_id, &query.vector, MAX_QUERY_PEERS),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut peers = routed
        .into_iter()
        .filter_map(|ticket| {
            let owner = ticket.endpoint_id();
            (owner != own && seen.insert(owner)).then_some(QueryPeer {
                owner,
                ticket: Some(ticket),
            })
        })
        .collect::<Vec<_>>();
    for owner in service
        .connected_peers()
        .into_iter()
        .chain(service.known_peers().into_iter().map(|peer| peer.peer_id))
    {
        if owner != own && seen.insert(owner) {
            peers.push(QueryPeer {
                owner,
                ticket: None,
            });
        }
        if peers.len() >= MAX_QUERY_PEERS {
            break;
        }
    }
    let query_signature = wire::embedding_signature(&query.vector)?;
    let request = Arc::new(SimilarityRequest::new(
        query.profile_id,
        query.vector,
        limit.clamp(1, wire::MAX_SIMILARITY_RESULTS),
    )?);
    let responses = stream::iter(peers.into_iter().map(|peer| {
        let service = Arc::clone(&service);
        let request = Arc::clone(&request);
        async move {
            tokio::time::timeout(QUERY_TIMEOUT, query_peer(service, peer, &request))
                .await
                .map_err(|_| anyhow::anyhow!("similarity peer timed out"))?
        }
    }))
    .buffer_unordered(QUERY_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut hits = responses
        .into_iter()
        .filter_map(Result::ok)
        .flatten()
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| right.1.total_cmp(&left.1));
    let mut dedup = HashSet::new();
    let mut signatures = vec![query_signature];
    let mut artist_counts = HashMap::<String, usize>::new();
    let mut results = Vec::new();
    for (track, score, signature) in hits {
        if score < minimum_score {
            break;
        }
        if query.source_content_id.as_deref().is_some_and(|source| {
            track
                .key
                .content_id()
                .is_some_and(|id| id.as_str() == source)
        }) {
            continue;
        }
        let identity = track
            .key
            .content_id()
            .map_or_else(|| format!("{:?}", track.key), |id| id.as_str().to_owned());
        if !dedup.insert(identity) {
            continue;
        }
        if signature.is_some_and(|candidate| {
            signatures.iter().any(|existing| {
                wire::signature_distance(&candidate, existing)
                    <= MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE
            })
        }) {
            continue;
        }
        let artist = track
            .artists
            .first()
            .map(|artist| music_dht::normalize_name(&artist.name))
            .unwrap_or_default();
        let count = artist_counts.entry(artist.clone()).or_default();
        if !artist.is_empty() && *count >= max_tracks_per_artist {
            continue;
        }
        *count += 1;
        if let Some(signature) = signature {
            signatures.push(signature);
        }
        results.push(ScoredTrack {
            track,
            score,
            embedding_signature: signature,
        });
        if results.len() >= limit.min(wire::MAX_SIMILARITY_RESULTS) {
            break;
        }
    }
    Ok(results)
}

type PeerHits = Vec<(Track, f32, Option<[u8; wire::SIMILARITY_SIGNATURE_BYTES]>)>;

#[derive(Clone)]
struct QueryPeer {
    owner: EndpointId,
    ticket: Option<PeerTicket>,
}

async fn query_peer(
    service: Arc<MusicDhtService>,
    peer: QueryPeer,
    request: &SimilarityRequest,
) -> Result<PeerHits> {
    let owner = peer.owner;
    let mut stream = match peer.ticket {
        Some(ticket) => service.open_stream_to(&ticket, SIMILARITY_ALPN).await,
        None => service.open_stream(owner, SIMILARITY_ALPN).await,
    }?;
    let response = wire::exchange(&mut stream, request).await?;
    anyhow::ensure!(
        response.ok,
        "peer refused similarity query: {}",
        response.error.unwrap_or_default()
    );
    Ok(response
        .hits
        .into_iter()
        .filter_map(|hit| hit_to_track(owner, hit))
        .collect())
}

/// Keeps the routing overlay synchronized with the active durable index.
pub async fn sync_routes(routing: Arc<SimilarityDht>, manager: Arc<Manager>) {
    let mut interval = tokio::time::interval(ROUTE_SYNC_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut published_marker: Option<(String, blake3::Hash)> = None;
    loop {
        interval.tick().await;
        let manager = Arc::clone(&manager);
        let loaded = tokio::task::spawn_blocking(move || manager.routing_signatures()).await;
        let Ok(Ok(snapshot)) = loaded else {
            continue;
        };
        let Some((profile_id, signatures)) = snapshot else {
            if published_marker.take().is_some() {
                routing.clear_local_signatures();
            }
            continue;
        };
        let mut hasher = blake3::Hasher::new();
        for signature in &signatures {
            hasher.update(signature);
        }
        let marker = (profile_id.clone(), hasher.finalize());
        if published_marker.as_ref() == Some(&marker) {
            continue;
        }
        if routing
            .sync_local_signatures(profile_id, signatures)
            .await
            .is_ok()
        {
            published_marker = Some(marker);
        }
    }
}

fn hit_to_track(
    owner: EndpointId,
    hit: SimilarityHit,
) -> Option<(Track, f32, Option<[u8; wire::SIMILARITY_SIGNATURE_BYTES]>)> {
    let peer_id = owner.to_string();
    let content_id = hit
        .content_id
        .as_deref()
        .and_then(|id| ContentId::parse(id).ok())?;
    let refs = |names: Vec<String>| {
        names
            .into_iter()
            .map(|name| ArtistRef {
                key: ArtistKey::Federation {
                    peer_id: peer_id.clone(),
                    id: music_dht::normalize_name(&name),
                },
                name,
            })
            .collect::<Vec<_>>()
    };
    let artists = refs(hit.artist_names);
    let featured_artists = refs(hit.featured_artist_names);
    let artist = artists
        .iter()
        .map(|artist| artist.name.as_str())
        .chain(featured_artists.iter().map(|artist| artist.name.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let release = hit.release_title.unwrap_or_default();
    let score = hit.score;
    let signature = hit.embedding_signature;
    Some((
        Track {
            key: TrackKey::federation(peer_id.clone(), hit.item_id, Some(content_id.clone())),
            title: hit.title,
            artist,
            artists,
            featured_artists,
            release: release.clone(),
            release_id: ReleaseKey::Federation {
                peer_id: peer_id.clone(),
                id: format!("name:{}", music_dht::normalize_name(&release)),
            },
            duration_seconds: hit
                .duration_seconds
                .and_then(|value| u32::try_from(value).ok())
                .map_or(0.0, f64::from),
            track_number: hit.track_number.and_then(|value| u32::try_from(value).ok()),
            disc_number: hit.disc_number.and_then(|value| u32::try_from(value).ok()),
            cover_uri: None,
            audio_format: None,
            audio_bitrate_kbps: None,
            audio_sample_rate_hz: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            liked: false,
            audio_source: AudioSource::Federation {
                peer_id,
                content_id,
            },
        },
        score,
        signature,
    ))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
