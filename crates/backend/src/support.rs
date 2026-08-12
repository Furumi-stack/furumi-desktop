use super::{
    Artist, ArtistId, ArtistKey, ArtistRef, Artwork, AudioSource, BuildInfoSnapshot,
    CONTROL_POSITION_ACK_TOLERANCE_SECONDS, CatalogSource, ContentId, DevicePlaybackRole, Duration,
    HashMap, HashSet, InternalEvent, LibrarySnapshot, LocalTrackId, PlaybackStatus,
    PlaylistSnapshot, Release, ReleaseId, ReleaseKey, SearchResults, SettingsSnapshot,
    SettingsStore, Track, TrackKey, VersionEntrySnapshot, federation, mpsc, std_mpsc, thread,
};
pub(super) fn expand_tilde(value: &str) -> std::path::PathBuf {
    if value == "~" {
        directories::UserDirs::new().map_or_else(|| value.into(), |dirs| dirs.home_dir().into())
    } else if let Some(rest) = value.strip_prefix("~/") {
        directories::UserDirs::new().map_or_else(|| value.into(), |dirs| dirs.home_dir().join(rest))
    } else {
        value.into()
    }
}

pub(super) fn normalize_device_name(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "furumi".into()
    } else {
        value.into()
    }
}

pub(super) fn selected_track_position(tracks: &[Track], selected: &TrackKey) -> usize {
    tracks
        .iter()
        .position(|track| track.key.matches(selected))
        .unwrap_or(0)
}

pub(super) fn runtime_build_info() -> BuildInfoSnapshot {
    use music_dht::capabilities::{
        CATALOG_ID, CapabilityManifest, DEVICE_SYNC_ID, FEDERATION_NET_ID, MUSIC_DHT_ID,
        RENDEZVOUS_ID, TICKET_ID,
    };

    let manifest = CapabilityManifest::frid("furumi-desktop", env!("CARGO_PKG_VERSION"));
    let protocol = |name: &str, id: &str| VersionEntrySnapshot {
        name: name.into(),
        version: manifest
            .protocols
            .get(id)
            .map_or_else(|| "unknown".into(), u16::to_string),
    };
    BuildInfoSnapshot {
        software: vec![
            VersionEntrySnapshot {
                name: "furumi-desktop".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            VersionEntrySnapshot {
                name: "furumi-library".into(),
                version: env!("FURUMI_LIBRARY_VERSION").into(),
            },
            VersionEntrySnapshot {
                name: "music-dht".into(),
                version: env!("FURUMI_MUSIC_DHT_VERSION").into(),
            },
            VersionEntrySnapshot {
                name: "federation-net".into(),
                version: env!("FURUMI_FEDERATION_NET_VERSION").into(),
            },
        ],
        protocols: vec![
            protocol("Federation transport", FEDERATION_NET_ID),
            protocol("Peer ticket", TICKET_ID),
            protocol("Rendezvous", RENDEZVOUS_ID),
            protocol("Music DHT", MUSIC_DHT_ID),
            protocol("Catalog", CATALOG_ID),
            VersionEntrySnapshot {
                name: "Audio transfer".into(),
                version: federation::AUDIO_PROTOCOL_VERSION.to_string(),
            },
            VersionEntrySnapshot {
                name: "Connected devices".into(),
                version: format!(
                    "{} · accepts v1",
                    manifest
                        .protocols
                        .get(DEVICE_SYNC_ID)
                        .copied()
                        .unwrap_or_default()
                ),
            },
        ],
    }
}

pub(super) fn find_catalog_track<'a>(
    library: &'a LibrarySnapshot,
    search: &'a SearchResults,
    key: &TrackKey,
) -> Option<&'a Track> {
    library
        .featured_releases
        .iter()
        .flat_map(|release| release.tracks.iter())
        .chain(
            library
                .playlists
                .iter()
                .flat_map(|playlist| playlist.tracks.iter()),
        )
        .chain(
            search
                .releases
                .iter()
                .flat_map(|release| release.tracks.iter()),
        )
        .chain(search.tracks.iter())
        .find(|track| track.key.matches(key))
}

pub(super) fn portable_playback_placeholder(
    wire: &music_dht::device_sync::PlaybackTrack,
    content_id: ContentId,
) -> Track {
    let peer_id = "content".to_owned();
    let refs = |names: &[String]| {
        names
            .iter()
            .map(|name| ArtistRef {
                key: ArtistKey::Federation {
                    peer_id: peer_id.clone(),
                    id: format!("name:{}", music_dht::normalize_name(name)),
                },
                name: name.clone(),
            })
            .collect::<Vec<_>>()
    };
    Track {
        key: TrackKey::remote(content_id.clone()),
        title: wire.title.clone(),
        artist: playback_artist_line(&wire.artist_names, &wire.featured_artist_names),
        artists: refs(&wire.artist_names),
        featured_artists: refs(&wire.featured_artist_names),
        release: wire.release_title.clone(),
        release_id: ReleaseKey::Federation {
            peer_id: peer_id.clone(),
            id: format!("name:{}", music_dht::normalize_name(&wire.release_title)),
        },
        duration_seconds: wire.duration_seconds,
        track_number: wire
            .track_number
            .and_then(|value| u32::try_from(value).ok()),
        disc_number: wire.disc_number.and_then(|value| u32::try_from(value).ok()),
        cover_uri: None,
        audio_format: wire.audio_format.clone(),
        audio_bitrate_kbps: wire
            .audio_bitrate
            .and_then(|value| u32::try_from(value).ok()),
        audio_sample_rate_hz: wire
            .audio_sample_rate
            .and_then(|value| u32::try_from(value).ok()),
        audio_bit_depth: wire
            .audio_bit_depth
            .and_then(|value| u32::try_from(value).ok()),
        file_size_bytes: wire
            .file_size_bytes
            .and_then(|value| u64::try_from(value).ok()),
        liked: false,
        audio_source: AudioSource::Federation {
            peer_id: String::new(),
            content_id,
        },
    }
}

pub(super) fn playback_artist_line(main: &[String], featured: &[String]) -> String {
    match (main.is_empty(), featured.is_empty()) {
        (false, false) => format!("{} feat. {}", main.join(", "), featured.join(", ")),
        (false, true) => main.join(", "),
        (true, false) => format!("feat. {}", featured.join(", ")),
        (true, true) => String::new(),
    }
}

pub(super) fn extrapolated_control_position(
    state: &music_dht::device_sync::PlaybackStateWire,
    elapsed: Duration,
) -> f64 {
    let elapsed = if state.playing && !state.paused {
        elapsed.as_secs_f64()
    } else {
        0.0
    };
    (state.position_secs + elapsed).max(0.0)
}

pub(super) fn remote_snapshot_has_authority(
    role: DevicePlaybackRole,
    active_device_id: &str,
    local_status: PlaybackStatus,
    local_queue_empty: bool,
    snapshot: &music_dht::device_sync::PlaybackSnapshot,
) -> bool {
    if !snapshot.active {
        return false;
    }
    match role {
        DevicePlaybackRole::Control => snapshot.device_id == active_device_id,
        DevicePlaybackRole::Active => local_status == PlaybackStatus::Stopped && local_queue_empty,
    }
}

pub(super) fn playback_state_acknowledges_command(
    expected: &music_dht::device_sync::PlaybackStateWire,
    actual: &music_dht::device_sync::PlaybackStateWire,
    seek: bool,
    elapsed: Duration,
) -> bool {
    let same_queue = expected.queue.len() == actual.queue.len()
        && expected
            .queue
            .iter()
            .zip(&actual.queue)
            .all(|(expected, actual)| playback_tracks_equivalent(expected, actual));
    let same_transport = expected.queue_pos == actual.queue_pos
        && expected.playing == actual.playing
        && expected.paused == actual.paused
        && expected.volume == actual.volume
        && expected.shuffle == actual.shuffle
        && expected.repeat == actual.repeat;
    if !same_queue || !same_transport {
        return false;
    }
    if !seek {
        return true;
    }
    let expected_position = extrapolated_control_position(expected, elapsed);
    (expected_position - actual.position_secs).abs() <= CONTROL_POSITION_ACK_TOLERANCE_SECONDS
}

pub(super) fn playback_tracks_equivalent(
    left: &music_dht::device_sync::PlaybackTrack,
    right: &music_dht::device_sync::PlaybackTrack,
) -> bool {
    let content_id = |track: &music_dht::device_sync::PlaybackTrack| {
        track
            .content_id
            .as_deref()
            .and_then(music_dht::normalize_content_id)
            .or_else(|| {
                track
                    .fed
                    .as_ref()
                    .and_then(|fed| music_dht::normalize_content_id(&fed.content_id))
            })
    };
    match (content_id(left), content_id(right)) {
        (Some(left), Some(right)) => left == right,
        _ => {
            music_dht::normalize_name(&left.title) == music_dht::normalize_name(&right.title)
                && music_dht::normalize_name(&left.release_title)
                    == music_dht::normalize_name(&right.release_title)
                && left.track_number == right.track_number
                && left.disc_number == right.disc_number
                && normalized_artist_names(&left.artist_names)
                    == normalized_artist_names(&right.artist_names)
        }
    }
}

pub(super) fn normalized_artist_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| music_dht::normalize_name(name))
        .collect()
}

pub(super) fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the playback volume is clamped to the wire protocol's 0..=100 range"
)]
pub(super) fn volume_percent(volume: f32) -> u8 {
    (volume.clamp(0.0, 1.0) * 100.0).round() as u8
}

pub(super) fn track_to_synced_fed(track: &Track) -> Option<music_dht::device_sync::SyncedFedTrack> {
    let (owner, item_id) = track.key.federation_id()?;
    let content_id = track.key.content_id()?.as_str().to_owned();
    Some(music_dht::device_sync::SyncedFedTrack {
        item_id: item_id.to_owned(),
        owner: owner.to_owned(),
        title: track.title.clone(),
        artist_names: track
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        featured_artist_names: track
            .featured_artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        year: None,
        duration_seconds: (track.duration_seconds.is_finite() && track.duration_seconds > 0.0)
            .then(|| {
                i64::try_from(Duration::from_secs_f64(track.duration_seconds).as_secs())
                    .unwrap_or(i64::MAX)
            }),
        content_id,
        release_title: (!track.release.is_empty()).then(|| track.release.clone()),
        track_number: track
            .track_number
            .and_then(|value| i32::try_from(value).ok()),
        disc_number: track
            .disc_number
            .and_then(|value| i32::try_from(value).ok()),
    })
}

pub(super) fn track_to_library_fed(track: &Track) -> Option<furumi_library::FederatedTrack> {
    let synced = track_to_synced_fed(track)?;
    Some(furumi_library::FederatedTrack {
        item_id: synced.item_id,
        owner: synced.owner,
        own: false,
        title: synced.title,
        artist_names: synced.artist_names,
        featured_artist_names: synced.featured_artist_names,
        year: synced.year,
        duration_seconds: synced.duration_seconds,
        content_id: Some(synced.content_id),
        release_title: synced.release_title,
        track_number: synced.track_number,
        disc_number: synced.disc_number,
    })
}

pub(super) fn track_to_playback_track(track: &Track) -> music_dht::device_sync::PlaybackTrack {
    let id = track.key.local_id().map_or(-1, LocalTrackId::get);
    let release_id = match track.release_id {
        ReleaseKey::Local(id) => id.get(),
        ReleaseKey::Federation { .. } => -1,
    };
    music_dht::device_sync::PlaybackTrack {
        id,
        title: track.title.clone(),
        track_number: track
            .track_number
            .and_then(|value| i32::try_from(value).ok()),
        disc_number: track
            .disc_number
            .and_then(|value| i32::try_from(value).ok()),
        duration_seconds: track.duration_seconds,
        artist_names: track
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        featured_artist_names: track
            .featured_artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
        release_id,
        release_title: track.release.clone(),
        release_year: None,
        file_path: String::new(),
        content_id: track.key.content_id().map(|id| id.as_str().to_owned()),
        audio_format: track.audio_format.clone(),
        audio_bitrate: track
            .audio_bitrate_kbps
            .and_then(|value| i32::try_from(value).ok()),
        audio_sample_rate: track
            .audio_sample_rate_hz
            .and_then(|value| i32::try_from(value).ok()),
        audio_bit_depth: track
            .audio_bit_depth
            .and_then(|value| i32::try_from(value).ok()),
        file_size_bytes: track
            .file_size_bytes
            .and_then(|value| i64::try_from(value).ok()),
        play_count: 0,
        fed: track_to_synced_fed(track),
    }
}

pub(super) fn sanitize_filename(value: &str) -> String {
    let clean: String = value
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "track".into()
    } else {
        clean
    }
}

pub(super) fn federation_specs(
    library: &furumi_library::Library,
) -> anyhow::Result<Vec<music_dht::ItemSpec>> {
    let export = library.federation_export()?;
    let mut specs = Vec::new();
    for (id, name) in export.artists {
        specs.push(music_dht::ItemSpec {
            local_key: format!("artist:{id}"),
            kind: music_dht::ItemKind::Artist,
            name,
            artist_names: Vec::new(),
            featured_artist_names: Vec::new(),
            year: None,
            release_type: None,
            release_title: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            content_id: None,
        });
    }
    for release in export.releases {
        specs.push(music_dht::ItemSpec {
            local_key: format!("release:{}", release.id),
            kind: music_dht::ItemKind::Release,
            name: release.title,
            artist_names: release.artist_names,
            featured_artist_names: Vec::new(),
            year: release.year,
            release_type: Some(release.release_type),
            release_title: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            content_id: None,
        });
    }
    for track in export.tracks {
        specs.push(music_dht::ItemSpec {
            local_key: format!("track:{}", track.id),
            kind: music_dht::ItemKind::Track,
            name: track.title,
            artist_names: track.artist_names,
            featured_artist_names: track.featured_artist_names,
            year: track.year,
            release_type: Some(track.release_type),
            release_title: Some(track.release_title),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_seconds: (track.duration_seconds > 0.0).then_some(track.duration_seconds),
            content_id: track.content_id,
        });
    }
    Ok(specs)
}

pub(super) fn apply_federated_metadata(
    import: &mut furumi_library::import::TrackImport,
    metadata: Option<federation::TrackMetadata>,
) {
    let Some(metadata) = metadata else {
        return;
    };
    if !metadata.title.trim().is_empty() {
        import.title = metadata.title;
    }
    if !metadata.artists.is_empty() {
        import.artists = metadata.artists;
    }
    if !metadata.featured_artists.is_empty() {
        import.featured_artists = metadata.featured_artists;
    }
    if !metadata.album_artists.is_empty() {
        import.album_artists = metadata.album_artists;
    }
    if !metadata.release_title.trim().is_empty() {
        import.release_title = metadata.release_title;
    }
    import.release_type = metadata.release_type.or(import.release_type.take());
    import.year = metadata.year.or(import.year);
    import.track_number = metadata.track_number.or(import.track_number);
    import.disc_number = metadata.disc_number.or(import.disc_number);
    import.duration_seconds = metadata.duration_seconds.unwrap_or(import.duration_seconds);
    import.audio_format = metadata.audio_format.or(import.audio_format.take());
    import.audio_bitrate = metadata.audio_bitrate.or(import.audio_bitrate);
    import.audio_sample_rate = metadata.audio_sample_rate.or(import.audio_sample_rate);
    import.audio_bit_depth = metadata.audio_bit_depth.or(import.audio_bit_depth);
}

pub(super) fn spawn_settings_worker(
    store: SettingsStore,
    receiver: std_mpsc::Receiver<SettingsSnapshot>,
    events: mpsc::Sender<InternalEvent>,
) {
    let report = events.clone();
    if let Err(error) = thread::Builder::new()
        .name("furumi-settings-storage".into())
        .spawn(move || {
            while let Ok(mut settings) = receiver.recv() {
                for newer in receiver.try_iter() {
                    settings = newer;
                }
                let result = store.save(&settings).map_err(|error| error.to_string());
                if events
                    .blocking_send(InternalEvent::SettingsPersisted(result))
                    .is_err()
                {
                    break;
                }
            }
        })
    {
        let _ = report.blocking_send(InternalEvent::SettingsPersisted(Err(format!(
            "settings worker: {error}"
        ))));
    }
}

pub(super) fn local_search_results(
    catalog: &furumi_library::Library,
    query: &str,
) -> anyhow::Result<SearchResults> {
    let found = catalog.search(query, 50)?;
    let artists = found
        .artists
        .into_iter()
        .map(|artist| Artist {
            key: ArtistKey::local(ArtistId::new(artist.id)),
            source: CatalogSource::Local,
            name: artist.name,
            artwork: Artwork {
                uri: artist.image_path,
            },
            release_count: usize::try_from(artist.release_count.max(0)).unwrap_or(usize::MAX),
            track_count: usize::try_from(artist.track_count.max(0)).unwrap_or(usize::MAX),
        })
        .collect();
    let mut releases = Vec::with_capacity(found.releases.len());
    for card in found.releases {
        let detail = catalog.release(card.id)?;
        releases.push(library_release(detail));
    }
    let tracks = found
        .tracks
        .into_iter()
        .map(|track| library_track(track, ""))
        .collect();
    Ok(SearchResults {
        artists,
        releases,
        tracks,
    })
}

pub(super) fn merge_search_results(target: &mut SearchResults, incoming: SearchResults) {
    for artist in incoming.artists {
        if let Some(existing) = target
            .artists
            .iter_mut()
            .find(|item| item.key == artist.key)
        {
            existing.release_count = existing.release_count.max(artist.release_count);
            existing.track_count = existing.track_count.max(artist.track_count);
            if existing.artwork.uri.is_none() && artist.artwork.uri.is_some() {
                existing.artwork = artist.artwork;
            }
        } else {
            target.artists.push(artist);
        }
    }
    for mut release in incoming.releases {
        populate_release_contributors(&mut release);
        sort_release_tracks(&mut release.tracks);
        if let Some(existing) = target
            .releases
            .iter_mut()
            .find(|item| item.key == release.key)
        {
            merge_release_preserving_local(existing, release);
        } else {
            target.releases.push(release);
        }
    }
    for track in incoming.tracks {
        if let Some(existing) = target
            .tracks
            .iter_mut()
            .find(|item| item.same_catalog_track(&track))
        {
            if matches!(existing.audio_source, AudioSource::Federation { .. })
                && matches!(track.audio_source, AudioSource::LocalFile(_))
            {
                *existing = track;
            }
        } else {
            target.tracks.push(track);
        }
    }
}

pub(super) fn merge_release_preserving_local(target: &mut Release, incoming: Release) {
    if matches!(target.source, CatalogSource::Federation { .. })
        && matches!(incoming.source, CatalogSource::Local)
    {
        let previous = std::mem::replace(target, incoming);
        merge_release_preserving_local(target, previous);
        return;
    }
    if target.artwork.uri.is_none() && incoming.artwork.uri.is_some() {
        target.artwork = incoming.artwork;
    }
    merge_artist_refs(&mut target.artists, incoming.artists);
    merge_artist_refs(&mut target.featured_artists, incoming.featured_artists);
    for track in incoming.tracks {
        if let Some(current) = target
            .tracks
            .iter_mut()
            .find(|item| tracks_match_within_release(item, &track))
        {
            if matches!(current.audio_source, AudioSource::Federation { .. })
                && matches!(track.audio_source, AudioSource::LocalFile(_))
            {
                *current = track;
            }
        } else {
            target.tracks.push(track);
        }
    }
    sort_release_tracks(&mut target.tracks);
    populate_release_contributors(target);
}

fn sort_release_tracks(tracks: &mut [Track]) {
    tracks.sort_by(|left, right| {
        left.disc_number
            .unwrap_or(1)
            .cmp(&right.disc_number.unwrap_or(1))
            .then_with(|| {
                left.track_number
                    .unwrap_or(u32::MAX)
                    .cmp(&right.track_number.unwrap_or(u32::MAX))
            })
            .then_with(|| left.title.cmp(&right.title))
    });
}

/// Match provider records within an already identified release. Track/disc
/// position is more reliable here than optional remote descriptive metadata.
pub(super) fn tracks_match_within_release(left: &Track, right: &Track) -> bool {
    if left.key.matches(&right.key) {
        return true;
    }
    if let (Some(left_number), Some(right_number)) = (left.track_number, right.track_number)
        && left_number == right_number
        && left.disc_number.unwrap_or(1) == right.disc_number.unwrap_or(1)
    {
        return true;
    }
    music_dht::normalize_name(&left.title) == music_dht::normalize_name(&right.title)
}

pub(super) fn merge_artist_refs(target: &mut Vec<ArtistRef>, incoming: Vec<ArtistRef>) {
    let mut known = target
        .iter()
        .map(|artist| music_dht::normalize_name(&artist.name))
        .collect::<HashSet<_>>();
    target.extend(
        incoming
            .into_iter()
            .filter(|artist| known.insert(music_dht::normalize_name(&artist.name))),
    );
}

pub(super) fn populate_release_contributors(release: &mut Release) {
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

pub(super) fn library_release(detail: furumi_library::ReleaseDetail) -> Release {
    let (artists, featured_artists) = release_artist_roles(&detail);
    let fallback = artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Release {
        key: ReleaseKey::local(ReleaseId::new(detail.id)),
        source: CatalogSource::Local,
        title: detail.title,
        artists,
        featured_artists,
        release_type: detail.release_type,
        year: detail.year,
        artwork: Artwork {
            uri: detail.cover_path,
        },
        tracks: detail
            .tracks
            .into_iter()
            .map(|track| library_track(track, &fallback))
            .collect(),
    }
}

pub(super) fn library_snapshot(
    catalog: &furumi_library::Library,
) -> anyhow::Result<LibrarySnapshot> {
    let liked_ids = catalog
        .liked_content_ids()?
        .into_iter()
        .chain(catalog.fed_like_ids()?)
        .collect::<HashSet<_>>();
    let artist_cards = catalog.artists(
        1,
        i64::MAX,
        furumi_library::LibraryFilters {
            source_mode: furumi_library::LibrarySourceMode::Local,
            ..furumi_library::LibraryFilters::default()
        },
    )?;
    let artists = artist_cards
        .items
        .into_iter()
        .map(|artist| Artist {
            key: ArtistKey::local(ArtistId::new(artist.id)),
            source: CatalogSource::Local,
            name: artist.name,
            artwork: Artwork {
                uri: artist.image_path,
            },
            release_count: usize::try_from(artist.release_count.max(0)).unwrap_or(usize::MAX),
            track_count: usize::try_from(artist.track_count.max(0)).unwrap_or(usize::MAX),
        })
        .collect();
    let mut releases = Vec::new();
    for card in catalog.releases()? {
        let detail = catalog.release(card.id)?;
        let (artist_refs, featured_artist_refs) = release_artist_roles(&detail);
        let artist_line = artist_refs
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut tracks = detail
            .tracks
            .into_iter()
            .map(|track| library_track(track, &artist_line))
            .collect::<Vec<_>>();
        for track in &mut tracks {
            track.liked = track_is_liked(track, &liked_ids);
        }
        releases.push(Release {
            key: ReleaseKey::local(ReleaseId::new(detail.id)),
            source: CatalogSource::Local,
            title: detail.title,
            artists: artist_refs,
            featured_artists: featured_artist_refs,
            release_type: detail.release_type,
            year: detail.year,
            artwork: Artwork {
                uri: detail.cover_path,
            },
            tracks,
        });
    }
    let recently_played = releases
        .iter()
        .flat_map(|release| release.tracks.iter().cloned())
        .take(12)
        .collect();
    let mut playlists = Vec::new();
    for card in catalog.playlists()? {
        let detail = catalog.playlist(card.id)?;
        let mut tracks = detail
            .tracks
            .into_iter()
            .map(|track| library_track(track, ""))
            .collect::<Vec<_>>();
        for track in &mut tracks {
            track.liked = track_is_liked(track, &liked_ids);
        }
        playlists.push(PlaylistSnapshot {
            id: card.id,
            title: detail.title,
            is_likes: card.kind == "likes",
            tracks,
        });
    }
    Ok(LibrarySnapshot {
        artists,
        featured_releases: releases,
        recently_played,
        playlists,
    })
}

pub(super) fn track_is_liked(track: &Track, liked_ids: &HashSet<String>) -> bool {
    track
        .key
        .content_id()
        .is_some_and(|id| liked_ids.contains(id.as_str()))
        || track
            .key
            .federation_id()
            .is_some_and(|(_, item)| liked_ids.contains(item))
}

pub(super) fn release_artist_roles(
    detail: &furumi_library::ReleaseDetail,
) -> (Vec<ArtistRef>, Vec<ArtistRef>) {
    let mut main_counts = HashMap::<i64, usize>::new();
    let mut featured = HashMap::<i64, String>::new();
    for track in &detail.tracks {
        for artist in &track.artists {
            *main_counts.entry(artist.id).or_default() += 1;
        }
        for artist in &track.featured_artists {
            featured
                .entry(artist.id)
                .or_insert_with(|| artist.name.clone());
        }
    }

    let mut main_artists = Vec::new();
    let mut featured_artists = Vec::new();
    let mut known = HashSet::new();
    for artist in &detail.artists {
        known.insert(artist.id);
        let reference = ArtistRef {
            key: ArtistKey::local(ArtistId::new(artist.id)),
            name: artist.name.clone(),
        };
        if main_counts.contains_key(&artist.id) || !featured.contains_key(&artist.id) {
            main_artists.push(reference);
        } else {
            featured_artists.push(reference);
        }
    }
    for track in &detail.tracks {
        for artist in &track.artists {
            if known.insert(artist.id) {
                main_artists.push(ArtistRef {
                    key: ArtistKey::local(ArtistId::new(artist.id)),
                    name: artist.name.clone(),
                });
            }
        }
    }
    for (id, name) in featured {
        if known.insert(id) {
            featured_artists.push(ArtistRef {
                key: ArtistKey::local(ArtistId::new(id)),
                name,
            });
        }
    }
    main_artists.sort_by(|left, right| {
        let count = |artist: &ArtistRef| match artist.key {
            ArtistKey::Local(id) => main_counts.get(&id.get()).copied().unwrap_or_default(),
            ArtistKey::Federation { .. } => 0,
        };
        count(right).cmp(&count(left))
    });
    (main_artists, featured_artists)
}

pub(super) fn library_track(track: furumi_library::TrackItem, fallback_artist: &str) -> Track {
    let content_id = track
        .content_id
        .as_deref()
        .and_then(|content_id| ContentId::parse(content_id).ok());
    let local_id = LocalTrackId::new(track.id);
    let key = content_id.map_or_else(
        || TrackKey::local(local_id),
        |content_id| TrackKey::new(local_id, content_id),
    );
    let artists = track
        .artists
        .iter()
        .map(|artist| ArtistRef {
            key: ArtistKey::local(ArtistId::new(artist.id)),
            name: artist.name.clone(),
        })
        .collect::<Vec<_>>();
    let featured_artists = track
        .featured_artists
        .iter()
        .map(|artist| ArtistRef {
            key: ArtistKey::local(ArtistId::new(artist.id)),
            name: artist.name.clone(),
        })
        .collect::<Vec<_>>();
    let artist = {
        let value = track.artist_line();
        if value.is_empty() {
            fallback_artist.to_string()
        } else {
            value
        }
    };
    Track {
        key,
        title: track.title,
        artist,
        artists,
        featured_artists,
        release: track.release_title,
        release_id: ReleaseKey::local(ReleaseId::new(track.release_id)),
        duration_seconds: track.duration_seconds,
        track_number: track
            .track_number
            .and_then(|value| u32::try_from(value).ok()),
        disc_number: track
            .disc_number
            .and_then(|value| u32::try_from(value).ok()),
        cover_uri: track.cover_path,
        audio_format: track.audio_format,
        audio_bitrate_kbps: track
            .audio_bitrate
            .and_then(|value| u32::try_from(value).ok()),
        audio_sample_rate_hz: track
            .audio_sample_rate
            .and_then(|value| u32::try_from(value).ok()),
        audio_bit_depth: track
            .audio_bit_depth
            .and_then(|value| u32::try_from(value).ok()),
        file_size_bytes: track
            .file_size_bytes
            .and_then(|value| u64::try_from(value).ok()),
        liked: false,
        audio_source: AudioSource::LocalFile(track.file_path.into()),
    }
}
