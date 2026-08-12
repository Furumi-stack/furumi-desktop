use super::*;
impl DeviceSync {
    pub(super) fn ensure_identity(&self) -> Result<Identity> {
        let conn = lock(&self.conn);
        if let (Some(device_id), Some(group_id), Some(name)) = (
            get_meta(&conn, "device_id")?,
            get_meta(&conn, "group_id")?,
            get_meta(&conn, "device_name")?,
        ) {
            return Ok(Identity {
                device_id,
                group_id,
                name,
            });
        }
        let seed = random_hex(32);
        let digest = hash_secret(&seed);
        let device_id = format!("dev_{}", &digest[..24]);
        let group_id = format!("grp_{}", &hash_secret(&device_id)[..24]);
        let name = format!("Furumi on {}", std::env::consts::OS);
        set_meta(&conn, "device_id", &device_id)?;
        set_meta(&conn, "device_secret", &seed)?;
        set_meta(&conn, "group_id", &group_id)?;
        set_meta(&conn, "device_name", &name)?;
        set_meta(&conn, "local_seq", "0")?;
        set_meta(&conn, "last_hlc_ms", "0")?;
        conn.execute(
            "INSERT INTO sync_devices
                (device_id, name, client_version, protocol_version, trusted_at_ms, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(device_id) DO UPDATE SET name = excluded.name,
                 client_version = excluded.client_version,
                 protocol_version = excluded.protocol_version,
                 trusted_at_ms = COALESCE(sync_devices.trusted_at_ms, excluded.trusted_at_ms)",
            params![
                device_id,
                name,
                CLIENT_VERSION,
                DEVICE_SYNC_PROTOCOL_VERSION,
                now_ms()
            ],
        )?;
        Ok(Identity {
            device_id,
            group_id,
            name,
        })
    }

    pub(super) fn set_group_id(&self, group_id: &str) -> Result<()> {
        let conn = lock(&self.conn);
        set_meta(&conn, "group_id", group_id)
    }

    pub(super) fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        set_meta(&lock(&self.conn), key, value)
    }

    pub(super) fn own_profile(&self, ticket: &str) -> Result<DeviceProfileWire> {
        let identity = self.ensure_identity()?;
        Ok(DeviceProfileWire {
            device_id: identity.device_id,
            name: identity.name,
            client_version: CLIENT_VERSION.into(),
            protocol_version: DEVICE_SYNC_PROTOCOL_VERSION,
            endpoint_id: ticket_endpoint_id(ticket).unwrap_or_default(),
            endpoint_ticket: ticket.into(),
            revoked: false,
            revoke_cutoff_seq: None,
            updated_at_ms: now_ms(),
        })
    }

    pub(super) fn active_device_count(&self) -> Result<usize> {
        let count = lock(&self.conn).query_row(
            "SELECT COUNT(*) FROM sync_devices
             WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(usize::try_from(count.max(0)).unwrap_or(usize::MAX))
    }

    pub(super) fn status_inner(&self) -> Result<Status> {
        let identity = self.ensure_identity()?;
        let conn = lock(&self.conn);
        let now = now_ms();
        let devices = {
            let mut stmt = conn.prepare(
                "SELECT device_id, name, client_version, last_seen_ms
                 FROM sync_devices
                 WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL
                 ORDER BY name COLLATE NOCASE, device_id",
            )?;
            stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let last_seen: Option<i64> = row.get(3)?;
                Ok(DeviceRow {
                    is_self: id == identity.device_id,
                    online: id == identity.device_id
                        || last_seen.is_some_and(|seen| now.saturating_sub(seen) <= ONLINE_TTL_MS),
                    id,
                    name: row.get(1)?,
                    client_version: row.get(2)?,
                    revoked: false,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let pending = {
            let mut stmt = conn.prepare(
                "SELECT request_id, device_id, name, client_version,
                        requester_group_id, requester_group_active_devices
                 FROM sync_pending_pairing WHERE status = 'pending'
                 ORDER BY created_at_ms",
            )?;
            stmt.query_map([], |row| {
                Ok(PendingPairing {
                    request_id: row.get(0)?,
                    device_id: row.get(1)?,
                    name: row.get(2)?,
                    client_version: row.get(3)?,
                    requester_group_id: row.get(4)?,
                    requester_group_active_devices: usize::try_from(row.get::<_, i64>(5)?.max(0))
                        .unwrap_or(usize::MAX),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Status {
            this_device_id: identity.device_id,
            this_device_name: identity.name,
            group_id: identity.group_id,
            devices,
            pending,
            last_sync: get_meta(&conn, "last_sync")?,
            error: get_meta(&conn, "last_error")?,
        })
    }

    pub(super) fn device_profiles(&self) -> Result<Vec<DeviceProfileWire>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT device_id, name, client_version, protocol_version, endpoint_id,
                    endpoint_ticket, revoked_at_ms IS NOT NULL, revoke_cutoff_seq,
                    MAX(COALESCE(last_seen_ms, 0), COALESCE(trusted_at_ms, 0),
                        COALESCE(revoked_at_ms, 0))
             FROM sync_devices WHERE trusted_at_ms IS NOT NULL",
        )?;
        Ok(stmt
            .query_map([], |row| {
                Ok(DeviceProfileWire {
                    device_id: row.get(0)?,
                    name: row.get(1)?,
                    client_version: row.get(2)?,
                    protocol_version: row.get::<_, u16>(3)?,
                    endpoint_id: row.get(4)?,
                    endpoint_ticket: row.get(5)?,
                    revoked: row.get::<_, i64>(6)? != 0,
                    revoke_cutoff_seq: row.get(7)?,
                    updated_at_ms: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(super) fn apply_device_profiles(&self, profiles: &[DeviceProfileWire]) -> Result<()> {
        for profile in profiles {
            self.apply_device_profile(profile, false)?;
        }
        Ok(())
    }

    pub(super) fn apply_device_profile(
        &self,
        profile: &DeviceProfileWire,
        trusted: bool,
    ) -> Result<()> {
        if profile.device_id == self.ensure_identity()?.device_id {
            return Ok(());
        }
        let now = now_ms();
        lock(&self.conn).execute(
            "INSERT INTO sync_devices
                (device_id, name, client_version, protocol_version, endpoint_id,
                 endpoint_ticket, trusted_at_ms, last_seen_ms, revoked_at_ms,
                 revoke_cutoff_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(device_id) DO UPDATE SET
                name = CASE WHEN excluded.last_seen_ms >= COALESCE(sync_devices.last_seen_ms, 0)
                            THEN excluded.name ELSE sync_devices.name END,
                client_version = excluded.client_version,
                protocol_version = excluded.protocol_version,
                endpoint_id = CASE WHEN excluded.endpoint_id != '' THEN excluded.endpoint_id
                                   ELSE sync_devices.endpoint_id END,
                endpoint_ticket = CASE WHEN excluded.endpoint_ticket != '' THEN excluded.endpoint_ticket
                                       ELSE sync_devices.endpoint_ticket END,
                trusted_at_ms = COALESCE(sync_devices.trusted_at_ms, excluded.trusted_at_ms),
                last_seen_ms = MAX(COALESCE(sync_devices.last_seen_ms, 0), excluded.last_seen_ms),
                revoked_at_ms = CASE WHEN excluded.revoked_at_ms IS NOT NULL
                                     THEN excluded.revoked_at_ms ELSE sync_devices.revoked_at_ms END,
                revoke_cutoff_seq = COALESCE(excluded.revoke_cutoff_seq,
                                             sync_devices.revoke_cutoff_seq)",
            params![
                profile.device_id,
                profile.name,
                profile.client_version,
                profile.protocol_version,
                profile.endpoint_id,
                profile.endpoint_ticket,
                trusted.then_some(now),
                profile.updated_at_ms.max(now),
                profile.revoked.then_some(profile.updated_at_ms.max(now)),
                profile.revoke_cutoff_seq,
            ],
        )?;
        Ok(())
    }

    pub(super) fn mark_seen(&self, device_id: &str, endpoint_id: &str) -> Result<()> {
        lock(&self.conn).execute(
            "UPDATE sync_devices SET last_seen_ms = ?2,
                    endpoint_id = CASE WHEN ?3 != '' THEN ?3 ELSE endpoint_id END
             WHERE device_id = ?1",
            params![device_id, now_ms(), endpoint_id],
        )?;
        Ok(())
    }

    pub(super) fn record_op(&self, payload: SyncOpPayload) -> Result<()> {
        let op = {
            let conn = lock(&self.conn);
            let identity = Self::ensure_identity_with_conn(&conn)?;
            let seq = get_meta(&conn, "local_seq")?
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0)
                .saturating_add(1);
            let previous_hlc = get_meta(&conn, "last_hlc_ms")?
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0);
            let hlc_ms = now_ms().max(previous_hlc.saturating_add(1));
            set_meta(&conn, "local_seq", &seq.to_string())?;
            set_meta(&conn, "last_hlc_ms", &hlc_ms.to_string())?;
            SyncOpWire {
                op_id: format!("{}:{seq}", identity.device_id),
                origin_device_id: identity.device_id,
                seq,
                hlc_ms,
                payload,
            }
        };
        self.store_and_apply_op(&op)?;
        self.request_sync();
        self.notify();
        Ok(())
    }

    pub(super) fn ensure_identity_with_conn(conn: &Connection) -> Result<Identity> {
        Ok(Identity {
            device_id: get_meta(conn, "device_id")?.context("missing device id")?,
            group_id: get_meta(conn, "group_id")?.context("missing device group")?,
            name: get_meta(conn, "device_name")?.context("missing device name")?,
        })
    }

    pub(super) fn store_and_apply_op(&self, op: &SyncOpWire) -> Result<()> {
        let inserted = lock(&self.conn).execute(
            "INSERT OR IGNORE INTO sync_ops
                (op_id, origin_device_id, seq, kind, payload_json, hlc_ms,
                 received_at_ms, tombstone)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                op.op_id,
                op.origin_device_id,
                op.seq,
                payload_kind(&op.payload),
                serde_json::to_string(&op.payload)?,
                op.hlc_ms,
                now_ms(),
                i64::from(op.payload.is_tombstone()),
            ],
        )?;
        if inserted == 0 {
            return Ok(());
        }
        lock(&self.conn).execute(
            "INSERT INTO sync_vectors (device_id, max_seq) VALUES (?1, ?2)
             ON CONFLICT(device_id) DO UPDATE SET max_seq = MAX(max_seq, excluded.max_seq)",
            params![op.origin_device_id, op.seq],
        )?;
        self.apply_op(op)
    }

    pub(super) fn apply_ops(&self, ops: Vec<SyncOpWire>) -> Result<()> {
        for op in ops {
            if self.should_accept_op(&op)? {
                self.store_and_apply_op(&op)?;
            }
        }
        self.notify();
        Ok(())
    }

    pub(super) fn should_accept_op(&self, op: &SyncOpWire) -> Result<bool> {
        if op.origin_device_id == self.ensure_identity()?.device_id {
            return Ok(true);
        }
        let conn = lock(&self.conn);
        let row = conn
            .query_row(
                "SELECT trusted_at_ms IS NOT NULL, revoked_at_ms IS NOT NULL,
                        COALESCE(revoke_cutoff_seq, 0)
                 FROM sync_devices WHERE device_id = ?1",
                [&op.origin_device_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.is_some_and(|(trusted, revoked, cutoff)| trusted && (!revoked || op.seq <= cutoff)))
    }

    pub(super) fn vector(&self) -> Result<BTreeMap<String, i64>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare("SELECT device_id, max_seq FROM sync_vectors")?;
        Ok(stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }

    pub(super) fn ops_for_peer(&self, peer: &str) -> Result<Vec<SyncOpWire>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT payload_json, op_id, origin_device_id, seq, hlc_ms
             FROM sync_ops o
             WHERE seq > COALESCE((SELECT max_seq FROM sync_peer_acks a
                                   WHERE a.peer_device_id = ?1
                                     AND a.origin_device_id = o.origin_device_id), 0)
               AND (kind != 'playback_command' OR hlc_ms >= ?2)
             ORDER BY received_at_ms, origin_device_id, seq LIMIT ?3",
        )?;
        Ok(stmt
            .query_map(
                params![
                    peer,
                    now_ms().saturating_sub(PLAYBACK_COMMAND_TTL_MS),
                    i64::try_from(MAX_OPS_PER_BATCH).unwrap_or(i64::MAX)
                ],
                |row| {
                    let payload: String = row.get(0)?;
                    Ok(SyncOpWire {
                        op_id: row.get(1)?,
                        origin_device_id: row.get(2)?,
                        seq: row.get(3)?,
                        hlc_ms: row.get(4)?,
                        payload: serde_json::from_str(&payload).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(super) fn note_peer_vector(
        &self,
        peer: &str,
        vector: &BTreeMap<String, i64>,
    ) -> Result<()> {
        let conn = lock(&self.conn);
        for (origin, seq) in vector {
            conn.execute(
                "INSERT INTO sync_peer_acks
                    (peer_device_id, origin_device_id, max_seq, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(peer_device_id, origin_device_id) DO UPDATE SET
                    max_seq = MAX(max_seq, excluded.max_seq),
                    updated_at_ms = excluded.updated_at_ms",
                params![peer, origin, seq, now_ms()],
            )?;
        }
        Ok(())
    }
}

impl DeviceSync {
    pub(super) fn apply_op(&self, op: &SyncOpWire) -> Result<()> {
        match &op.payload {
            SyncOpPayload::TrackLikeSet {
                content_id,
                liked,
                fed,
            } => self.apply_like(content_id, *liked, fed.as_ref(), op.hlc_ms, &op.op_id)?,
            SyncOpPayload::PlaylistCreated { playlist_id, title }
            | SyncOpPayload::PlaylistRenamed { playlist_id, title } => {
                self.apply_playlist(playlist_id, title, false, op.hlc_ms, &op.op_id)?;
            }
            SyncOpPayload::PlaylistDeleted { playlist_id } => {
                self.apply_playlist(playlist_id, "", true, op.hlc_ms, &op.op_id)?;
            }
            SyncOpPayload::PlaylistTrackAdded {
                playlist_id,
                content_id,
                position,
                fed,
            } => self.apply_playlist_item(
                playlist_id,
                content_id,
                true,
                *position,
                fed.as_ref(),
                op.hlc_ms,
                &op.op_id,
            )?,
            SyncOpPayload::PlaylistTrackRemoved {
                playlist_id,
                content_id,
            } => self.apply_playlist_item(
                playlist_id,
                content_id,
                false,
                0,
                None,
                op.hlc_ms,
                &op.op_id,
            )?,
            SyncOpPayload::DeviceProfileSet {
                name,
                client_version,
                endpoint_ticket,
                endpoint_id,
            } => self.apply_device_profile(
                &DeviceProfileWire {
                    device_id: op.origin_device_id.clone(),
                    name: name.clone(),
                    client_version: client_version.clone(),
                    protocol_version: DEVICE_SYNC_PROTOCOL_VERSION,
                    endpoint_id: endpoint_id.clone(),
                    endpoint_ticket: endpoint_ticket.clone(),
                    revoked: false,
                    revoke_cutoff_seq: None,
                    updated_at_ms: op.hlc_ms,
                },
                false,
            )?,
            SyncOpPayload::DeviceTrusted { target_device_id } => {
                lock(&self.conn).execute(
                    "UPDATE sync_devices SET trusted_at_ms = MAX(COALESCE(trusted_at_ms, 0), ?2),
                            revoked_at_ms = CASE WHEN COALESCE(revoked_at_ms, 0) <= ?2
                                                 THEN NULL ELSE revoked_at_ms END
                     WHERE device_id = ?1",
                    params![target_device_id, op.hlc_ms],
                )?;
            }
            SyncOpPayload::DeviceRevoked {
                target_device_id,
                target_max_seq_seen,
            } => {
                lock(&self.conn).execute(
                    "UPDATE sync_devices SET revoked_at_ms = ?2, revoked_by = ?3,
                            revoke_cutoff_seq = ?4 WHERE device_id = ?1",
                    params![
                        target_device_id,
                        op.hlc_ms,
                        op.origin_device_id,
                        target_max_seq_seen
                    ],
                )?;
            }
            SyncOpPayload::PlaybackCommand {
                target_device_id,
                command,
            } => self.apply_playback_command(target_device_id, command, &op.op_id)?,
            SyncOpPayload::ListenRecorded { event } => {
                self.library
                    .apply_listen_event(event, &op.origin_device_id)?;
            }
        }
        Ok(())
    }

    pub(super) fn apply_like(
        &self,
        content_id: &str,
        liked: bool,
        fed: Option<&SyncedFedTrack>,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<()> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(());
        };
        if !self.lww_wins("sync_state_likes", "content_id", &content_id, hlc_ms, op_id)? {
            return Ok(());
        }
        lock(&self.conn).execute(
            "INSERT INTO sync_state_likes (content_id, liked, hlc_ms, op_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(content_id) DO UPDATE SET liked = excluded.liked,
                 hlc_ms = excluded.hlc_ms, op_id = excluded.op_id",
            params![content_id, i64::from(liked), hlc_ms, op_id],
        )?;
        if let Some(track_id) = self.library.track_id_by_content_id(&content_id)? {
            self.library.set_synced_like(track_id, liked, hlc_ms)?;
            if liked {
                self.library.remove_fed_like_by_content_id(&content_id)?;
            }
        } else if liked {
            if let Some(fed) = fed {
                self.library
                    .upsert_synced_fed_like(&to_library_fed(fed), hlc_ms)?;
            }
        } else {
            self.library.remove_fed_like_by_content_id(&content_id)?;
        }
        self.notify_library();
        Ok(())
    }

    pub(super) fn apply_playlist(
        &self,
        playlist_id: &str,
        title: &str,
        deleted: bool,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<()> {
        if !self.lww_wins(
            "sync_state_playlists",
            "playlist_id",
            playlist_id,
            hlc_ms,
            op_id,
        )? {
            return Ok(());
        }
        lock(&self.conn).execute(
            "INSERT INTO sync_state_playlists
                (playlist_id, title, deleted, hlc_ms, op_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(playlist_id) DO UPDATE SET title = excluded.title,
                 deleted = excluded.deleted, hlc_ms = excluded.hlc_ms,
                 op_id = excluded.op_id",
            params![playlist_id, title, i64::from(deleted), hlc_ms, op_id],
        )?;
        if deleted {
            self.library.delete_playlist_by_sync_id(playlist_id)?;
        } else if !title.trim().is_empty() {
            self.library.upsert_synced_playlist(playlist_id, title)?;
        }
        self.notify_library();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_playlist_item(
        &self,
        playlist_id: &str,
        content_id: &str,
        present: bool,
        position: i64,
        fed: Option<&SyncedFedTrack>,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<()> {
        let Some(content_id) = music_dht::normalize_content_id(content_id) else {
            return Ok(());
        };
        let current = lock(&self.conn)
            .query_row(
                "SELECT hlc_ms, op_id FROM sync_state_playlist_items
                 WHERE playlist_id = ?1 AND content_id = ?2",
                params![playlist_id, content_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if current.is_some_and(|(current_hlc, current_op)| {
            (current_hlc, current_op.as_str()) >= (hlc_ms, op_id)
        }) {
            return Ok(());
        }
        lock(&self.conn).execute(
            "INSERT INTO sync_state_playlist_items
                (playlist_id, content_id, present, position, hlc_ms, op_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(playlist_id, content_id) DO UPDATE SET
                present = excluded.present, position = excluded.position,
                hlc_ms = excluded.hlc_ms, op_id = excluded.op_id",
            params![
                playlist_id,
                content_id,
                i64::from(present),
                position,
                hlc_ms,
                op_id
            ],
        )?;
        if present {
            self.library
                .add_content_id_to_synced_playlist(playlist_id, &content_id)?;
            if let Some(fed) = fed {
                self.library.upsert_fed_playlist_track(
                    playlist_id,
                    &to_library_fed(fed),
                    position,
                )?;
            } else if let Some(fed) = self.library.fed_like_by_content_id(&content_id)? {
                self.library
                    .upsert_fed_playlist_track(playlist_id, &fed, position)?;
            }
        } else {
            self.library
                .remove_content_id_from_synced_playlist(playlist_id, &content_id)?;
        }
        self.notify_library();
        Ok(())
    }

    pub(super) fn lww_wins(
        &self,
        table: &str,
        key_name: &str,
        key: &str,
        hlc_ms: i64,
        op_id: &str,
    ) -> Result<bool> {
        let sql = format!("SELECT hlc_ms, op_id FROM {table} WHERE {key_name} = ?1");
        let current = lock(&self.conn)
            .query_row(&sql, [key], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .optional()?;
        Ok(current.is_none_or(|(current_hlc, current_op)| {
            (hlc_ms, op_id) > (current_hlc, current_op.as_str())
        }))
    }

    pub(super) fn apply_playback_command(
        &self,
        target_device_id: &str,
        command: &PlaybackCommand,
        op_id: &str,
    ) -> Result<()> {
        if target_device_id != self.ensure_identity()?.device_id {
            return Ok(());
        }
        let inserted = lock(&self.conn).execute(
            "INSERT OR IGNORE INTO sync_playback_applied (op_id, applied_at_ms)
             VALUES (?1, ?2)",
            params![op_id, now_ms()],
        )?;
        if inserted > 0 {
            let _ = self
                .events
                .try_send(InternalEvent::DevicePlaybackCommand(command.clone()));
        }
        Ok(())
    }

    pub(super) fn apply_playback_snapshot(&self, snapshot: PlaybackSnapshot) {
        if self
            .ensure_identity()
            .is_ok_and(|identity| identity.device_id == snapshot.device_id)
        {
            return;
        }
        let changed = {
            let mut playback = lock(&self.playback);
            let changed = playback
                .remote
                .get(&snapshot.device_id)
                .is_none_or(|current| snapshot.updated_at_ms > current.updated_at_ms);
            if changed {
                playback
                    .remote
                    .insert(snapshot.device_id.clone(), snapshot.clone());
            }
            changed
        };
        if changed {
            let _ = self
                .events
                .try_send(InternalEvent::DevicePlaybackSnapshot(snapshot));
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the snapshot is one transactional projection of all synchronized entities"
    )]
    pub(super) fn snapshot(&self) -> Result<SyncSnapshot> {
        let conn = lock(&self.conn);
        let mut snapshot = SyncSnapshot::default();
        {
            let mut stmt =
                conn.prepare("SELECT content_id, liked, hlc_ms, op_id FROM sync_state_likes")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (content_id, liked, hlc_ms, op_id) = row?;
                if liked {
                    snapshot.likes.push(SnapshotLike {
                        content_id,
                        hlc_ms,
                        op_id,
                        fed: None,
                    });
                } else {
                    snapshot.unlikes.push(SnapshotLikeTombstone {
                        content_id,
                        hlc_ms,
                        op_id,
                    });
                }
            }
        }
        let mut playlists = BTreeMap::<String, SnapshotPlaylist>::new();
        {
            let mut stmt = conn.prepare(
                "SELECT playlist_id, title, deleted, hlc_ms, op_id
                 FROM sync_state_playlists",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? != 0,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                let (playlist_id, title, deleted, hlc_ms, op_id) = row?;
                if deleted {
                    snapshot.deleted_playlists.push(SnapshotPlaylistTombstone {
                        playlist_id,
                        hlc_ms,
                        op_id,
                    });
                } else {
                    playlists.insert(
                        playlist_id.clone(),
                        SnapshotPlaylist {
                            playlist_id,
                            title,
                            hlc_ms,
                            op_id,
                            items: Vec::new(),
                        },
                    );
                }
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT playlist_id, content_id, present, position, hlc_ms, op_id
                 FROM sync_state_playlist_items",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? != 0,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (playlist_id, content_id, present, position, hlc_ms, op_id) = row?;
                if present {
                    if let Some(playlist) = playlists.get_mut(&playlist_id) {
                        playlist.items.push(SnapshotPlaylistItem {
                            content_id,
                            position,
                            hlc_ms,
                            op_id,
                            fed: None,
                        });
                    }
                } else {
                    snapshot
                        .removed_playlist_items
                        .push(SnapshotPlaylistItemTombstone {
                            playlist_id,
                            content_id,
                            hlc_ms,
                            op_id,
                        });
                }
            }
        }
        snapshot.playlists = playlists.into_values().collect();
        Ok(snapshot)
    }

    pub(super) fn apply_snapshot(&self, snapshot: SyncSnapshot) -> Result<()> {
        for like in snapshot.likes {
            self.apply_like(
                &like.content_id,
                true,
                like.fed.as_ref(),
                like.hlc_ms,
                &like.op_id,
            )?;
        }
        for like in snapshot.unlikes {
            self.apply_like(&like.content_id, false, None, like.hlc_ms, &like.op_id)?;
        }
        for playlist in snapshot.playlists {
            self.apply_playlist(
                &playlist.playlist_id,
                &playlist.title,
                false,
                playlist.hlc_ms,
                &playlist.op_id,
            )?;
            for item in playlist.items {
                self.apply_playlist_item(
                    &playlist.playlist_id,
                    &item.content_id,
                    true,
                    item.position,
                    item.fed.as_ref(),
                    item.hlc_ms,
                    &item.op_id,
                )?;
            }
        }
        for playlist in snapshot.deleted_playlists {
            self.apply_playlist(
                &playlist.playlist_id,
                "",
                true,
                playlist.hlc_ms,
                &playlist.op_id,
            )?;
        }
        for item in snapshot.removed_playlist_items {
            self.apply_playlist_item(
                &item.playlist_id,
                &item.content_id,
                false,
                0,
                None,
                item.hlc_ms,
                &item.op_id,
            )?;
        }
        self.notify();
        Ok(())
    }

    pub(super) fn notify(&self) {
        let _ = self.events.try_send(InternalEvent::DevicesChanged);
    }

    pub(super) fn notify_library(&self) {
        let _ = self.events.try_send(InternalEvent::DeviceLibraryChanged);
    }
}
