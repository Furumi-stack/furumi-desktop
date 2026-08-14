//! Trusted personal-device synchronization.
//!
//! The wire format is owned by `music-dht`; this module supplies the durable
//! operation log, materialized `SQLite` state and application integration.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context as _, Result};
use music_dht::device_sync::{
    DEFAULT_INVITE_TTL_MS, DEVICE_SYNC_PROTOCOL_VERSION, DeviceProfileWire, InviteWire,
    ListenEndReason, ListenEvent, ListenTrackMetadata, PlaybackCommand, PlaybackSnapshot,
    SnapshotLike, SnapshotLikeTombstone, SnapshotPlaylist, SnapshotPlaylistItem,
    SnapshotPlaylistItemTombstone, SnapshotPlaylistTombstone, SyncOpPayload, SyncOpWire,
    SyncSnapshot, SyncedFedTrack, WireMessage, encode_invite, finish_response, finish_send,
    hash_secret, parse_invite, random_hex, read_msg, ticket_endpoint_id, write_msg,
};
use music_dht::{ByteStream, MusicDhtService, PeerTicket, StreamAcceptor};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::InternalEvent;
mod store;

const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const SYNC_INTERVAL: Duration = Duration::from_secs(2);
const PAIR_RETRY: Duration = Duration::from_secs(1);
const PAIR_WAIT_MS: i64 = 5 * 60 * 1_000;
const RESPONSE_DRAIN: Duration = Duration::from_secs(2);
const ONLINE_TTL_MS: i64 = 15_000;
const MAX_OPS_PER_BATCH: usize = 1_000;
const PLAYBACK_COMMAND_TTL_MS: i64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub this_device_id: String,
    pub this_device_name: String,
    pub group_id: String,
    pub devices: Vec<DeviceRow>,
    pub pending: Vec<PendingPairing>,
    pub last_sync: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub client_version: String,
    pub is_self: bool,
    pub online: bool,
    pub revoked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PendingPairing {
    pub request_id: String,
    pub device_id: String,
    pub name: String,
    pub client_version: String,
    pub requester_group_id: Option<String>,
    pub requester_group_active_devices: usize,
}

#[derive(Debug, Clone)]
struct Identity {
    device_id: String,
    group_id: String,
    name: String,
}

#[derive(Clone)]
pub struct DeviceSync {
    conn: Arc<Mutex<Connection>>,
    library: Arc<furumi_library::Library>,
    events: tokio::sync::mpsc::Sender<InternalEvent>,
    playback: Arc<Mutex<PlaybackStore>>,
    sync_requested: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Default)]
struct PlaybackStore {
    local: Option<PlaybackSnapshot>,
    remote: HashMap<String, PlaybackSnapshot>,
}

impl DeviceSync {
    pub fn open(
        path: &Path,
        library: Arc<furumi_library::Library>,
        events: tokio::sync::mpsc::Sender<InternalEvent>,
    ) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening device sync database {}", path.display()))?;
        init_schema(&conn)?;
        let sync = Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
            library,
            events,
            playback: Arc::new(Mutex::new(PlaybackStore::default())),
            sync_requested: Arc::new(tokio::sync::Notify::new()),
        });
        sync.ensure_identity()?;
        Ok(sync)
    }

    pub fn status(&self) -> Status {
        self.status_inner().unwrap_or_else(|error| Status {
            error: Some(format!("{error:#}")),
            ..Status::default()
        })
    }

    pub fn identity(&self) -> Result<(String, String)> {
        let identity = self.ensure_identity()?;
        Ok((identity.device_id, identity.name))
    }

    pub fn new_listen_id() -> String {
        format!("{}-{}", now_ms(), random_hex(12))
    }

    pub fn set_device_name(&self, name: &str, endpoint_ticket: Option<&str>) -> Result<()> {
        let name = if name.trim().is_empty() {
            "furumi".to_string()
        } else {
            name.trim().to_string()
        };
        let identity = self.ensure_identity()?;
        {
            let conn = lock(&self.conn);
            set_meta(&conn, "device_name", &name)?;
            conn.execute(
                "UPDATE sync_devices
                 SET name = ?2, client_version = ?3
                 WHERE device_id = ?1",
                params![identity.device_id, name, CLIENT_VERSION],
            )?;
        }
        if let Some(ticket) = endpoint_ticket {
            self.record_op(SyncOpPayload::DeviceProfileSet {
                name,
                client_version: CLIENT_VERSION.to_string(),
                endpoint_ticket: ticket.to_string(),
                endpoint_id: ticket_endpoint_id(ticket).unwrap_or_default(),
            })?;
        } else {
            self.request_sync();
            self.notify();
        }
        Ok(())
    }

    pub fn publish_playback(&self, mut snapshot: PlaybackSnapshot) {
        if snapshot.updated_at_ms <= 0 {
            snapshot.updated_at_ms = now_ms();
        }
        lock(&self.playback).local = Some(snapshot);
    }

    /// Wake the sync loop immediately after an interactive operation.
    pub fn request_sync(&self) {
        self.sync_requested.notify_one();
    }

    pub async fn create_invite(&self, service: Arc<MusicDhtService>) -> Result<String> {
        let identity = self.ensure_identity()?;
        let secret = random_hex(16);
        let invite_id = random_hex(8);
        let expires_at_ms = now_ms() + DEFAULT_INVITE_TTL_MS;
        lock(&self.conn).execute(
            "INSERT INTO sync_invites (invite_id, secret_hash, expires_at_ms, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![invite_id, hash_secret(&secret), expires_at_ms, now_ms()],
        )?;
        encode_invite(&InviteWire {
            v: 1,
            ticket: service.ticket().await?.to_string(),
            device_id: identity.device_id,
            invite_id,
            secret,
            expires_at_ms,
        })
        .map_err(Into::into)
    }

    pub async fn connect_invite(
        &self,
        service: Arc<MusicDhtService>,
        value: &str,
    ) -> Result<String> {
        let invite = parse_invite(value)?;
        anyhow::ensure!(invite.expires_at_ms >= now_ms(), "device invite expired");
        let ticket =
            PeerTicket::from_str(&invite.ticket).context("invalid device invite ticket")?;
        let deadline = (now_ms() + PAIR_WAIT_MS).min(invite.expires_at_ms);
        loop {
            match self
                .try_connect_invite(Arc::clone(&service), &invite, ticket.clone())
                .await
            {
                Ok(PairAttempt::Accepted) => {
                    return Ok(format!("Connected to {}", short_id(&invite.device_id)));
                }
                Ok(PairAttempt::Denied(message)) => anyhow::bail!(message),
                Ok(PairAttempt::Pending) | Err(_) if now_ms() < deadline => {
                    tokio::time::sleep(PAIR_RETRY).await;
                }
                Ok(PairAttempt::Pending) => anyhow::bail!("pairing timed out"),
                Err(error) => anyhow::bail!("pairing timed out: {error:#}"),
            }
        }
    }

    async fn try_connect_invite(
        &self,
        service: Arc<MusicDhtService>,
        invite: &InviteWire,
        ticket: PeerTicket,
    ) -> Result<PairAttempt> {
        let peer = service.connect(ticket).await?;
        let identity = self.ensure_identity()?;
        let mut stream = service
            .open_stream(peer, music_dht::device_sync::SYNC_ALPN)
            .await?;
        let playback = lock(&self.playback).local.clone();
        write_msg(
            &mut stream,
            &WireMessage::PairRequest {
                invite_id: invite.invite_id.clone(),
                secret: invite.secret.clone(),
                profile: self.own_profile(&service.ticket().await?.to_string())?,
                group_id: Some(identity.group_id),
                group_active_devices: self.active_device_count()?,
                devices: self.device_profiles()?,
                vector: self.vector()?,
                ops: self.ops_for_peer(&invite.device_id)?,
                snapshot: self.snapshot()?,
                playback,
            },
        )
        .await?;
        finish_send(&mut stream).await?;
        match read_msg(&mut stream).await? {
            WireMessage::PairResponse {
                accepted: true,
                group_id: Some(group_id),
                profile,
                devices,
                vector,
                ops,
                snapshot,
                playback,
                ..
            } => {
                self.set_group_id(&group_id)?;
                if let Some(profile) = profile {
                    self.apply_device_profile(&profile, true)?;
                }
                self.apply_device_profiles(&devices)?;
                self.apply_snapshot(snapshot)?;
                self.apply_ops(ops)?;
                if let Some(playback) = playback {
                    self.apply_playback_snapshot(playback);
                }
                self.record_op(SyncOpPayload::DeviceTrusted {
                    target_device_id: invite.device_id.clone(),
                })?;
                self.note_peer_vector(&invite.device_id, &vector)?;
                self.set_meta("last_sync", &format!("paired · {}", time_label()))?;
                self.notify();
                Ok(PairAttempt::Accepted)
            }
            WireMessage::PairResponse {
                accepted: false,
                pending: true,
                ..
            } => Ok(PairAttempt::Pending),
            WireMessage::PairResponse {
                accepted: false,
                error,
                ..
            } => Ok(PairAttempt::Denied(
                error.unwrap_or_else(|| "pairing denied".into()),
            )),
            _ => anyhow::bail!("unexpected pairing response"),
        }
    }

    pub fn answer_pairing(
        &self,
        request_id: &str,
        accept: bool,
        use_requester_group: bool,
    ) -> Result<()> {
        let pending = {
            let conn = lock(&self.conn);
            conn.query_row(
                "SELECT device_id, name, client_version, endpoint_id, endpoint_ticket,
                        requester_group_id, requester_group_devices_json
                 FROM sync_pending_pairing WHERE request_id = ?1",
                [request_id],
                |row| {
                    Ok((
                        DeviceProfileWire {
                            device_id: row.get(0)?,
                            name: row.get(1)?,
                            client_version: row.get(2)?,
                            protocol_version: DEVICE_SYNC_PROTOCOL_VERSION,
                            endpoint_id: row.get(3)?,
                            endpoint_ticket: row.get(4)?,
                            revoked: false,
                            revoke_cutoff_seq: None,
                            updated_at_ms: now_ms(),
                        },
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?
        };
        let changed = lock(&self.conn).execute(
            "UPDATE sync_pending_pairing SET status = ?2, answered_at_ms = ?3,
                    use_requester_group = ?4
             WHERE request_id = ?1 AND status = 'pending'",
            params![
                request_id,
                if accept { "accepted" } else { "denied" },
                now_ms(),
                i64::from(use_requester_group)
            ],
        )?;
        if accept
            && changed > 0
            && let Some((profile, group, devices_json)) = pending
        {
            if use_requester_group {
                if let Some(group) = group.filter(|value| !value.trim().is_empty()) {
                    self.set_group_id(&group)?;
                }
                let devices: Vec<DeviceProfileWire> = serde_json::from_str(&devices_json)?;
                self.apply_device_profiles(&devices)?;
            }
            self.apply_device_profile(&profile, true)?;
            self.record_op(SyncOpPayload::DeviceTrusted {
                target_device_id: profile.device_id,
            })?;
        }
        self.notify();
        Ok(())
    }

    pub fn record_like(
        &self,
        content_id: &str,
        liked: bool,
        fed: Option<SyncedFedTrack>,
    ) -> Result<()> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(());
        };
        self.record_op(SyncOpPayload::TrackLikeSet {
            content_id,
            liked,
            fed,
        })
    }

    pub fn record_listen(
        &self,
        listen_id: String,
        track: &furumi_domain::Track,
        started_at_ms: i64,
        listened_ms: i64,
        ended_reason: ListenEndReason,
    ) -> Result<()> {
        let Some(content_id) = track
            .key
            .content_id()
            .and_then(|id| music_dht::normalize_content_id(id.as_str()))
        else {
            return Ok(());
        };
        let mut artist_names = track
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .filter(|artist| !artist.trim().is_empty())
            .collect::<Vec<_>>();
        if artist_names.is_empty() && !track.artist.trim().is_empty() {
            artist_names.push(track.artist.clone());
        }
        let event = ListenEvent {
            listen_id,
            content_id,
            started_at_ms,
            listened_ms: listened_ms.max(0),
            track_duration_ms: (track.duration_seconds > 0.0).then_some(
                crate::support::seconds_to_milliseconds(track.duration_seconds),
            ),
            ended_reason,
            track: ListenTrackMetadata {
                title: track.title.clone(),
                artist_names,
                featured_artist_names: track
                    .featured_artists
                    .iter()
                    .map(|artist| artist.name.clone())
                    .collect(),
                release_title: (!track.release.trim().is_empty()).then(|| track.release.clone()),
            },
        };
        if event.should_record() {
            self.record_op(SyncOpPayload::ListenRecorded { event })?;
        }
        Ok(())
    }

    pub fn record_playlist_created(&self, id: i64, title: &str) -> Result<()> {
        let playlist_id = self.library.ensure_playlist_sync_id(id)?;
        self.record_op(SyncOpPayload::PlaylistCreated {
            playlist_id,
            title: title.to_owned(),
        })
    }

    pub fn record_playlist_renamed(&self, id: i64, title: &str) -> Result<()> {
        let playlist_id = self.library.ensure_playlist_sync_id(id)?;
        self.record_op(SyncOpPayload::PlaylistRenamed {
            playlist_id,
            title: title.to_owned(),
        })
    }

    pub fn record_playlist_deleted(&self, sync_id: String) -> Result<()> {
        self.record_op(SyncOpPayload::PlaylistDeleted {
            playlist_id: sync_id,
        })
    }

    pub fn record_playlist_tracks_added(&self, playlist_id: i64, track_ids: &[i64]) -> Result<()> {
        let sync_id = self.library.ensure_playlist_sync_id(playlist_id)?;
        for (content_id, position) in self
            .library
            .playlist_track_content_positions(playlist_id, track_ids)?
        {
            self.record_op(SyncOpPayload::PlaylistTrackAdded {
                playlist_id: sync_id.clone(),
                content_id,
                position,
                fed: None,
            })?;
        }
        Ok(())
    }

    pub fn record_playlist_fed_added(
        &self,
        playlist_id: i64,
        tracks: &[(String, i64, SyncedFedTrack)],
    ) -> Result<()> {
        let sync_id = self.library.ensure_playlist_sync_id(playlist_id)?;
        for (content_id, position, fed) in tracks {
            self.record_op(SyncOpPayload::PlaylistTrackAdded {
                playlist_id: sync_id.clone(),
                content_id: content_id.clone(),
                position: *position,
                fed: Some(fed.clone()),
            })?;
        }
        Ok(())
    }

    pub fn record_playlist_removed(&self, playlist_id: i64, ids: &[String]) -> Result<()> {
        let Some(sync_id) = self.library.playlist_sync_id(playlist_id)? else {
            return Ok(());
        };
        for content_id in ids {
            if let Some(content_id) = music_dht::normalize_content_id(content_id) {
                self.record_op(SyncOpPayload::PlaylistTrackRemoved {
                    playlist_id: sync_id.clone(),
                    content_id,
                })?;
            }
        }
        Ok(())
    }

    pub fn record_playback_command(
        &self,
        target_device_id: &str,
        command: PlaybackCommand,
    ) -> Result<()> {
        self.record_op(SyncOpPayload::PlaybackCommand {
            target_device_id: target_device_id.to_owned(),
            command,
        })
    }
}

pub async fn serve(
    mut acceptor: StreamAcceptor,
    sync: Arc<DeviceSync>,
    service: Arc<MusicDhtService>,
) {
    while let Some(stream) = acceptor.accept().await {
        let sync = Arc::clone(&sync);
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            if let Err(error) = serve_one(stream, &sync, &service).await {
                let _ = sync.set_meta("last_error", &format!("incoming sync: {error:#}"));
                sync.notify();
            }
        });
    }
}

pub async fn sync_loop(sync: Arc<DeviceSync>, service: Arc<MusicDhtService>) {
    let mut interval = tokio::time::interval(SYNC_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = sync.sync_requested.notified() => {}
        }
        match sync.sync_once(Arc::clone(&service)).await {
            Ok(()) => {
                let _ = sync.set_meta("last_error", "");
            }
            Err(error) => {
                let _ = sync.set_meta("last_error", &format!("{error:#}"));
            }
        }
        sync.notify();
    }
}

impl DeviceSync {
    /// Synchronizes one explicitly selected device without waiting for the
    /// periodic all-device pass.
    pub async fn sync_target(
        &self,
        service: Arc<MusicDhtService>,
        target_device_id: &str,
    ) -> Result<()> {
        let device = self
            .active_remote_devices()?
            .into_iter()
            .find(|device| device.id == target_device_id)
            .with_context(|| format!("device {} is unavailable", short_id(target_device_id)))?;
        let result = self.sync_device(service, &device).await;
        if let Err(error) = &result {
            let _ = self.set_meta(
                "last_error",
                &format!("{}: {error:#}", short_id(target_device_id)),
            );
        }
        self.notify();
        result
    }

    async fn sync_once(&self, service: Arc<MusicDhtService>) -> Result<()> {
        let devices = self.active_remote_devices()?;
        let mut last_error = None;
        for device in devices {
            if let Err(error) = self.sync_device(Arc::clone(&service), &device).await {
                last_error = Some(error);
            }
        }
        if let Some(error) = last_error {
            return Err(error);
        }
        Ok(())
    }

    fn active_remote_devices(&self) -> Result<Vec<StoredDevice>> {
        let own = self.ensure_identity()?.device_id;
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT device_id, endpoint_id, endpoint_ticket FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL
               AND device_id != ?1 AND endpoint_ticket != ''",
        )?;
        Ok(stmt
            .query_map([own], |row| {
                Ok(StoredDevice {
                    id: row.get(0)?,
                    endpoint_id: row.get(1)?,
                    ticket: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    async fn sync_device(
        &self,
        service: Arc<MusicDhtService>,
        device: &StoredDevice,
    ) -> Result<()> {
        let peer = if let Ok(peer) = device.endpoint_id.parse::<music_dht::EndpointId>()
            && (service.is_connected(peer)
                || service
                    .known_peers()
                    .iter()
                    .any(|contact| contact.peer_id == peer))
        {
            peer
        } else {
            service
                .connect(PeerTicket::from_str(&device.ticket)?)
                .await?
        };
        let identity = self.ensure_identity()?;
        let mut stream = service
            .open_stream(peer, music_dht::device_sync::SYNC_ALPN)
            .await?;
        let playback = lock(&self.playback).local.clone();
        write_msg(
            &mut stream,
            &WireMessage::Hello {
                group_id: identity.group_id,
                profile: self.own_profile(&service.ticket().await?.to_string())?,
                devices: self.device_profiles()?,
                vector: self.vector()?,
                ops: self.ops_for_peer(&device.id)?,
                snapshot: self.snapshot()?,
                playback,
            },
        )
        .await?;
        finish_send(&mut stream).await?;
        match read_msg(&mut stream).await? {
            WireMessage::SyncResponse {
                accepted: true,
                devices,
                vector,
                ops,
                snapshot,
                playback,
                ..
            } => {
                self.apply_device_profiles(&devices)?;
                self.apply_snapshot(snapshot)?;
                self.apply_ops(ops)?;
                if let Some(playback) = playback {
                    self.apply_playback_snapshot(playback);
                }
                self.note_peer_vector(&device.id, &vector)?;
                self.mark_seen(&device.id, &peer.to_string())?;
                self.set_meta(
                    "last_sync",
                    &format!("{} · {}", time_label(), short_id(&device.id)),
                )?;
                Ok(())
            }
            WireMessage::SyncResponse {
                accepted: false,
                error,
                ..
            } => anyhow::bail!(error.unwrap_or_else(|| "sync refused".into())),
            _ => anyhow::bail!("unexpected sync response"),
        }
    }
}

#[derive(Debug)]
struct StoredDevice {
    id: String,
    endpoint_id: String,
    ticket: String,
}

async fn serve_one(
    mut stream: ByteStream,
    sync: &Arc<DeviceSync>,
    service: &Arc<MusicDhtService>,
) -> Result<()> {
    match read_msg(&mut stream).await? {
        WireMessage::PairRequest {
            invite_id,
            secret,
            mut profile,
            group_id,
            group_active_devices,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        } => {
            profile.endpoint_id = stream.peer_id.to_string();
            handle_pair_request(
                stream,
                sync,
                service,
                invite_id,
                secret,
                profile,
                group_id,
                group_active_devices,
                devices,
                vector,
                ops,
                snapshot,
                playback,
            )
            .await
        }
        WireMessage::Hello {
            group_id,
            mut profile,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        } => {
            profile.endpoint_id = stream.peer_id.to_string();
            handle_hello(
                stream, sync, service, group_id, profile, devices, vector, ops, snapshot, playback,
            )
            .await
        }
        _ => anyhow::bail!("unexpected first device-sync message"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_pair_request(
    mut stream: ByteStream,
    sync: &Arc<DeviceSync>,
    service: &Arc<MusicDhtService>,
    invite_id: String,
    secret: String,
    profile: DeviceProfileWire,
    requester_group_id: Option<String>,
    requester_group_active_devices: usize,
    requester_devices: Vec<DeviceProfileWire>,
    vector: BTreeMap<String, i64>,
    ops: Vec<SyncOpWire>,
    snapshot: SyncSnapshot,
    playback: Option<PlaybackSnapshot>,
) -> Result<()> {
    let request_id = pair_request_id(&invite_id, &profile.device_id);
    if !valid_pair_request(sync, &invite_id, &secret, &request_id)? {
        write_msg(&mut stream, &pair_error("invalid or expired invite", false)).await?;
        finish_response(&mut stream, RESPONSE_DRAIN).await?;
        return Ok(());
    }
    let requester_group_id = requester_group_id.filter(|group| !group.trim().is_empty());
    let requester_group_active_devices = requester_group_active_devices.max(1);
    let devices_json = serde_json::to_string(&requester_devices)?;
    lock(&sync.conn).execute(
        "INSERT OR IGNORE INTO sync_pending_pairing
            (request_id, device_id, name, client_version, endpoint_id, endpoint_ticket,
             invite_id, created_at_ms, status, requester_group_id,
             requester_group_active_devices, requester_group_devices_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?10, ?11)",
        params![
            request_id,
            profile.device_id,
            profile.name,
            profile.client_version,
            profile.endpoint_id,
            profile.endpoint_ticket,
            invite_id,
            now_ms(),
            requester_group_id,
            i64::try_from(requester_group_active_devices).unwrap_or(i64::MAX),
            devices_json,
        ],
    )?;
    sync.notify();
    let pairing = lock(&sync.conn)
        .query_row(
            "SELECT status, use_requester_group FROM sync_pending_pairing
             WHERE request_id = ?1",
            [&request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
        )
        .optional()?;
    match pairing.as_ref().map(|(status, _)| status.as_str()) {
        Some("pending") => {
            write_msg(&mut stream, &pair_error("pairing pending", true)).await?;
            finish_response(&mut stream, RESPONSE_DRAIN).await?;
            return Ok(());
        }
        Some("accepted") => {}
        _ => {
            write_msg(&mut stream, &pair_error("pairing denied", false)).await?;
            finish_response(&mut stream, RESPONSE_DRAIN).await?;
            return Ok(());
        }
    }
    let mut response_group = sync.ensure_identity()?.group_id;
    if pairing.is_some_and(|(_, use_group)| use_group)
        && let Some(group) = requester_group_id.filter(|group| !group.trim().is_empty())
    {
        sync.set_group_id(&group)?;
        response_group = group;
        sync.apply_device_profiles(&requester_devices)?;
    }
    sync.apply_device_profile(&profile, true)?;
    sync.apply_snapshot(snapshot)?;
    sync.apply_ops(ops)?;
    if let Some(playback) = playback {
        sync.apply_playback_snapshot(playback);
    }
    sync.note_peer_vector(&profile.device_id, &vector)?;
    lock(&sync.conn).execute(
        "UPDATE sync_invites SET used_at_ms = ?2 WHERE invite_id = ?1",
        params![invite_id, now_ms()],
    )?;
    let own_profile = sync.own_profile(&service.ticket().await?.to_string())?;
    let playback = lock(&sync.playback).local.clone();
    write_msg(
        &mut stream,
        &WireMessage::PairResponse {
            accepted: true,
            pending: false,
            error: None,
            group_id: Some(response_group),
            profile: Some(own_profile),
            devices: sync.device_profiles()?,
            vector: sync.vector()?,
            ops: sync.ops_for_peer(&profile.device_id)?,
            snapshot: sync.snapshot()?,
            playback,
        },
    )
    .await?;
    finish_response(&mut stream, RESPONSE_DRAIN).await?;
    sync.set_meta("last_sync", &format!("paired · {}", time_label()))?;
    sync.notify();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_hello(
    mut stream: ByteStream,
    sync: &Arc<DeviceSync>,
    service: &Arc<MusicDhtService>,
    group_id: String,
    profile: DeviceProfileWire,
    devices: Vec<DeviceProfileWire>,
    vector: BTreeMap<String, i64>,
    ops: Vec<SyncOpWire>,
    snapshot: SyncSnapshot,
    playback: Option<PlaybackSnapshot>,
) -> Result<()> {
    if group_id != sync.ensure_identity()?.group_id || !is_trusted(sync, &profile.device_id)? {
        write_msg(
            &mut stream,
            &WireMessage::SyncResponse {
                accepted: false,
                error: Some("device group mismatch or device is not trusted".into()),
                devices: Vec::new(),
                vector: BTreeMap::new(),
                ops: Vec::new(),
                snapshot: SyncSnapshot::default(),
                playback: None,
            },
        )
        .await?;
        finish_response(&mut stream, RESPONSE_DRAIN).await?;
        return Ok(());
    }
    sync.apply_device_profile(&profile, false)?;
    sync.apply_device_profiles(&devices)?;
    sync.apply_snapshot(snapshot)?;
    sync.apply_ops(ops)?;
    if let Some(playback) = playback {
        sync.apply_playback_snapshot(playback);
    }
    sync.note_peer_vector(&profile.device_id, &vector)?;
    sync.mark_seen(&profile.device_id, &stream.peer_id.to_string())?;
    let mut profiles = sync.device_profiles()?;
    profiles.push(sync.own_profile(&service.ticket().await?.to_string())?);
    let playback = lock(&sync.playback).local.clone();
    write_msg(
        &mut stream,
        &WireMessage::SyncResponse {
            accepted: true,
            error: None,
            devices: profiles,
            vector: sync.vector()?,
            ops: sync.ops_for_peer(&profile.device_id)?,
            snapshot: sync.snapshot()?,
            playback,
        },
    )
    .await?;
    finish_response(&mut stream, RESPONSE_DRAIN).await?;
    sync.set_meta(
        "last_sync",
        &format!("{} · {}", time_label(), short_id(&profile.device_id)),
    )?;
    sync.notify();
    Ok(())
}

fn pair_error(message: &str, pending: bool) -> WireMessage {
    WireMessage::PairResponse {
        accepted: false,
        pending,
        error: Some(message.into()),
        group_id: None,
        profile: None,
        devices: Vec::new(),
        vector: BTreeMap::new(),
        ops: Vec::new(),
        snapshot: SyncSnapshot::default(),
        playback: None,
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS sync_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_devices (
    device_id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    client_version TEXT NOT NULL DEFAULT '',
    protocol_version INTEGER NOT NULL DEFAULT 1,
    endpoint_id TEXT NOT NULL DEFAULT '',
    endpoint_ticket TEXT NOT NULL DEFAULT '',
    trusted_at_ms INTEGER,
    last_seen_ms INTEGER,
    revoked_at_ms INTEGER,
    revoked_by TEXT,
    revoke_cutoff_seq INTEGER
);
CREATE TABLE IF NOT EXISTS sync_invites (
    invite_id TEXT PRIMARY KEY,
    secret_hash TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    used_at_ms INTEGER
);
CREATE TABLE IF NOT EXISTS sync_pending_pairing (
    request_id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    name TEXT NOT NULL,
    client_version TEXT NOT NULL,
    endpoint_id TEXT NOT NULL,
    endpoint_ticket TEXT NOT NULL,
    invite_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    answered_at_ms INTEGER,
    status TEXT NOT NULL,
    requester_group_id TEXT,
    requester_group_active_devices INTEGER NOT NULL DEFAULT 1,
    requester_group_devices_json TEXT NOT NULL DEFAULT '[]',
    use_requester_group INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS sync_ops (
    op_id TEXT PRIMARY KEY,
    origin_device_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    hlc_ms INTEGER NOT NULL,
    received_at_ms INTEGER NOT NULL,
    tombstone INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sync_ops_origin_seq
    ON sync_ops(origin_device_id, seq);
CREATE TABLE IF NOT EXISTS sync_vectors (
    device_id TEXT PRIMARY KEY,
    max_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_peer_acks (
    peer_device_id TEXT NOT NULL,
    origin_device_id TEXT NOT NULL,
    max_seq INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (peer_device_id, origin_device_id)
);
CREATE TABLE IF NOT EXISTS sync_state_likes (
    content_id TEXT PRIMARY KEY,
    liked INTEGER NOT NULL,
    hlc_ms INTEGER NOT NULL,
    op_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state_playlists (
    playlist_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    hlc_ms INTEGER NOT NULL,
    op_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state_playlist_items (
    playlist_id TEXT NOT NULL,
    content_id TEXT NOT NULL,
    present INTEGER NOT NULL,
    position INTEGER NOT NULL,
    hlc_ms INTEGER NOT NULL,
    op_id TEXT NOT NULL,
    PRIMARY KEY (playlist_id, content_id)
);
CREATE TABLE IF NOT EXISTS sync_playback_applied (
    op_id TEXT PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL
);
",
    )?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM sync_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        conn.execute("DELETE FROM sync_meta WHERE key = ?1", [key])?;
    } else {
        conn.execute(
            "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn time_label() -> String {
    let seconds = (now_ms() / 1_000).max(0);
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3_600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn short_id(value: &str) -> String {
    value.chars().take(10).collect()
}

fn pair_request_id(invite_id: &str, device_id: &str) -> String {
    let digest = hash_secret(&format!("{invite_id}:{device_id}"));
    format!("pair_{}", &digest[..16])
}

fn requester_group_conflict(
    local_group_id: &str,
    requester_group_id: Option<&str>,
    requester_group_active_devices: usize,
) -> bool {
    requester_group_id.is_some_and(|group| !group.trim().is_empty() && group != local_group_id)
        && requester_group_active_devices > 1
}

fn valid_pair_request(
    sync: &DeviceSync,
    invite_id: &str,
    secret: &str,
    request_id: &str,
) -> Result<bool> {
    let conn = lock(&sync.conn);
    let row = conn
        .query_row(
            "SELECT secret_hash, expires_at_ms, used_at_ms FROM sync_invites
             WHERE invite_id = ?1",
            [invite_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((expected, expires, used)) = row else {
        return Ok(false);
    };
    if expires < now_ms() || expected != hash_secret(secret) {
        return Ok(false);
    }
    if used.is_none() {
        return Ok(true);
    }
    Ok(conn
        .query_row(
            "SELECT 1 FROM sync_pending_pairing
             WHERE request_id = ?1 AND status = 'accepted'",
            [request_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

fn is_trusted(sync: &DeviceSync, device_id: &str) -> Result<bool> {
    if device_id == sync.ensure_identity()?.device_id {
        return Ok(true);
    }
    Ok(lock(&sync.conn)
        .query_row(
            "SELECT 1 FROM sync_devices WHERE device_id = ?1
             AND trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
            [device_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

fn payload_kind(payload: &SyncOpPayload) -> &'static str {
    match payload {
        SyncOpPayload::TrackLikeSet { .. } => "track_like_set",
        SyncOpPayload::PlaylistCreated { .. } => "playlist_created",
        SyncOpPayload::PlaylistRenamed { .. } => "playlist_renamed",
        SyncOpPayload::PlaylistDeleted { .. } => "playlist_deleted",
        SyncOpPayload::PlaylistTrackAdded { .. } => "playlist_track_added",
        SyncOpPayload::PlaylistTrackRemoved { .. } => "playlist_track_removed",
        SyncOpPayload::DeviceProfileSet { .. } => "device_profile_set",
        SyncOpPayload::DeviceTrusted { .. } => "device_trusted",
        SyncOpPayload::DeviceRevoked { .. } => "device_revoked",
        SyncOpPayload::PlaybackCommand { .. } => "playback_command",
        SyncOpPayload::ListenRecorded { .. } => "listen_recorded",
    }
}

fn to_library_fed(fed: &SyncedFedTrack) -> furumi_library::FederatedTrack {
    furumi_library::FederatedTrack {
        item_id: fed.item_id.clone(),
        owner: fed.owner.clone(),
        own: false,
        title: fed.title.clone(),
        artist_names: fed.artist_names.clone(),
        featured_artist_names: fed.featured_artist_names.clone(),
        year: fed.year,
        duration_seconds: fed.duration_seconds,
        content_id: Some(fed.content_id.clone()),
        release_title: fed.release_title.clone(),
        track_number: fed.track_number,
        disc_number: fed.disc_number,
    }
}

enum PairAttempt {
    Accepted,
    Pending,
    Denied(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEST_DB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    fn with_test_sync(test: impl FnOnce(&DeviceSync)) {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let unique = NEXT_TEST_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let library_path = std::env::temp_dir().join(format!(
            "furumi-desktop-devices-test-{}-{}-{}.sqlite3",
            std::process::id(),
            now_ms(),
            unique
        ));
        let library = Arc::new(furumi_library::Library::open(&library_path).unwrap());
        let (events, _event_rx) = tokio::sync::mpsc::channel(4);
        let sync = DeviceSync {
            conn: Arc::new(Mutex::new(conn)),
            library,
            events,
            playback: Arc::new(Mutex::new(PlaybackStore::default())),
            sync_requested: Arc::new(tokio::sync::Notify::new()),
        };
        sync.ensure_identity().unwrap();

        test(&sync);

        drop(sync);
        for suffix in ["", "-wal", "-shm"] {
            let mut path = library_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }

    fn test_profile(device_id: &str) -> DeviceProfileWire {
        DeviceProfileWire {
            device_id: device_id.into(),
            name: device_id.into(),
            client_version: CLIENT_VERSION.into(),
            protocol_version: DEVICE_SYNC_PROTOCOL_VERSION,
            endpoint_id: String::new(),
            endpoint_ticket: String::new(),
            revoked: false,
            revoke_cutoff_seq: None,
            updated_at_ms: now_ms(),
        }
    }

    #[test]
    fn locally_finished_listen_enters_the_shared_history() {
        with_test_sync(|sync| {
            let content_id =
                furumi_domain::ContentId::parse(format!("b3:{}", "a".repeat(64))).unwrap();
            let track = furumi_domain::Track {
                key: furumi_domain::TrackKey::remote(content_id.clone()),
                title: "Shared listen".into(),
                artist: "Artist".into(),
                artists: vec![furumi_domain::ArtistRef {
                    key: furumi_domain::ArtistKey::Federation {
                        peer_id: "peer".into(),
                        id: "artist".into(),
                    },
                    name: "Artist".into(),
                }],
                featured_artists: Vec::new(),
                release: "Release".into(),
                release_id: furumi_domain::ReleaseKey::Federation {
                    peer_id: "peer".into(),
                    id: "release".into(),
                },
                duration_seconds: 180.0,
                track_number: Some(1),
                disc_number: Some(1),
                cover_uri: None,
                audio_format: None,
                audio_bitrate_kbps: None,
                audio_sample_rate_hz: None,
                audio_bit_depth: None,
                file_size_bytes: None,
                liked: false,
                audio_source: furumi_domain::AudioSource::Federation {
                    peer_id: "peer".into(),
                    content_id,
                },
            };

            sync.record_listen(
                "desktop-listen".into(),
                &track,
                now_ms(),
                180_000,
                ListenEndReason::Finished,
            )
            .unwrap();

            let history = sync.library.listen_history(10).unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].listen_id, "desktop-listen");
            assert_eq!(history[0].title, "Shared listen");
        });
    }

    fn insert_pending_pairing(sync: &DeviceSync, requester_group_devices: &[DeviceProfileWire]) {
        lock(&sync.conn)
            .execute(
                "INSERT INTO sync_pending_pairing
                    (request_id, device_id, name, client_version, endpoint_id,
                     endpoint_ticket, invite_id, created_at_ms, status,
                     requester_group_id, requester_group_active_devices,
                     requester_group_devices_json)
                 VALUES ('request', 'dev_requester', 'Requester', ?1, '', '',
                         'invite', ?2, 'pending', 'grp_remote', 2, ?3)",
                params![
                    CLIENT_VERSION,
                    now_ms(),
                    serde_json::to_string(requester_group_devices).unwrap()
                ],
            )
            .unwrap();
    }

    fn device_known(sync: &DeviceSync, device_id: &str) -> bool {
        lock(&sync.conn)
            .query_row(
                "SELECT 1 FROM sync_devices WHERE device_id = ?1",
                [device_id],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some()
    }

    fn device_trusted(sync: &DeviceSync, device_id: &str) -> bool {
        lock(&sync.conn)
            .query_row(
                "SELECT trusted_at_ms IS NOT NULL FROM sync_devices WHERE device_id = ?1",
                [device_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .unwrap()
            .unwrap_or(false)
    }

    #[test]
    fn pairing_request_ids_are_stable_and_scoped_to_the_device() {
        assert_eq!(
            pair_request_id("invite", "device"),
            pair_request_id("invite", "device")
        );
        assert_ne!(
            pair_request_id("invite", "device-a"),
            pair_request_id("invite", "device-b")
        );
    }

    #[test]
    fn group_choice_is_only_required_for_an_existing_different_group() {
        assert!(requester_group_conflict("grp_local", Some("grp_remote"), 2));
        assert!(!requester_group_conflict(
            "grp_local",
            Some("grp_remote"),
            1
        ));
        assert!(!requester_group_conflict("grp_local", Some("grp_local"), 3));
        assert!(!requester_group_conflict("grp_local", None, 3));
        assert!(!requester_group_conflict("grp_local", Some("  "), 3));
    }

    #[test]
    fn pairing_choice_either_joins_the_requester_group_or_keeps_the_local_group() {
        let requester_peer = test_profile("dev_requester_peer");

        with_test_sync(|sync| {
            let local_group = sync.ensure_identity().unwrap().group_id;
            insert_pending_pairing(sync, std::slice::from_ref(&requester_peer));

            sync.answer_pairing("request", true, false).unwrap();

            assert_eq!(sync.ensure_identity().unwrap().group_id, local_group);
            assert!(device_trusted(sync, "dev_requester"));
            assert!(!device_known(sync, "dev_requester_peer"));
        });

        with_test_sync(|sync| {
            insert_pending_pairing(sync, std::slice::from_ref(&requester_peer));

            sync.answer_pairing("request", true, true).unwrap();

            assert_eq!(sync.ensure_identity().unwrap().group_id, "grp_remote");
            assert!(device_trusted(sync, "dev_requester"));
            assert!(device_known(sync, "dev_requester_peer"));
        });
    }
}
