use super::{
    Actor, ArtistKey, ArtistRef, AudioSource, ConnectedDeviceSnapshot, ConnectedDevicesSnapshot,
    ContentId, ControlPlaybackAnchor, DeviceOperationResult, DevicePlaybackRole, DevicePresence,
    DeviceTrust, Duration, Instant, InternalEvent, PendingControlState, PendingPairingSnapshot,
    PlaybackStatus, ReleaseKey, Track, TrackKey, library_track, normalize_device_name,
    portable_playback_placeholder, track_to_playback_track, unix_time_ms, volume_percent,
};

impl Actor {
    pub(super) fn create_device_invite(&mut self) {
        let Some(service) = self.device_service.clone() else {
            self.state.connected_devices.error =
                Some("Federation network is still starting".into());
            self.publish();
            return;
        };
        self.state.connected_devices.busy = true;
        self.state.connected_devices.error = None;
        self.publish();
        let devices = std::sync::Arc::clone(&self.devices);
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = devices
                .create_invite(service)
                .await
                .map(DeviceOperationResult::Invite)
                .map_err(|error| format!("{error:#}"));
            let _ = internal
                .send(InternalEvent::DeviceOperationFinished(result))
                .await;
        });
    }

    pub(super) fn connect_device(&mut self, invite: String) {
        let Some(service) = self.device_service.clone() else {
            self.state.connected_devices.error =
                Some("Federation network is still starting".into());
            self.publish();
            return;
        };
        self.state.connected_devices.busy = true;
        self.state.connected_devices.error = None;
        self.publish();
        let devices = std::sync::Arc::clone(&self.devices);
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = devices
                .connect_invite(service, &invite)
                .await
                .map(DeviceOperationResult::Connected)
                .map_err(|error| format!("{error:#}"));
            let _ = internal
                .send(InternalEvent::DeviceOperationFinished(result))
                .await;
        });
    }

    pub(super) fn apply_local_device_name(&mut self, name: &str) {
        let name = normalize_device_name(name);
        if let Err(error) = self.devices.set_device_name(&name, None) {
            self.state.connected_devices.error = Some(format!("device name: {error:#}"));
            return;
        }
        if self.active_device_id == self.state.connected_devices.this_device_id {
            self.active_device_name = name;
        }
        self.refresh_connected_devices();
    }

    pub(super) fn schedule_device_name_publish(&self, candidate: String) {
        let internal = self.internal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = internal
                .send(InternalEvent::DeviceNamePublishDue(candidate))
                .await;
        });
    }

    pub(super) fn publish_device_name(&self, name: String) {
        let Some(service) = self.device_service.clone() else {
            return;
        };
        let devices = std::sync::Arc::clone(&self.devices);
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = async {
                let ticket = service
                    .ticket()
                    .await
                    .map_err(|error| format!("device name: {error:#}"))?;
                devices
                    .set_device_name(&name, Some(&ticket.to_string()))
                    .map_err(|error| format!("device name: {error:#}"))
            }
            .await;
            let _ = internal
                .send(InternalEvent::DeviceNamePublished(result))
                .await;
        });
    }

    pub(super) fn refresh_connected_devices(&mut self) {
        let status = self.devices.status();
        let invite = self.state.connected_devices.invite.take();
        let busy = self.state.connected_devices.busy;
        let existing_error = self.state.connected_devices.error.take();
        self.state.connected_devices = ConnectedDevicesSnapshot {
            this_device_id: status.this_device_id,
            this_device_name: status.this_device_name,
            group_id: status.group_id,
            role: self.device_role,
            active_device_id: self.active_device_id.clone(),
            active_device_name: self.active_device_name.clone(),
            devices: status
                .devices
                .into_iter()
                .filter(|device| !device.revoked)
                .map(|device| ConnectedDeviceSnapshot {
                    is_active: device.id == self.active_device_id,
                    id: device.id,
                    name: device.name,
                    client_version: device.client_version,
                    is_self: device.is_self,
                    presence: if device.online {
                        DevicePresence::Online
                    } else {
                        DevicePresence::Offline
                    },
                    trust: if device.revoked {
                        DeviceTrust::Revoked
                    } else {
                        DeviceTrust::Trusted
                    },
                })
                .collect(),
            pending_pairings: status
                .pending
                .into_iter()
                .map(|pending| PendingPairingSnapshot {
                    request_id: pending.request_id,
                    device_id: pending.device_id,
                    name: pending.name,
                    client_version: pending.client_version,
                    requester_group_id: pending.requester_group_id,
                    requester_group_active_devices: pending.requester_group_active_devices,
                })
                .collect(),
            invite,
            busy,
            last_sync: status.last_sync,
            error: existing_error.or(status.error),
        };
    }

    pub(super) fn select_playback_device(&mut self, device_id: &str) {
        // Selecting the device that already owns playback must not rebuild or
        // restart the current audio stream.
        if device_id == self.active_device_id {
            return;
        }
        let target = self
            .state
            .connected_devices
            .devices
            .iter()
            .find(|device| device.id == device_id && device.trust == DeviceTrust::Trusted)
            .cloned();
        let Some(target) = target else {
            return;
        };
        let wire = self.playback_wire_state();
        let previous = self.active_device_id.clone();
        let mut urgent_targets = Vec::with_capacity(2);
        if target.is_self {
            if previous != target.id {
                let command = music_dht::device_sync::PlaybackCommand::ActiveChanged {
                    active_device_id: target.id.clone(),
                    active_device_name: target.name.clone(),
                    state: wire.clone(),
                };
                if let Err(error) = self.devices.record_playback_command(&previous, command) {
                    self.state.connected_devices.error = Some(format!("device handoff: {error:#}"));
                    self.publish();
                    return;
                }
                urgent_targets.push(previous.clone());
            }
            self.device_role = DevicePlaybackRole::Active;
            self.active_device_id = target.id;
            self.active_device_name = target.name;
            self.control_anchor = None;
            self.pending_control = None;
            if self.state.playback.status == PlaybackStatus::Playing {
                self.play_current();
            }
        } else {
            let command = music_dht::device_sync::PlaybackCommand::ActiveChanged {
                active_device_id: target.id.clone(),
                active_device_name: target.name.clone(),
                state: wire.clone(),
            };
            if let Err(error) = self
                .devices
                .record_playback_command(&target.id, command.clone())
            {
                self.state.connected_devices.error = Some(format!("device handoff: {error:#}"));
                self.publish();
                return;
            }
            urgent_targets.push(target.id.clone());
            if previous != target.id && previous != self.state.connected_devices.this_device_id {
                if let Err(error) = self.devices.record_playback_command(&previous, command) {
                    self.state.connected_devices.error =
                        Some(format!("previous device handoff: {error:#}"));
                }
                urgent_targets.push(previous);
            }
            self.audio.stop();
            self.device_role = DevicePlaybackRole::Control;
            self.active_device_id.clone_from(&target.id);
            self.active_device_name = target.name;
            self.control_anchor = Some(ControlPlaybackAnchor {
                device_id: target.id.clone(),
                state: wire.clone(),
                observed_at: Instant::now(),
            });
            self.pending_control = Some(PendingControlState {
                device_id: target.id,
                state: wire,
                seek: true,
                sent_at: Instant::now(),
            });
        }
        self.devices.request_sync();
        if let Some(service) = self.device_service.clone() {
            urgent_targets.sort();
            urgent_targets.dedup();
            for target_id in urgent_targets {
                let devices = std::sync::Arc::clone(&self.devices);
                let service = std::sync::Arc::clone(&service);
                tokio::spawn(async move {
                    let _ = devices.sync_target(service, &target_id).await;
                });
            }
        }
        self.refresh_connected_devices();
        self.publish_device_playback();
        self.publish();
    }

    pub(super) fn playback_wire_state(&self) -> music_dht::device_sync::PlaybackStateWire {
        music_dht::device_sync::PlaybackStateWire {
            queue: self
                .state
                .queue
                .items()
                .iter()
                .map(|item| track_to_playback_track(&item.track))
                .collect(),
            queue_pos: self.state.queue.current_index().unwrap_or(0),
            playing: self.state.playback.status != PlaybackStatus::Stopped,
            paused: self.state.playback.status == PlaybackStatus::Paused,
            idle_since_ms: (self.state.playback.status != PlaybackStatus::Playing)
                .then_some(unix_time_ms()),
            position_secs: self.state.playback.position_seconds,
            volume: volume_percent(self.state.playback.volume),
            shuffle: self.state.playback.shuffle,
            repeat: repeat_to_wire(self.state.playback.repeat),
        }
    }

    pub(super) fn publish_device_playback(&self) {
        let snapshot = music_dht::device_sync::PlaybackSnapshot {
            device_id: self.state.connected_devices.this_device_id.clone(),
            device_name: self.state.connected_devices.this_device_name.clone(),
            active: self.device_role == DevicePlaybackRole::Active,
            updated_at_ms: unix_time_ms(),
            state: self.playback_wire_state(),
        };
        self.devices.publish_playback(snapshot);
    }

    pub(super) fn send_control_state(&mut self, seek: bool) {
        if self.device_role != DevicePlaybackRole::Control {
            return;
        }
        let state = self.playback_wire_state();
        let command = music_dht::device_sync::PlaybackCommand::SetState {
            state: state.clone(),
            seek,
        };
        if let Err(error) = self
            .devices
            .record_playback_command(&self.active_device_id, command)
        {
            self.state.connected_devices.error = Some(format!("device control: {error:#}"));
        } else {
            self.pending_control = Some(PendingControlState {
                device_id: self.active_device_id.clone(),
                state,
                seek,
                sent_at: Instant::now(),
            });
        }
    }

    pub(super) fn apply_device_playback_state(
        &mut self,
        wire: &music_dht::device_sync::PlaybackStateWire,
        start_audio: bool,
        seek: bool,
    ) {
        let previous_status = self.state.playback.status;
        let previous_index = self.state.queue.current_index();
        let previous_track = self
            .state
            .queue
            .current()
            .map(|item| item.track.key.clone());
        let tracks = wire
            .queue
            .iter()
            .filter_map(|track| self.resolve_playback_track(track))
            .collect::<Vec<_>>();
        if !tracks.is_empty() {
            if wire.shuffle {
                if !self.state.playback.shuffle {
                    self.state.queue.remember_order();
                }
                self.state
                    .queue
                    .replace_shuffled_context(tracks, wire.queue_pos);
            } else {
                self.state.queue.replace_context(tracks, wire.queue_pos);
            }
            self.resolve_queue_artwork();
        }
        let current_changed = previous_index != self.state.queue.current_index()
            || previous_track
                .as_ref()
                .zip(self.state.queue.current().map(|item| &item.track.key))
                .is_none_or(|(previous, current)| !previous.matches(current));
        self.state.playback.volume = f32::from(wire.volume.min(100)) / 100.0;
        self.state.playback.shuffle = wire.shuffle;
        self.state.playback.repeat = repeat_from_wire(wire.repeat);
        self.state.playback.position_seconds = wire.position_secs.max(0.0);
        self.state.playback.duration_seconds = self
            .state
            .queue
            .current()
            .map_or(0.0, |item| item.track.duration_seconds);
        self.state.playback.status = if !wire.playing {
            PlaybackStatus::Stopped
        } else if wire.paused {
            PlaybackStatus::Paused
        } else {
            PlaybackStatus::Playing
        };
        self.audio.set_volume(self.state.playback.volume);
        if !start_audio {
            return;
        }
        if !wire.playing {
            self.audio.stop();
        } else if previous_status == PlaybackStatus::Stopped || current_changed {
            self.play_current();
            if seek {
                self.audio
                    .seek(Duration::from_secs_f64(wire.position_secs.max(0.0)));
                self.state.playback.position_seconds = wire.position_secs.max(0.0);
            }
            if wire.paused {
                self.audio.pause();
                self.state.playback.status = PlaybackStatus::Paused;
            }
        } else if wire.paused {
            self.audio.pause();
        } else if previous_status == PlaybackStatus::Paused {
            self.audio.resume();
        } else if seek {
            self.audio
                .seek(Duration::from_secs_f64(wire.position_secs.max(0.0)));
        }
    }

    pub(super) fn apply_device_playback_command(
        &mut self,
        command: music_dht::device_sync::PlaybackCommand,
    ) {
        match command {
            music_dht::device_sync::PlaybackCommand::SetState { state, seek } => {
                self.device_role = DevicePlaybackRole::Active;
                self.active_device_id = self.state.connected_devices.this_device_id.clone();
                self.active_device_name = self.state.connected_devices.this_device_name.clone();
                self.control_anchor = None;
                self.pending_control = None;
                self.apply_device_playback_state(&state, true, seek);
            }
            music_dht::device_sync::PlaybackCommand::ActiveChanged {
                active_device_id,
                active_device_name,
                state,
            } => {
                let is_self = active_device_id == self.state.connected_devices.this_device_id;
                self.active_device_id = active_device_id;
                self.active_device_name = active_device_name;
                self.device_role = if is_self {
                    DevicePlaybackRole::Active
                } else {
                    DevicePlaybackRole::Control
                };
                self.pending_control = None;
                self.control_anchor = (!is_self).then(|| ControlPlaybackAnchor {
                    device_id: self.active_device_id.clone(),
                    state: state.clone(),
                    observed_at: Instant::now(),
                });
                if !is_self {
                    self.audio.stop();
                }
                self.apply_device_playback_state(&state, is_self, true);
            }
        }
        self.refresh_connected_devices();
        self.publish();
    }

    pub(super) fn resolve_playback_track(
        &self,
        wire: &music_dht::device_sync::PlaybackTrack,
    ) -> Option<Track> {
        if let Some(content_id) = wire
            .content_id
            .as_deref()
            .and_then(|id| ContentId::parse(id).ok())
        {
            let key = TrackKey::remote(content_id.clone());
            if let Some(track) = self.track(&key) {
                return Some(track.clone());
            }
            if let Ok(Some(track)) = self.catalog.track_by_content_id(content_id.as_str()) {
                return Some(library_track(track, ""));
            }
        }
        let Some(fed) = wire.fed.as_ref() else {
            let content_id = wire
                .content_id
                .as_deref()
                .and_then(|id| ContentId::parse(id).ok())?;
            return Some(portable_playback_placeholder(wire, content_id));
        };
        let content_id = ContentId::parse(&fed.content_id).ok()?;
        let release_key = ReleaseKey::Federation {
            peer_id: fed.owner.clone(),
            id: format!(
                "name:{}",
                music_dht::normalize_name(fed.release_title.as_deref().unwrap_or_default())
            ),
        };
        let refs = |names: &[String]| {
            names
                .iter()
                .map(|name| ArtistRef {
                    key: ArtistKey::Federation {
                        peer_id: fed.owner.clone(),
                        id: format!("name:{}", music_dht::normalize_name(name)),
                    },
                    name: name.clone(),
                })
                .collect::<Vec<_>>()
        };
        Some(Track {
            key: TrackKey::federation(
                fed.owner.clone(),
                fed.item_id.clone(),
                Some(content_id.clone()),
            ),
            title: wire.title.clone(),
            artist: wire.artist_names.join(", "),
            artists: refs(&wire.artist_names),
            featured_artists: refs(&wire.featured_artist_names),
            release: wire.release_title.clone(),
            release_id: release_key,
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
                peer_id: fed.owner.clone(),
                content_id,
            },
        })
    }
}

fn repeat_to_wire(
    repeat: furumi_backend_api::PlaybackRepeat,
) -> music_dht::device_sync::PlaybackRepeat {
    match repeat {
        furumi_backend_api::PlaybackRepeat::Off => music_dht::device_sync::PlaybackRepeat::Off,
        furumi_backend_api::PlaybackRepeat::One => music_dht::device_sync::PlaybackRepeat::One,
        furumi_backend_api::PlaybackRepeat::All => music_dht::device_sync::PlaybackRepeat::All,
    }
}

fn repeat_from_wire(
    repeat: music_dht::device_sync::PlaybackRepeat,
) -> furumi_backend_api::PlaybackRepeat {
    match repeat {
        music_dht::device_sync::PlaybackRepeat::Off => furumi_backend_api::PlaybackRepeat::Off,
        music_dht::device_sync::PlaybackRepeat::One => furumi_backend_api::PlaybackRepeat::One,
        music_dht::device_sync::PlaybackRepeat::All => furumi_backend_api::PlaybackRepeat::All,
    }
}
