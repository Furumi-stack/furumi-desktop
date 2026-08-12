use super::*;

fn merge_test_track(key: TrackKey, audio_source: AudioSource) -> Track {
    Track {
        key,
        title: "Track".into(),
        artist: "Artist".into(),
        artists: vec![ArtistRef {
            key: ArtistKey::local(ArtistId::new(1)),
            name: "Artist".into(),
        }],
        featured_artists: Vec::new(),
        release: "Album".into(),
        release_id: ReleaseKey::local(ReleaseId::new(1)),
        duration_seconds: 180.0,
        track_number: Some(1),
        disc_number: Some(1),
        cover_uri: None,
        audio_format: Some("flac".into()),
        audio_bitrate_kbps: None,
        audio_sample_rate_hz: None,
        audio_bit_depth: None,
        file_size_bytes: None,
        liked: false,
        audio_source,
    }
}

#[test]
fn merging_search_results_deduplicates_source_keys() {
    let artist = Artist {
        key: ArtistKey::Federation {
            peer_id: "peer".into(),
            id: "artist".into(),
        },
        source: CatalogSource::Federation {
            peer_id: "peer".into(),
        },
        name: "Artist".into(),
        artwork: Artwork::default(),
        release_count: 0,
        track_count: 0,
    };
    let mut target = SearchResults {
        artists: vec![artist.clone()],
        ..SearchResults::default()
    };
    merge_search_results(
        &mut target,
        SearchResults {
            artists: vec![artist],
            ..SearchResults::default()
        },
    );
    assert_eq!(target.artists.len(), 1);
}

#[test]
fn newly_received_release_tracks_are_sorted_by_disc_and_track_number() {
    let mut fifth = merge_test_track(
        TrackKey::federation("peer".into(), "five".into(), None),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id: ContentId::parse(format!("b3:{}", "5".repeat(64))).unwrap(),
        },
    );
    fifth.title = "Los".into();
    fifth.track_number = Some(5);
    let mut first = merge_test_track(
        TrackKey::federation("peer".into(), "one".into(), None),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id: ContentId::parse(format!("b3:{}", "1".repeat(64))).unwrap(),
        },
    );
    first.title = "Reise, Reise".into();

    let mut target = SearchResults::default();
    merge_search_results(
        &mut target,
        SearchResults {
            releases: vec![Release {
                key: ReleaseKey::Federation {
                    peer_id: "peer".into(),
                    id: "reise-reise".into(),
                },
                source: CatalogSource::Federation {
                    peer_id: "peer".into(),
                },
                title: "Reise, Reise".into(),
                artists: first.artists.clone(),
                featured_artists: Vec::new(),
                release_type: "album".into(),
                year: Some(2004),
                artwork: Artwork::default(),
                tracks: vec![fifth, first],
            }],
            ..SearchResults::default()
        },
    );

    assert_eq!(
        target.releases[0]
            .tracks
            .iter()
            .map(|track| track.track_number)
            .collect::<Vec<_>>(),
        vec![Some(1), Some(5)]
    );
}

#[test]
fn selected_track_key_survives_context_filtering_and_reordering() {
    let selected = TrackKey::local(LocalTrackId::new(2));
    let resolved = vec![
        merge_test_track(
            TrackKey::local(LocalTrackId::new(3)),
            AudioSource::LocalFile("three.flac".into()),
        ),
        merge_test_track(selected.clone(), AudioSource::LocalFile("two.flac".into())),
    ];

    assert_eq!(selected_track_position(&resolved, &selected), 1);
}

#[test]
fn queue_artwork_requests_are_grouped_per_peer_and_release() {
    let first_content = ContentId::parse(format!("b3:{}", "1".repeat(64))).unwrap();
    let second_content = ContentId::parse(format!("b3:{}", "2".repeat(64))).unwrap();
    let mut first = merge_test_track(
        TrackKey::federation("peer".into(), "one".into(), Some(first_content.clone())),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id: first_content,
        },
    );
    let mut second = merge_test_track(
        TrackKey::federation("peer".into(), "two".into(), Some(second_content.clone())),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id: second_content,
        },
    );
    first.title = "One".into();
    second.title = "Two".into();

    assert_eq!(
        Actor::queue_artwork_request_id(&first),
        Actor::queue_artwork_request_id(&second)
    );
    second.release = "Another album".into();
    assert_ne!(
        Actor::queue_artwork_request_id(&first),
        Actor::queue_artwork_request_id(&second)
    );
}

#[test]
fn federated_release_enrichment_never_replaces_a_local_track() {
    let release_key = ReleaseKey::local(ReleaseId::new(1));
    let artist = ArtistRef {
        key: ArtistKey::local(ArtistId::new(1)),
        name: "Artist".into(),
    };
    let local_track = merge_test_track(
        TrackKey::local(LocalTrackId::new(1)),
        AudioSource::LocalFile("track.flac".into()),
    );
    let mut target = SearchResults {
        releases: vec![Release {
            key: release_key.clone(),
            source: CatalogSource::Local,
            title: "Album".into(),
            artists: vec![artist.clone()],
            featured_artists: Vec::new(),
            release_type: "album".into(),
            year: Some(2026),
            artwork: Artwork::default(),
            tracks: vec![local_track],
        }],
        ..SearchResults::default()
    };
    let content_id = ContentId::parse(format!("b3:{}", "a".repeat(64))).unwrap();
    let remote_track = merge_test_track(
        TrackKey::federation(
            "peer".into(),
            "remote-item".into(),
            Some(content_id.clone()),
        ),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id,
        },
    );

    merge_search_results(
        &mut target,
        SearchResults {
            releases: vec![Release {
                key: release_key,
                source: CatalogSource::Federation {
                    peer_id: "peer".into(),
                },
                title: "Album".into(),
                artists: vec![artist],
                featured_artists: Vec::new(),
                release_type: "album".into(),
                year: Some(2026),
                artwork: Artwork::default(),
                tracks: vec![remote_track],
            }],
            ..SearchResults::default()
        },
    );

    let tracks = &target.releases[0].tracks;
    assert_eq!(tracks.len(), 1);
    assert!(matches!(tracks[0].audio_source, AudioSource::LocalFile(_)));
}

#[test]
fn incomplete_federated_metadata_cannot_shadow_local_release_metadata() {
    let release_key = ReleaseKey::local(ReleaseId::new(1));
    let artist = ArtistRef {
        key: ArtistKey::local(ArtistId::new(1)),
        name: "Artist".into(),
    };
    let mut local_track = merge_test_track(
        TrackKey::local(LocalTrackId::new(1)),
        AudioSource::LocalFile("track.flac".into()),
    );
    local_track.cover_uri = Some("local-cover.png".into());
    local_track.audio_bitrate_kbps = Some(1_411);
    let mut target = Release {
        key: release_key.clone(),
        source: CatalogSource::Local,
        title: "Complete local album title".into(),
        artists: vec![artist.clone()],
        featured_artists: Vec::new(),
        release_type: "album".into(),
        year: Some(2026),
        artwork: Artwork {
            uri: Some("local-cover.png".into()),
        },
        tracks: vec![local_track],
    };
    let content_id = ContentId::parse(format!("b3:{}", "b".repeat(64))).unwrap();
    let mut incomplete_remote = merge_test_track(
        TrackKey::federation("peer".into(), "item".into(), Some(content_id.clone())),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id,
        },
    );
    incomplete_remote.title = String::new();
    incomplete_remote.artist = String::new();
    incomplete_remote.artists.clear();
    incomplete_remote.audio_format = None;

    merge_release_preserving_local(
        &mut target,
        Release {
            key: release_key,
            source: CatalogSource::Federation {
                peer_id: "peer".into(),
            },
            title: String::new(),
            artists: vec![artist],
            featured_artists: Vec::new(),
            release_type: String::new(),
            year: None,
            artwork: Artwork::default(),
            tracks: vec![incomplete_remote],
        },
    );

    assert!(matches!(target.source, CatalogSource::Local));
    assert_eq!(target.title, "Complete local album title");
    assert_eq!(target.tracks.len(), 1);
    assert_eq!(target.tracks[0].title, "Track");
    assert_eq!(target.tracks[0].audio_format.as_deref(), Some("flac"));
    assert_eq!(target.tracks[0].audio_bitrate_kbps, Some(1_411));
    assert!(matches!(
        target.tracks[0].audio_source,
        AudioSource::LocalFile(_)
    ));
}

#[test]
fn queue_resolver_keeps_tracks_nested_in_a_federated_release() {
    let content_id = ContentId::parse(format!("b3:{}", "c".repeat(64))).unwrap();
    let key = TrackKey::federation(
        "peer".into(),
        "remote-track".into(),
        Some(content_id.clone()),
    );
    let track = merge_test_track(
        key.clone(),
        AudioSource::Federation {
            peer_id: "peer".into(),
            content_id,
        },
    );
    let search = SearchResults {
        releases: vec![Release {
            key: ReleaseKey::Federation {
                peer_id: "peer".into(),
                id: "album".into(),
            },
            source: CatalogSource::Federation {
                peer_id: "peer".into(),
            },
            title: "Album".into(),
            artists: track.artists.clone(),
            featured_artists: Vec::new(),
            release_type: "album".into(),
            year: Some(2026),
            artwork: Artwork::default(),
            tracks: vec![track],
        }],
        ..SearchResults::default()
    };

    let library = LibrarySnapshot::default();
    let resolved = find_catalog_track(&library, &search, &key);

    assert!(resolved.is_some());
    assert!(matches!(
        resolved.unwrap().audio_source,
        AudioSource::Federation { .. }
    ));
}

#[test]
fn portable_device_queue_keeps_unresolved_content_as_a_federated_placeholder() {
    let content_id = ContentId::parse(format!("b3:{}", "d".repeat(64))).unwrap();
    let wire = music_dht::device_sync::PlaybackTrack {
        id: 12,
        title: "Portable track".into(),
        track_number: Some(2),
        disc_number: Some(1),
        duration_seconds: 123.0,
        artist_names: vec!["Artist".into()],
        featured_artist_names: vec!["Guest".into()],
        release_id: 4,
        release_title: "Release".into(),
        release_year: Some(2026),
        file_path: String::new(),
        content_id: Some(content_id.as_str().into()),
        audio_format: Some("flac".into()),
        audio_bitrate: Some(1_411),
        audio_sample_rate: Some(44_100),
        audio_bit_depth: Some(16),
        file_size_bytes: Some(42),
        play_count: 0,
        fed: None,
    };

    let placeholder = portable_playback_placeholder(&wire, content_id.clone());

    assert_eq!(placeholder.key.content_id(), Some(&content_id));
    assert_eq!(placeholder.artist, "Artist feat. Guest");
    assert!(matches!(
        placeholder.audio_source,
        AudioSource::Federation { ref peer_id, .. } if peer_id.is_empty()
    ));
}

fn playback_test_state(content_byte: char) -> music_dht::device_sync::PlaybackStateWire {
    music_dht::device_sync::PlaybackStateWire {
        queue: vec![music_dht::device_sync::PlaybackTrack {
            id: 1,
            title: format!("Track {content_byte}"),
            track_number: Some(1),
            disc_number: Some(1),
            duration_seconds: 180.0,
            artist_names: vec!["Artist".into()],
            featured_artist_names: Vec::new(),
            release_id: 1,
            release_title: "Album".into(),
            release_year: Some(2026),
            file_path: String::new(),
            content_id: Some(format!("b3:{}", content_byte.to_string().repeat(64))),
            audio_format: Some("flac".into()),
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
            fed: None,
        }],
        queue_pos: 0,
        playing: true,
        paused: false,
        idle_since_ms: None,
        position_secs: 40.0,
        volume: 72,
        shuffle: false,
        repeat: music_dht::device_sync::PlaybackRepeat::Off,
    }
}

#[test]
fn control_device_ignores_snapshots_from_the_previous_active_device() {
    let snapshot = music_dht::device_sync::PlaybackSnapshot {
        device_id: "old-device".into(),
        device_name: "Old".into(),
        active: true,
        updated_at_ms: 1,
        state: playback_test_state('a'),
    };

    assert!(!remote_snapshot_has_authority(
        DevicePlaybackRole::Control,
        "current-device",
        PlaybackStatus::Playing,
        false,
        &snapshot,
    ));
    assert!(remote_snapshot_has_authority(
        DevicePlaybackRole::Control,
        "old-device",
        PlaybackStatus::Playing,
        false,
        &snapshot,
    ));
}

#[test]
fn stale_remote_state_cannot_ack_a_new_control_command() {
    let expected = playback_test_state('a');
    let stale_track = playback_test_state('b');
    assert!(!playback_state_acknowledges_command(
        &expected,
        &stale_track,
        true,
        Duration::from_secs(2),
    ));

    let mut acknowledged = expected.clone();
    acknowledged.position_secs = 42.0;
    assert!(playback_state_acknowledges_command(
        &expected,
        &acknowledged,
        true,
        Duration::from_secs(2),
    ));

    acknowledged.position_secs = 5.0;
    assert!(!playback_state_acknowledges_command(
        &expected,
        &acknowledged,
        true,
        Duration::from_secs(2),
    ));
    assert!(playback_state_acknowledges_command(
        &expected,
        &acknowledged,
        false,
        Duration::from_secs(2),
    ));
}
