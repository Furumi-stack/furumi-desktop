use super::*;
pub(super) fn render(window: &AppWindow, state: &AppState) {
    render_shell(window, state);
    render_catalog(window, state);
    render_search(window, state);
    render_queue(window, state);
    render_current_track(window, state);
    render_playback(window, state);
}

pub(super) fn render_shell(window: &AppWindow, state: &AppState) {
    let strings = state.frontend.locale.strings();
    window.set_app_name(strings.app_name.into());
    window.set_home_label(strings.home.into());
    window.set_search_label(strings.search.into());
    window.set_library_label(strings.library.into());
    window.set_queue_label(strings.queue.into());
    window.set_recent_label(strings.recently_played.into());
    window.set_featured_label(strings.made_for_listening.into());
    window.set_search_placeholder(strings.search_placeholder.into());
    window.set_active_screen(
        match &state.frontend.screen {
            Screen::Home => "home",
            Screen::Search => "search",
            Screen::Library => "library",
            Screen::Artist(_) => "artist",
            Screen::Release(_, _) => "release",
            Screen::Playlist(_) => "playlist",
        }
        .into(),
    );
    window.set_can_go_back(!state.frontend.navigation_history.is_empty());
    window.set_can_go_forward(!state.frontend.navigation_forward.is_empty());
    window.set_breadcrumbs(model(breadcrumbs(state)));
    window.set_queue_open(state.frontend.queue_open);
    window.set_settings_open(state.frontend.settings_open);
    window.set_playlist_picker_open(state.frontend.playlist_picker_track.is_some());
    window.set_network_id(state.backend.settings.network_id.clone().into());
    window.set_device_name(state.backend.settings.device_name.clone().into());
    window.set_library_path(state.backend.settings.library_path.clone().into());
    window.set_federation_enabled(state.backend.settings.federation_enabled);
    window.set_save_federated_on_listen(state.backend.settings.save_federated_on_listen);
    window.set_selected_language(state.backend.settings.language.clone().into());
    window.set_available_languages(model(vec![SharedString::from("English")]));
    window.set_search_query(state.frontend.search_query.clone().into());
    render_playlist_shell(window, state);
    render_device_shell(window, state);
    let (network_busy, network_text) = federation_status(state);
    window.set_federation_status_busy(network_busy);
    window.set_federation_status_text(network_text.into());
    render_federation_debug(window, state);
    render_build_info(window, state);
    window.set_error_message(
        state
            .transient_error
            .clone()
            .or_else(|| state.backend.playback_error.clone())
            .or_else(|| state.backend.settings_error.clone())
            .unwrap_or_default()
            .into(),
    );
    render_track_info(window, state);
}

fn render_federation_debug(window: &AppWindow, state: &AppState) {
    let debug = &state.backend.federation_debug;
    window.set_federation_debug_node(
        if debug.running {
            "Running"
        } else if debug.error.is_some() {
            "Error"
        } else {
            "Stopped"
        }
        .into(),
    );
    window.set_federation_debug_peers(
        format!(
            "{} connected · {} known",
            debug.connected_peers, debug.known_contacts
        )
        .into(),
    );
    window.set_federation_debug_dht(
        debug
            .stored_dht_records
            .map_or_else(|| "n/a".into(), |records| format!("{records} records"))
            .into(),
    );
    window.set_federation_debug_published(format!("{} items", debug.published_items).into());
    window.set_federation_debug_endpoint(
        federation_node_ids(&debug.endpoint_id, &debug.dht_node_id).into(),
    );
}

fn render_build_info(window: &AppWindow, state: &AppState) {
    window.set_software_versions(model(
        state
            .backend
            .build_info
            .software
            .iter()
            .map(|entry| VersionView {
                name: entry.name.clone().into(),
                version: entry.version.clone().into(),
            })
            .collect(),
    ));
    window.set_protocol_versions(model(
        state
            .backend
            .build_info
            .protocols
            .iter()
            .map(|entry| VersionView {
                name: entry.name.clone().into(),
                version: entry.version.clone().into(),
            })
            .collect(),
    ));
}

fn render_playlist_shell(window: &AppWindow, state: &AppState) {
    let playlists = ready_library(state)
        .map(|library| {
            library
                .playlists
                .iter()
                .map(|playlist| PlaylistView {
                    id: playlist.id.to_string().into(),
                    title: playlist.title.clone().into(),
                    details: format!(
                        "{} {}",
                        playlist.tracks.len(),
                        if playlist.tracks.len() == 1 {
                            "track"
                        } else {
                            "tracks"
                        }
                    )
                    .into(),
                    is_likes: playlist.is_likes,
                })
                .collect()
        })
        .unwrap_or_default();
    window.set_playlists(model(playlists));
}

fn render_device_shell(window: &AppWindow, state: &AppState) {
    let devices = &state.backend.connected_devices;
    window.set_connected_devices(model(
        devices
            .devices
            .iter()
            .filter(|device| device.trust != DeviceTrust::Revoked)
            .map(|device| DeviceView {
                id: device.id.clone().into(),
                name: device.name.clone().into(),
                details: format!(
                    "{} · {}",
                    if device.presence == DevicePresence::Online {
                        "online"
                    } else {
                        "offline"
                    },
                    if device.trust == DeviceTrust::Revoked {
                        "revoked".to_string()
                    } else if device.client_version.is_empty() {
                        "Furumi".to_string()
                    } else {
                        format!("v{}", device.client_version)
                    }
                )
                .into(),
                is_self: device.is_self,
                online: device.presence == DevicePresence::Online,
                active: device.is_active,
                revoked: device.trust == DeviceTrust::Revoked,
            })
            .collect(),
    ));
    window.set_pending_pairings(model(
        devices
            .pending_pairings
            .iter()
            .map(|pending| PairingView {
                request_id: pending.request_id.clone().into(),
                name: pending.name.clone().into(),
                details: format!("Furumi {} · {}", pending.client_version, pending.device_id)
                    .into(),
                group_conflict: pending.requester_group_id.is_some()
                    && pending.requester_group_active_devices > 1,
            })
            .collect(),
    ));
    window.set_device_role_label(
        match devices.role {
            DevicePlaybackRole::Active => "Active device",
            DevicePlaybackRole::Control => "Control device",
        }
        .into(),
    );
    window.set_device_is_active(devices.role == DevicePlaybackRole::Active);
    window.set_active_device_label(devices.active_device_name.clone().into());
    window.set_device_group_label(format!("Network · {}", devices.group_id).into());
    window.set_device_invite(devices.invite.clone().unwrap_or_default().into());
    window.set_device_busy(devices.busy);
    window.set_device_status(
        devices
            .error
            .clone()
            .or_else(|| devices.last_sync.clone())
            .unwrap_or_default()
            .into(),
    );
}

pub(super) fn render_track_info(window: &AppWindow, state: &AppState) {
    let track = state
        .frontend
        .track_info
        .as_ref()
        .and_then(|key| find_track(state, key));
    window.set_track_info_open(track.is_some());
    window.set_track_info_title(
        track
            .map_or_else(String::new, |track| track.title.clone())
            .into(),
    );
    window.set_track_info_artist_links(model(track.map_or_else(Vec::new, track_artist_links)));
    window.set_track_info_release(
        track
            .map_or_else(String::new, |track| {
                let mut label = track.release.clone();
                if let Some(disc) = track.disc_number {
                    let _ = write!(label, " · disc {disc}");
                }
                if let Some(number) = track.track_number {
                    let _ = write!(label, " · track {number}");
                }
                label
            })
            .into(),
    );
    window.set_track_info_duration(
        track
            .map_or_else(String::new, |track| {
                format!(
                    "{} ({:.2} s)",
                    duration_label(track.duration_seconds),
                    track.duration_seconds
                )
            })
            .into(),
    );
    window.set_track_info_format(
        track
            .and_then(|track| track.audio_format.clone())
            .unwrap_or_default()
            .to_uppercase()
            .into(),
    );
    window.set_track_info_quality(track.map(track_quality_label).unwrap_or_default().into());
    window.set_track_info_source(
        track
            .map_or_else(String::new, |track| match track.audio_source {
                AudioSource::LocalFile(_) => "Local library".into(),
                AudioSource::Federation { .. } => "Federation".into(),
            })
            .into(),
    );
    window.set_track_info_content_id(
        track
            .and_then(|track| track.key.content_id())
            .map_or_else(String::new, |id| id.as_str().to_owned())
            .into(),
    );
    window.set_track_info_path(
        track
            .map_or_else(String::new, |track| match &track.audio_source {
                AudioSource::LocalFile(path) => path.to_string_lossy().into_owned(),
                AudioSource::Federation { peer_id, .. } => format!("peer {peer_id}"),
            })
            .into(),
    );
    let (artwork, has_artwork) = load_artwork(track.and_then(|track| track.cover_uri.as_deref()));
    window.set_track_info_artwork(artwork);
    window.set_track_info_has_artwork(has_artwork);
}

fn track_quality_label(track: &Track) -> String {
    let mut fields = Vec::new();
    if let Some(bitrate) = track.audio_bitrate_kbps {
        fields.push(format!("{bitrate} kbps"));
    }
    if let Some(rate) = track.audio_sample_rate_hz {
        fields.push(format!("{:.1} kHz", f64::from(rate) / 1_000.0));
    }
    if let Some(depth) = track.audio_bit_depth {
        fields.push(format!("{depth} bit"));
    }
    if let Some(size) = track.file_size_bytes {
        let whole = size / 1_048_576;
        let decimal = size % 1_048_576 * 10 / 1_048_576;
        fields.push(format!("{whole}.{decimal} MiB"));
    }
    fields.join(" · ")
}

fn sync_media_session(state: &AppState) {
    MEDIA_SESSION.with_borrow_mut(|session| {
        let Some(session) = session.as_mut() else {
            return;
        };
        if let Some(track) = state.backend.queue.current().map(|item| &item.track) {
            session.update_metadata(
                &track.title,
                &track.artist,
                &track.release,
                track.duration_seconds,
            );
        }
        session.update_playback(
            state.backend.playback.status != PlaybackStatus::Stopped,
            state.backend.playback.status == PlaybackStatus::Paused,
            state.backend.playback.position_seconds,
        );
    });
}

fn federation_status(state: &AppState) -> (bool, String) {
    if !state.backend.settings.federation_enabled {
        return (false, "Federation disabled".into());
    }
    let activity = &state.backend.federation_activity;
    let operation = match activity.operation {
        FederationOperation::Idle => "Connecting to federation",
        FederationOperation::Search => "Searching federation",
        FederationOperation::Artist => "Loading artist",
        FederationOperation::Release => "Loading release",
    };
    if activity.pending {
        return (true, format!("{operation}…"));
    }
    if let Some(error) = &activity.error {
        return (false, format!("Network error · {error}"));
    }
    if let Some(stats) = &activity.stats {
        return (
            false,
            format!(
                "{} tracks · {} artists · {} peers · {}",
                stats.tracks,
                stats.artists,
                stats.peers_queried,
                search_duration_label(stats.duration_ms)
            ),
        );
    }
    (false, "Federation ready".into())
}

fn federation_node_ids(endpoint_id: &str, dht_node_id: &str) -> String {
    fn short(value: &str) -> &str {
        value.get(..12).unwrap_or(value)
    }
    match (endpoint_id.is_empty(), dht_node_id.is_empty()) {
        (false, false) => format!("ep {} · dht {}", short(endpoint_id), short(dht_node_id)),
        (false, true) => format!("ep {}", short(endpoint_id)),
        (true, false) => format!("dht {}", short(dht_node_id)),
        (true, true) => "n/a".into(),
    }
}

fn ready_library(state: &AppState) -> Option<&furumi_backend_api::LibrarySnapshot> {
    match &state.backend.library {
        RemoteData::Ready(library) => Some(library),
        _ => None,
    }
}

fn find_track<'a>(state: &'a AppState, key: &TrackKey) -> Option<&'a Track> {
    state
        .backend
        .queue
        .items()
        .iter()
        .map(|item| &item.track)
        .chain(state.backend.search.results.tracks.iter())
        .chain(
            state
                .backend
                .search
                .results
                .releases
                .iter()
                .flat_map(|release| release.tracks.iter()),
        )
        .chain(
            ready_library(state)
                .into_iter()
                .flat_map(|library| library.featured_releases.iter())
                .flat_map(|release| release.tracks.iter()),
        )
        .chain(
            ready_library(state)
                .into_iter()
                .flat_map(|library| library.playlists.iter())
                .flat_map(|playlist| playlist.tracks.iter()),
        )
        .find(|track| track.key.matches(key))
}

fn selected_artist(state: &AppState) -> Option<&Artist> {
    let Screen::Artist(key) = &state.frontend.screen else {
        return None;
    };
    state
        .backend
        .search
        .results
        .artists
        .iter()
        .chain(
            ready_library(state)
                .into_iter()
                .flat_map(|library| library.artists.iter()),
        )
        .find(|artist| &artist.key == key)
}

pub(super) fn selected_release(state: &AppState) -> Option<Release> {
    let Screen::Release(key, _) = &state.frontend.screen else {
        return None;
    };
    let mut candidates = ready_library(state)
        .into_iter()
        .flat_map(|library| library.featured_releases.iter())
        .chain(state.backend.search.results.releases.iter())
        .filter(|release| &release.key == key)
        .cloned();
    let mut selected = candidates.next()?;
    for candidate in candidates {
        merge_release_availability(&mut selected, candidate);
    }
    sort_release_tracks(&mut selected.tracks);
    Some(selected)
}

fn selected_playlist(state: &AppState) -> Option<&furumi_backend_api::PlaylistSnapshot> {
    let Screen::Playlist(id) = state.frontend.screen else {
        return None;
    };
    ready_library(state)?
        .playlists
        .iter()
        .find(|playlist| playlist.id == id)
}

pub(super) fn track_context(state: &AppState, context: &str) -> Vec<TrackKey> {
    let tracks: Vec<Track> = match context {
        "release" => selected_release(state).map_or_else(Vec::new, |release| release.tracks),
        "search" => state.backend.search.results.tracks.clone(),
        "recent" => ready_library(state)
            .into_iter()
            .flat_map(|library| library.recently_played.iter())
            .cloned()
            .collect(),
        "artist-featured" => selected_artist(state).map_or_else(Vec::new, |artist| {
            ready_library(state)
                .into_iter()
                .flat_map(|library| library.featured_releases.iter())
                .flat_map(|release| release.tracks.iter())
                .chain(state.backend.search.results.tracks.iter())
                .filter(|track| {
                    track
                        .featured_artists
                        .iter()
                        .any(|candidate| candidate.key == artist.key)
                })
                .cloned()
                .collect()
        }),
        "playlist" => selected_playlist(state)
            .into_iter()
            .flat_map(|playlist| playlist.tracks.iter())
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    tracks.into_iter().map(|track| track.key).collect()
}

pub(super) fn breadcrumb_screen(state: &AppState, target: &SharedString) -> Option<Screen> {
    match target.as_str() {
        "home" => Some(Screen::Home),
        "search" => Some(Screen::Search),
        "library" => Some(Screen::Library),
        value if value.starts_with("playlist:") => value
            .trim_start_matches("playlist:")
            .parse::<i64>()
            .ok()
            .map(Screen::Playlist),
        _ => find_artist_key(state, target).map(Screen::Artist),
    }
}

fn breadcrumbs(state: &AppState) -> Vec<BreadcrumbView> {
    let crumb = |label: String, target: String, trailing: bool| BreadcrumbView {
        label: if trailing {
            format!("{label} › ")
        } else {
            label
        }
        .into(),
        target: target.into(),
    };
    match &state.frontend.screen {
        Screen::Home => vec![crumb("Home".into(), String::new(), false)],
        Screen::Search => vec![
            crumb("Home".into(), "home".into(), true),
            crumb("Search".into(), String::new(), false),
        ],
        Screen::Library => vec![
            crumb("Home".into(), "home".into(), true),
            crumb("Library".into(), String::new(), false),
        ],
        Screen::Artist(_) => vec![
            crumb("Home".into(), "home".into(), true),
            crumb(
                selected_artist(state)
                    .map_or("Artist", |artist| artist.name.as_str())
                    .to_owned(),
                String::new(),
                false,
            ),
        ],
        Screen::Release(_, preferred) => {
            let release = selected_release(state);
            let release = release.as_ref();
            let artist = preferred
                .as_ref()
                .and_then(|key| {
                    release.and_then(|release| {
                        release.artists.iter().find(|artist| &artist.key == key)
                    })
                })
                .or_else(|| release.and_then(|release| release.artists.first()));
            let mut result = vec![crumb("Home".into(), "home".into(), true)];
            if let Some(artist) = artist {
                result.push(crumb(
                    artist.name.clone(),
                    format_artist_key(&artist.key),
                    true,
                ));
            }
            result.push(crumb(
                release
                    .map_or("Release", |release| release.title.as_str())
                    .to_owned(),
                String::new(),
                false,
            ));
            result
        }
        Screen::Playlist(_) => vec![
            crumb("Home".into(), "home".into(), true),
            crumb(
                selected_playlist(state)
                    .map_or("Playlist", |playlist| playlist.title.as_str())
                    .to_owned(),
                String::new(),
                false,
            ),
        ],
    }
}

pub(super) fn render_catalog(window: &AppWindow, state: &AppState) {
    let library = ready_library(state);
    let releases = library.map_or_else(Vec::new, |library| {
        library
            .featured_releases
            .iter()
            .map(release_to_view)
            .collect()
    });
    window.set_releases(model(releases));
    let artists = library.map_or_else(Vec::new, |library| {
        library.artists.iter().map(artist_to_view).collect()
    });
    window.set_artists(model(artists));

    let selected_artist = match &state.frontend.screen {
        Screen::Artist(key) => state
            .backend
            .search
            .results
            .artists
            .iter()
            .chain(
                library
                    .into_iter()
                    .flat_map(|library| library.artists.iter()),
            )
            .find(|artist| &artist.key == key),
        _ => None,
    };
    let artist_releases = selected_artist.map_or_else(Vec::new, |artist| {
        let releases = library
            .into_iter()
            .flat_map(|library| library.featured_releases.iter())
            .chain(state.backend.search.results.releases.iter())
            .filter(|release| {
                release
                    .artists
                    .iter()
                    .any(|candidate| candidate.key == artist.key)
            })
            .cloned();
        let mut releases = merge_artist_releases(releases);
        sort_releases_newest_first(&mut releases);
        releases
    });
    window.set_artist_albums(model(
        artist_releases
            .iter()
            .filter(|release| release.is_album())
            .map(release_to_view)
            .collect(),
    ));
    window.set_artist_other_releases(model(
        artist_releases
            .iter()
            .filter(|release| !release.is_album())
            .map(release_to_view)
            .collect(),
    ));
    window.set_artist_featured_tracks(model(selected_artist.map_or_else(Vec::new, |artist| {
        let tracks = library
            .into_iter()
            .flat_map(|library| {
                library
                    .featured_releases
                    .iter()
                    .flat_map(|release| release.tracks.iter())
            })
            .chain(state.backend.search.results.tracks.iter())
            .filter(|track| {
                track
                    .featured_artists
                    .iter()
                    .any(|candidate| candidate.key == artist.key)
            })
            .cloned()
            .collect::<Vec<_>>();
        tracks_to_views(&tracks, state)
    })));

    render_catalog_detail(window, state, selected_artist);

    let recent = library.map_or_else(Vec::new, |library| {
        tracks_to_views(&library.recently_played, state)
    });
    window.set_tracks(model(recent));
    let playlist = selected_playlist(state);
    window.set_playlist_title(
        playlist
            .map_or_else(String::new, |playlist| playlist.title.clone())
            .into(),
    );
    window.set_playlist_tracks(model(playlist.map_or_else(Vec::new, |playlist| {
        tracks_to_views(&playlist.tracks, state)
    })));
}

fn render_catalog_detail(window: &AppWindow, state: &AppState, selected_artist: Option<&Artist>) {
    let selected_release = selected_release(state);
    let selected_release = selected_release.as_ref();
    let detail_artwork = selected_artist
        .and_then(|artist| artist.artwork.uri.as_deref())
        .or_else(|| selected_release.and_then(|release| release.artwork.uri.as_deref()));
    let (detail_artwork, detail_has_artwork) = load_artwork(detail_artwork);
    window.set_detail_artwork(detail_artwork);
    window.set_detail_has_artwork(detail_has_artwork);
    window.set_detail_title(
        selected_artist
            .map(|artist| artist.name.as_str())
            .or_else(|| selected_release.map(|release| release.title.as_str()))
            .unwrap_or_default()
            .into(),
    );
    let preferred_artist = match &state.frontend.screen {
        Screen::Release(_, preferred) => preferred.as_ref(),
        _ => None,
    };
    window.set_detail_subtitle(
        selected_artist
            .map(|artist| {
                format!(
                    "{} releases · {} tracks",
                    artist.release_count, artist.track_count
                )
            })
            .unwrap_or_default()
            .into(),
    );
    window.set_detail_release_type(
        selected_release
            .map(|release| release_type_label(&release.release_type))
            .unwrap_or_default()
            .into(),
    );
    let (main_artists, contributors) = selected_release.map_or_else(
        || (Vec::new(), Vec::new()),
        |release| release_artist_credits(release, preferred_artist),
    );
    window.set_detail_main_artists(model(
        main_artists
            .iter()
            .enumerate()
            .map(|(index, artist)| artist_link_view(artist, index))
            .collect(),
    ));
    window.set_detail_contributor_lines(model(contributor_lines(&contributors, 420.0)));
    window.set_detail_tracks(model(selected_release.map_or_else(Vec::new, |release| {
        release_tracks_to_views(&release.tracks, state)
    })));
    window.set_detail_federation_state(selected_release.map_or(0, release_federation_state));
}

pub(super) fn render_queue(window: &AppWindow, state: &AppState) {
    let queue = state
        .backend
        .queue
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let (artwork, has_artwork) = load_artwork(item.track.cover_uri.as_deref());
            QueueView {
                key: item.id.get().to_string().into(),
                title: item.track.title.clone().into(),
                artist: item.track.artist.clone().into(),
                artist_key: item
                    .track
                    .artists
                    .first()
                    .map(|artist| format_artist_key(&artist.key))
                    .unwrap_or_default()
                    .into(),
                release: item.track.release.clone().into(),
                release_key: format_release_key(&item.track.release_id).into(),
                active: state.backend.queue.current_index() == Some(index),
                artwork,
                has_artwork,
            }
        })
        .collect();
    window.set_queue_items(model(queue));
}

pub(super) fn render_playback(window: &AppWindow, state: &AppState) {
    sync_media_session(state);
    let player = state.player_projection();
    window.set_has_track(player.has_track);
    window.set_playing(player.playing);
    window.set_elapsed(player.elapsed.into());
    window.set_duration(player.duration.into());
    window.set_progress(player.progress);
    window.set_volume(player.volume);
    window.set_shuffle_enabled(player.shuffle);
    window.set_repeat_mode(match player.repeat {
        PlaybackRepeat::Off => 0,
        PlaybackRepeat::One => 1,
        PlaybackRepeat::All => 2,
    });
}

pub(super) fn render_current_track(window: &AppWindow, state: &AppState) {
    let current = state.backend.queue.current().map(|item| &item.track);
    window.set_current_title(
        current
            .map(|track| track.title.clone())
            .unwrap_or_default()
            .into(),
    );
    window.set_current_artists(model(current.map_or_else(Vec::new, track_artist_links)));
    window.set_current_metadata(current.map(track_metadata_label).unwrap_or_default().into());
    window.set_current_release(
        current
            .map(|track| track.release.clone())
            .unwrap_or_default()
            .into(),
    );
    window.set_current_release_key(
        current
            .map(|track| format_release_key(&track.release_id))
            .unwrap_or_default()
            .into(),
    );
    let (current_artwork, current_has_artwork) =
        load_artwork(current.and_then(|track| track.cover_uri.as_deref()));
    window.set_current_artwork(current_artwork);
    window.set_current_has_artwork(current_has_artwork);
}

pub(super) fn render_search(window: &AppWindow, state: &AppState) {
    let search = &state.backend.search;
    window.set_search_artists(model(
        search.results.artists.iter().map(artist_to_view).collect(),
    ));
    window.set_search_releases(model(
        search
            .results
            .releases
            .iter()
            .map(release_to_view)
            .collect(),
    ));
    window.set_search_results(model(tracks_to_views(&search.results.tracks, state)));
}

fn search_duration_label(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format!(
            "{}.{:01} s",
            milliseconds / 1_000,
            milliseconds % 1_000 / 100
        )
    }
}

fn tracks_to_views(tracks: &[Track], state: &AppState) -> Vec<TrackView> {
    tracks_to_views_with_numbers(tracks, state, false)
}

fn release_tracks_to_views(tracks: &[Track], state: &AppState) -> Vec<TrackView> {
    tracks_to_views_with_numbers(tracks, state, true)
}

fn tracks_to_views_with_numbers(
    tracks: &[Track],
    state: &AppState,
    use_metadata_number: bool,
) -> Vec<TrackView> {
    let current = state.backend.queue.current().map(|item| &item.track.key);
    tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let (artwork, has_artwork) = load_artwork(track.cover_uri.as_deref());
            let artist_links = track_artist_links(track);
            TrackView {
                key: format_track_key(&track.key).into(),
                number: track_number_label(track, index, use_metadata_number).into(),
                title: track.title.clone().into(),
                artists: model(artist_links),
                artists_label: track_artist_label(track).into(),
                artist_line_break: track_artist_line_break(track),
                release: track.release.clone().into(),
                release_key: format_release_key(&track.release_id).into(),
                duration: duration_label(track.duration_seconds).into(),
                active: current.is_some_and(|key| key.matches(&track.key)),
                artwork,
                has_artwork,
                federated: matches!(track.audio_source, AudioSource::Federation { .. }),
                liked: track.liked,
            }
        })
        .collect()
}

fn track_number_label(track: &Track, index: usize, use_metadata_number: bool) -> String {
    if use_metadata_number {
        track
            .track_number
            .map_or_else(|| "—".into(), |number| number.to_string())
    } else {
        (index + 1).to_string()
    }
}

fn track_artist_links(track: &Track) -> Vec<ArtistLinkView> {
    let mut links = Vec::with_capacity(track.artists.len() + track.featured_artists.len());
    for (index, artist) in track.artists.iter().enumerate() {
        links.push(ArtistLinkView {
            key: format_artist_key(&artist.key).into(),
            name: artist.name.clone().into(),
            prefix: if index == 0 { "" } else { ", " }.into(),
        });
    }
    for (index, artist) in track.featured_artists.iter().enumerate() {
        links.push(ArtistLinkView {
            key: format_artist_key(&artist.key).into(),
            name: artist.name.clone().into(),
            prefix: if index == 0 { " feat. " } else { ", " }.into(),
        });
    }
    links
}

fn track_artist_label(track: &Track) -> String {
    let mut label = track
        .artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !track.featured_artists.is_empty() {
        if !label.is_empty() {
            label.push_str(" feat. ");
        }
        label.push_str(
            &track
                .featured_artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if label.is_empty() {
        track.artist.clone()
    } else {
        label
    }
}

fn track_artist_line_break(track: &Track) -> i32 {
    let segments = track
        .artists
        .iter()
        .map(|artist| artist.name.chars().count() + 2)
        .chain(
            track
                .featured_artists
                .iter()
                .map(|artist| artist.name.chars().count() + 2),
        )
        .collect::<Vec<_>>();
    if segments.len() < 2 {
        return 0;
    }

    let total = segments.iter().sum::<usize>();
    let mut prefix = 0_usize;
    let mut best_index = 1_usize;
    let mut best_delta = usize::MAX;
    for (index, width) in segments.iter().take(segments.len() - 1).enumerate() {
        prefix = prefix.saturating_add(*width);
        let delta = prefix.abs_diff(total.saturating_sub(prefix));
        if delta < best_delta {
            best_delta = delta;
            best_index = index + 1;
        }
    }
    i32::try_from(best_index).unwrap_or(i32::MAX)
}

fn track_metadata_label(track: &Track) -> String {
    let mut parts = Vec::with_capacity(2);
    if let Some(format) = track
        .audio_format
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format.to_uppercase());
    }
    if let Some(bitrate) = track.audio_bitrate_kbps {
        parts.push(format!("{bitrate} kbps"));
    }
    parts.join(" · ")
}

fn artist_link_view(artist: &ArtistRef, index: usize) -> ArtistLinkView {
    ArtistLinkView {
        key: format_artist_key(&artist.key).into(),
        name: artist.name.clone().into(),
        prefix: if index == 0 { "" } else { ", " }.into(),
    }
}

pub(super) fn contributor_lines(artists: &[ArtistRef], width: f32) -> Vec<ArtistLinkLineView> {
    let width = width.max(220.0);
    let mut lines = Vec::<Vec<ArtistLinkView>>::new();
    let mut current = Vec::new();
    let mut current_width = 0.0_f32;

    for (index, artist) in artists.iter().enumerate() {
        // Eleven-pixel system text averages a little under seven logical
        // pixels per glyph. The conservative estimate keeps every artist on
        // one line while still packing short names naturally.
        let has_trailing_comma = index + 1 < artists.len();
        let glyph_count = u16::try_from(artist.name.chars().count()).unwrap_or(u16::MAX);
        let name_width = f32::from(glyph_count) * 7.0 + if has_trailing_comma { 3.0 } else { 0.0 };
        let separator_width = if current.is_empty() { 0.0 } else { 9.0 };
        if !current.is_empty() && current_width + separator_width + name_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0.0;
        }
        let prefix = if current.is_empty() { "" } else { " " };
        current.push(ArtistLinkView {
            key: format_artist_key(&artist.key).into(),
            name: if has_trailing_comma {
                format!("{},", artist.name)
            } else {
                artist.name.clone()
            }
            .into(),
            prefix: prefix.into(),
        });
        current_width += (if prefix.is_empty() { 0.0 } else { 9.0 }) + name_width;

        if index + 1 == artists.len() && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
    }

    lines
        .into_iter()
        .map(|artists| ArtistLinkLineView {
            artists: model(artists),
        })
        .collect()
}

fn artist_to_view(artist: &Artist) -> ArtistView {
    let (artwork, has_artwork) = load_artwork(artist.artwork.uri.as_deref());
    ArtistView {
        key: format_artist_key(&artist.key).into(),
        name: artist.name.clone().into(),
        details: format!(
            "{} releases · {} tracks",
            artist.release_count, artist.track_count
        )
        .into(),
        artwork,
        has_artwork,
        federated: matches!(artist.source, CatalogSource::Federation { .. }),
    }
}

fn release_to_view(release: &Release) -> ReleaseView {
    let (artwork, has_artwork) = load_artwork(release.artwork.uri.as_deref());
    ReleaseView {
        key: format_release_key(&release.key).into(),
        title: release.title.clone().into(),
        artist: release.artist_line().into(),
        artist_key: release
            .artists
            .first()
            .map(|artist| format_artist_key(&artist.key))
            .unwrap_or_default()
            .into(),
        year: release
            .year
            .map_or_else(String::new, |year| year.to_string())
            .into(),
        release_type: release_type_label(&release.release_type).into(),
        artwork,
        has_artwork,
        federation_state: release_federation_state(release),
    }
}

fn release_federation_state(release: &Release) -> i32 {
    if release.tracks.is_empty() {
        return i32::from(matches!(release.source, CatalogSource::Federation { .. }));
    }
    let local = release
        .tracks
        .iter()
        .filter(|track| matches!(track.audio_source, AudioSource::LocalFile(_)))
        .count();
    match local {
        0 => 1,
        count if count == release.tracks.len() => 0,
        _ => 2,
    }
}

fn release_type_label(release_type: &str) -> String {
    let release_type = release_type.trim();
    if release_type.is_empty() {
        return "Release".into();
    }
    if release_type.eq_ignore_ascii_case("ep") {
        return "EP".into();
    }
    let mut characters = release_type.chars();
    let Some(first) = characters.next() else {
        return "Release".into();
    };
    first.to_uppercase().chain(characters).collect()
}

pub(super) fn release_artist_credits(
    release: &Release,
    preferred: Option<&ArtistKey>,
) -> (Vec<ArtistRef>, Vec<ArtistRef>) {
    let all_artists = || {
        release
            .artists
            .iter()
            .chain(release.featured_artists.iter())
            .chain(
                release
                    .tracks
                    .iter()
                    .flat_map(|track| track.artists.iter().chain(track.featured_artists.iter())),
            )
    };
    let primary = preferred
        .and_then(|preferred| all_artists().find(|artist| &artist.key == preferred))
        .or_else(|| release.artists.first())
        .or_else(|| {
            release
                .tracks
                .iter()
                .find_map(|track| track.artists.first())
        })
        .cloned();
    let mut known = HashSet::new();
    let mut main = Vec::new();
    if let Some(primary) = primary {
        known.insert(artist_name_key(&primary.name));
        main.push(primary);
    }
    let contributors = all_artists()
        .filter(|artist| known.insert(artist_name_key(&artist.name)))
        .cloned()
        .collect();
    (main, contributors)
}

fn artist_name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

fn merge_artist_releases(releases: impl IntoIterator<Item = Release>) -> Vec<Release> {
    let mut merged = Vec::<Release>::new();
    let mut slots = HashMap::<(String, String, Option<i32>), usize>::new();
    for release in releases {
        let identity = (
            release.title.trim().to_lowercase(),
            release.release_type.trim().to_lowercase(),
            release.year,
        );
        if let Some(index) = slots.get(&identity).copied() {
            merge_release_availability(&mut merged[index], release);
        } else {
            slots.insert(identity, merged.len());
            merged.push(release);
        }
    }
    merged
}

fn sort_releases_newest_first(releases: &mut [Release]) {
    releases.sort_by(|left, right| {
        right
            .year
            .unwrap_or(i32::MIN)
            .cmp(&left.year.unwrap_or(i32::MIN))
            .then_with(|| left.release_type.cmp(&right.release_type))
            .then_with(|| left.title.cmp(&right.title))
    });
}

fn merge_release_availability(target: &mut Release, incoming: Release) {
    if matches!(target.source, CatalogSource::Federation { .. })
        && matches!(incoming.source, CatalogSource::Local)
    {
        let previous = std::mem::replace(target, incoming);
        merge_release_availability(target, previous);
        return;
    }
    if target.artwork.uri.is_none() && incoming.artwork.uri.is_some() {
        target.artwork = incoming.artwork;
    }
    merge_credit_refs(&mut target.artists, incoming.artists);
    merge_credit_refs(&mut target.featured_artists, incoming.featured_artists);
    for track in incoming.tracks {
        if let Some(existing) = target
            .tracks
            .iter_mut()
            .find(|existing| tracks_match_within_release(existing, &track))
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
    sort_release_tracks(&mut target.tracks);
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

fn tracks_match_within_release(left: &Track, right: &Track) -> bool {
    if left.key.matches(&right.key) {
        return true;
    }
    if let (Some(left_number), Some(right_number)) = (left.track_number, right.track_number)
        && left_number == right_number
        && left.disc_number.unwrap_or(1) == right.disc_number.unwrap_or(1)
    {
        return true;
    }
    left.title.trim().eq_ignore_ascii_case(right.title.trim())
}

fn merge_credit_refs(target: &mut Vec<ArtistRef>, incoming: Vec<ArtistRef>) {
    let mut known = target
        .iter()
        .map(|artist| artist_name_key(&artist.name))
        .collect::<HashSet<_>>();
    target.extend(
        incoming
            .into_iter()
            .filter(|artist| known.insert(artist_name_key(&artist.name))),
    );
}

fn load_artwork(uri: Option<&str>) -> (Image, bool) {
    let Some(uri) = uri.filter(|uri| !uri.trim().is_empty()) else {
        return (Image::default(), false);
    };
    let path = PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri));
    let cached = ARTWORK_CACHE.with(|cache| cache.borrow().get(&path).cloned());
    let image = cached.unwrap_or_else(|| {
        let image = decode_artwork(&path).ok();
        ARTWORK_CACHE.with(|cache| {
            cache.borrow_mut().insert(path.clone(), image.clone());
        });
        image
    });
    image.map_or_else(|| (Image::default(), false), |image| (image, true))
}

fn decode_artwork(path: &Path) -> Result<Image, String> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("webp"))
    {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        decode_webp(&bytes)
    } else {
        Image::load_from_path(path).map_err(|error| error.to_string())
    }
}

fn decode_webp(bytes: &[u8]) -> Result<Image, String> {
    let rgba = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
        .map_err(|error| error.to_string())?
        .into_rgba8();
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    Ok(Image::from_rgba8(buffer))
}

fn format_artist_key(key: &ArtistKey) -> String {
    match key {
        ArtistKey::Local(id) => format!("local-artist:{}", id.get()),
        ArtistKey::Federation { peer_id, id } => format!("fed-artist:{peer_id}:{id}"),
    }
}

fn format_release_key(key: &ReleaseKey) -> String {
    match key {
        ReleaseKey::Local(id) => format!("local-release:{}", id.get()),
        ReleaseKey::Federation { peer_id, id } => format!("fed-release:{peer_id}:{id}"),
    }
}

pub(super) fn find_artist_key(state: &AppState, value: &SharedString) -> Option<ArtistKey> {
    let local = match &state.backend.library {
        RemoteData::Ready(library) => library.artists.as_slice(),
        _ => &[],
    };
    local
        .iter()
        .chain(state.backend.search.results.artists.iter())
        .find(|artist| format_artist_key(&artist.key) == value.as_str())
        .map(|artist| artist.key.clone())
}

pub(super) fn find_release_key(state: &AppState, value: &SharedString) -> Option<ReleaseKey> {
    let local = match &state.backend.library {
        RemoteData::Ready(library) => library.featured_releases.as_slice(),
        _ => &[],
    };
    local
        .iter()
        .chain(state.backend.search.results.releases.iter())
        .find(|release| format_release_key(&release.key) == value.as_str())
        .map(|release| release.key.clone())
}

fn format_track_key(key: &TrackKey) -> String {
    if let Some(local) = key.local_id() {
        format!("local:{}", local.get())
    } else if let Some((peer, item)) = key.federation_id() {
        format!(
            "fed-track:{peer}:{item}:{}",
            key.content_id().map_or("", ContentId::as_str)
        )
    } else if let Some(content) = key.content_id() {
        content.as_str().to_owned()
    } else {
        String::new()
    }
}

pub(super) fn parse_track_key(value: &SharedString) -> Option<TrackKey> {
    if let Some(local) = value.strip_prefix("local:") {
        return local
            .parse::<i64>()
            .ok()
            .map(|id| TrackKey::local(LocalTrackId::new(id)));
    }
    if let Some(federated) = value.strip_prefix("fed-track:") {
        let mut parts = federated.splitn(3, ':');
        let peer = parts.next()?.to_owned();
        let item = parts.next()?.to_owned();
        let content = parts
            .next()
            .filter(|value| !value.is_empty())
            .and_then(|value| ContentId::parse(value).ok());
        return Some(TrackKey::federation(peer, item, content));
    }
    furumi_domain::ContentId::parse(value.to_string())
        .ok()
        .map(TrackKey::remote)
}

pub(super) fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

#[cfg(test)]
mod tests {
    use image::ImageEncoder as _;

    use super::*;

    #[test]
    fn webp_is_converted_to_a_slint_image() {
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .write_image(&[20, 40, 60, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();

        let decoded = decode_webp(&encoded).unwrap();

        assert_eq!(decoded.size().width, 1);
        assert_eq!(decoded.size().height, 1);
    }

    #[test]
    fn release_credits_include_artists_found_only_on_tracks() {
        let pasha = ArtistRef {
            key: ArtistKey::local(furumi_domain::ArtistId::new(1)),
            name: "Паша Техник".into(),
        };
        let kunteynir = ArtistRef {
            key: ArtistKey::local(furumi_domain::ArtistId::new(2)),
            name: "KUNTEYNIR".into(),
        };
        let metox = ArtistRef {
            key: ArtistKey::local(furumi_domain::ArtistId::new(3)),
            name: "Metox".into(),
        };
        let release = Release {
            key: ReleaseKey::local(furumi_domain::ReleaseId::new(7)),
            source: CatalogSource::Local,
            title: "Порядочный".into(),
            artists: vec![pasha.clone()],
            featured_artists: Vec::new(),
            release_type: "album".into(),
            year: None,
            artwork: furumi_domain::Artwork::default(),
            tracks: vec![Track {
                key: TrackKey::local(LocalTrackId::new(10)),
                title: "12 шагов до рая".into(),
                artist: "KUNTEYNIR feat. Metox".into(),
                artists: vec![kunteynir],
                featured_artists: vec![metox],
                release: "Порядочный".into(),
                release_id: ReleaseKey::local(furumi_domain::ReleaseId::new(7)),
                duration_seconds: 219.0,
                track_number: Some(1),
                disc_number: Some(1),
                cover_uri: None,
                audio_format: Some("mp3".into()),
                audio_bitrate_kbps: Some(320),
                audio_sample_rate_hz: Some(44_100),
                audio_bit_depth: None,
                file_size_bytes: None,
                liked: false,
                audio_source: AudioSource::LocalFile(PathBuf::from("track.mp3")),
            }],
        };

        let (main, contributors) = release_artist_credits(&release, Some(&pasha.key));

        assert_eq!(
            main.iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Паша Техник"]
        );
        assert_eq!(
            contributors
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>(),
            vec!["KUNTEYNIR", "Metox"]
        );
    }

    #[test]
    fn artist_release_tile_reports_mixed_local_and_federated_tracks() {
        let artist = ArtistRef {
            key: ArtistKey::local(furumi_domain::ArtistId::new(1)),
            name: "Artist".into(),
        };
        let local = Release {
            key: ReleaseKey::local(furumi_domain::ReleaseId::new(7)),
            source: CatalogSource::Local,
            title: "Album".into(),
            artists: vec![artist.clone()],
            featured_artists: Vec::new(),
            release_type: "album".into(),
            year: Some(2026),
            artwork: furumi_domain::Artwork::default(),
            tracks: vec![Track {
                key: TrackKey::local(LocalTrackId::new(10)),
                title: "Local".into(),
                artist: "Artist".into(),
                artists: vec![artist.clone()],
                featured_artists: Vec::new(),
                release: "Album".into(),
                release_id: ReleaseKey::local(furumi_domain::ReleaseId::new(7)),
                duration_seconds: 100.0,
                track_number: Some(1),
                disc_number: Some(1),
                cover_uri: None,
                audio_format: Some("flac".into()),
                audio_bitrate_kbps: None,
                audio_sample_rate_hz: None,
                audio_bit_depth: None,
                file_size_bytes: None,
                liked: false,
                audio_source: AudioSource::LocalFile(PathBuf::from("local.flac")),
            }],
        };
        let content_id = ContentId::parse(format!("b3:{}", "a".repeat(64))).unwrap();
        let mut remote_track = local.tracks[0].clone();
        remote_track.key = TrackKey::federation(
            "peer".into(),
            "remote-track".into(),
            Some(content_id.clone()),
        );
        remote_track.title = "Remote".into();
        remote_track.track_number = Some(2);
        remote_track.audio_source = AudioSource::Federation {
            peer_id: "peer".into(),
            content_id,
        };
        let remote = Release {
            key: ReleaseKey::Federation {
                peer_id: "peer".into(),
                id: "album".into(),
            },
            source: CatalogSource::Federation {
                peer_id: "peer".into(),
            },
            title: "Album".into(),
            artists: vec![artist],
            featured_artists: Vec::new(),
            release_type: "album".into(),
            year: Some(2026),
            artwork: furumi_domain::Artwork::default(),
            tracks: vec![remote_track],
        };

        let merged = merge_artist_releases([local, remote]);

        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].key, ReleaseKey::Local(_)));
        assert_eq!(release_federation_state(&merged[0]), 2);
    }

    #[test]
    fn artist_releases_are_sorted_newest_first_with_unknown_years_last() {
        let artist = ArtistRef {
            key: ArtistKey::local(furumi_domain::ArtistId::new(1)),
            name: "Artist".into(),
        };
        let release = |title: &str, year| Release {
            key: ReleaseKey::Federation {
                peer_id: "peer".into(),
                id: title.into(),
            },
            source: CatalogSource::Federation {
                peer_id: "peer".into(),
            },
            title: title.into(),
            artists: vec![artist.clone()],
            featured_artists: Vec::new(),
            release_type: "single".into(),
            year,
            artwork: furumi_domain::Artwork::default(),
            tracks: Vec::new(),
        };
        let mut releases = vec![
            release("Old", Some(1999)),
            release("Unknown", None),
            release("New", Some(2026)),
        ];

        sort_releases_newest_first(&mut releases);

        assert_eq!(
            releases
                .iter()
                .map(|release| release.title.as_str())
                .collect::<Vec<_>>(),
            vec!["New", "Old", "Unknown"]
        );
    }

    #[test]
    fn release_rows_use_metadata_track_numbers_instead_of_visual_positions() {
        let mut track = Track {
            key: TrackKey::local(LocalTrackId::new(1)),
            title: "Track".into(),
            artist: "Artist".into(),
            artists: Vec::new(),
            featured_artists: Vec::new(),
            release: "Release".into(),
            release_id: ReleaseKey::local(furumi_domain::ReleaseId::new(1)),
            duration_seconds: 1.0,
            track_number: Some(9),
            disc_number: Some(1),
            cover_uri: None,
            audio_format: None,
            audio_bitrate_kbps: None,
            audio_sample_rate_hz: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            liked: false,
            audio_source: AudioSource::LocalFile(PathBuf::from("track.flac")),
        };

        assert_eq!(track_number_label(&track, 0, true), "9");
        assert_eq!(track_number_label(&track, 0, false), "1");
        track.track_number = None;
        assert_eq!(track_number_label(&track, 8, true), "—");
    }

    #[test]
    fn full_artist_label_keeps_main_and_featured_artists() {
        let mut track = Track {
            key: TrackKey::local(LocalTrackId::new(1)),
            title: "Track".into(),
            artist: String::new(),
            artists: vec![ArtistRef {
                key: ArtistKey::local(furumi_domain::ArtistId::new(1)),
                name: "Main".into(),
            }],
            featured_artists: vec![
                ArtistRef {
                    key: ArtistKey::local(furumi_domain::ArtistId::new(2)),
                    name: "Guest A".into(),
                },
                ArtistRef {
                    key: ArtistKey::local(furumi_domain::ArtistId::new(3)),
                    name: "Guest B".into(),
                },
            ],
            release: "Release".into(),
            release_id: ReleaseKey::local(furumi_domain::ReleaseId::new(1)),
            duration_seconds: 1.0,
            track_number: None,
            disc_number: None,
            cover_uri: None,
            audio_format: None,
            audio_bitrate_kbps: None,
            audio_sample_rate_hz: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            liked: false,
            audio_source: AudioSource::LocalFile(PathBuf::from("track.flac")),
        };

        assert_eq!(track_artist_label(&track), "Main feat. Guest A, Guest B");
        track.artists.clear();
        assert_eq!(track_artist_label(&track), "Guest A, Guest B");
    }

    #[test]
    fn contributors_wrap_into_compact_comma_separated_lines() {
        use slint::Model as _;

        let artists = ["Alpha Alpha", "Beta Beta", "Gamma Gamma"]
            .into_iter()
            .zip(1_i64..)
            .map(|(name, id)| ArtistRef {
                key: ArtistKey::local(furumi_domain::ArtistId::new(id)),
                name: name.into(),
            })
            .collect::<Vec<_>>();

        let lines = contributor_lines(&artists, 80.0);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].artists.row_count(), 2);
        assert_eq!(lines[1].artists.row_count(), 1);
        assert_eq!(lines[0].artists.row_data(0).unwrap().prefix, "");
        assert_eq!(lines[0].artists.row_data(0).unwrap().name, "Alpha Alpha,");
        assert_eq!(lines[0].artists.row_data(1).unwrap().prefix, " ");
        assert_eq!(lines[1].artists.row_data(0).unwrap().prefix, "");
    }
}
