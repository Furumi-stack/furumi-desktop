//! Real audio output isolated from the backend actor and UI thread.

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, Player, stream::MixerDeviceSink};

pub trait TrackReadSeek: std::io::Read + std::io::Seek + Send + Sync {}
impl<T> TrackReadSeek for T where T: std::io::Read + std::io::Seek + Send + Sync {}
pub type TrackReader = Box<dyn TrackReadSeek>;

#[derive(Debug)]
pub enum Event {
    Started,
    Finished,
    Failed(String),
}

enum Command {
    Play {
        path: PathBuf,
        volume: f32,
    },
    PlayStream {
        reader: TrackReader,
        mime_type: String,
        volume: f32,
    },
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
}

#[derive(Debug, Default)]
pub struct Shared {
    position_ms: AtomicU64,
}

impl Shared {
    pub fn position_seconds(&self) -> f64 {
        Duration::from_millis(self.position_ms.load(Ordering::Relaxed)).as_secs_f64()
    }
}

#[derive(Clone)]
pub struct Controller {
    commands: Sender<Command>,
    pub shared: Arc<Shared>,
}

impl Controller {
    pub fn play(&self, path: PathBuf, volume: f32) {
        self.shared.position_ms.store(0, Ordering::Relaxed);
        let _ = self.commands.send(Command::Play { path, volume });
    }
    pub fn play_stream(&self, reader: TrackReader, mime_type: String, volume: f32) {
        self.shared.position_ms.store(0, Ordering::Relaxed);
        let _ = self.commands.send(Command::PlayStream {
            reader,
            mime_type,
            volume,
        });
    }

    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    pub fn resume(&self) {
        let _ = self.commands.send(Command::Resume);
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    pub fn seek(&self, position: Duration) {
        let millis = u64::try_from(position.as_millis()).unwrap_or(u64::MAX);
        self.shared.position_ms.store(millis, Ordering::Relaxed);
        let _ = self.commands.send(Command::Seek(position));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.commands.send(Command::SetVolume(volume));
    }
}

pub fn spawn(on_event: impl Fn(Event) + Send + 'static) -> io::Result<Controller> {
    let (commands, receiver) = std::sync::mpsc::channel();
    let shared = Arc::new(Shared::default());
    let thread_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("furumi-audio".into())
        .spawn(move || run(&receiver, &thread_shared, &on_event))?;
    Ok(Controller { commands, shared })
}

struct Output {
    _device: MixerDeviceSink,
    player: Player,
}

fn run(receiver: &Receiver<Command>, shared: &Arc<Shared>, on_event: &impl Fn(Event)) {
    let mut output: Option<Output> = None;
    let mut track_loaded = false;
    let mut previous_queue_len = 0;

    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(command) => {
                handle(command, shared, &mut output, &mut track_loaded, on_event);
                previous_queue_len = output.as_ref().map_or(0, |output| output.player.len());
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(output) = &output {
                    let millis =
                        u64::try_from(output.player.get_pos().as_millis()).unwrap_or(u64::MAX);
                    shared.position_ms.store(millis, Ordering::Relaxed);
                    let queue_len = output.player.len();
                    if track_loaded && queue_len < previous_queue_len {
                        track_loaded = false;
                        on_event(Event::Finished);
                    }
                    previous_queue_len = queue_len;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[allow(clippy::too_many_lines, reason = "exhaustive audio command dispatcher")]
fn handle(
    command: Command,
    shared: &Arc<Shared>,
    output: &mut Option<Output>,
    track_loaded: &mut bool,
    on_event: &impl Fn(Event),
) {
    match command {
        Command::Play { path, volume } => {
            let output = match ensure_output(output) {
                Ok(output) => output,
                Err(error) => {
                    on_event(Event::Failed(format!("cannot open audio output: {error}")));
                    return;
                }
            };
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(error) => {
                    on_event(Event::Failed(format!(
                        "cannot open {}: {error}",
                        path.display()
                    )));
                    return;
                }
            };
            let byte_len = file.metadata().ok().map(|metadata| metadata.len());
            let mut decoder = Decoder::builder()
                .with_data(file)
                .with_seekable(true)
                .with_gapless(true);
            if let Some(byte_len) = byte_len {
                decoder = decoder.with_byte_len(byte_len);
            }
            match decoder.build() {
                Ok(source) => {
                    output.player.stop();
                    output.player.set_volume(amplitude(volume));
                    output.player.append(source);
                    output.player.play();
                    shared.position_ms.store(0, Ordering::Relaxed);
                    *track_loaded = true;
                    on_event(Event::Started);
                }
                Err(error) => on_event(Event::Failed(format!(
                    "cannot decode {}: {error}",
                    path.display()
                ))),
            }
        }
        Command::PlayStream {
            reader,
            mime_type,
            volume,
        } => {
            let output = match ensure_output(output) {
                Ok(output) => output,
                Err(error) => {
                    on_event(Event::Failed(format!("cannot open audio output: {error}")));
                    return;
                }
            };
            match Decoder::builder()
                .with_data(reader)
                .with_mime_type(&mime_type)
                .with_seekable(false)
                .with_gapless(true)
                .build()
            {
                Ok(source) => {
                    output.player.stop();
                    output.player.set_volume(amplitude(volume));
                    output.player.append(source);
                    output.player.play();
                    shared.position_ms.store(0, Ordering::Relaxed);
                    *track_loaded = true;
                    on_event(Event::Started);
                }
                Err(error) => on_event(Event::Failed(format!(
                    "cannot decode federated stream: {error}"
                ))),
            }
        }
        Command::Pause => {
            if let Some(output) = output {
                output.player.pause();
            }
        }
        Command::Resume => {
            if let Some(output) = output {
                output.player.play();
            }
        }
        Command::Stop => {
            if let Some(output) = output {
                output.player.stop();
            }
            shared.position_ms.store(0, Ordering::Relaxed);
            *track_loaded = false;
        }
        Command::Seek(position) => {
            if let Some(output) = output
                && let Err(error) = output.player.try_seek(position)
            {
                on_event(Event::Failed(format!("cannot seek: {error}")));
            }
        }
        Command::SetVolume(volume) => {
            if let Some(output) = output {
                output.player.set_volume(amplitude(volume));
            }
        }
    }
}

fn ensure_output(output: &mut Option<Output>) -> Result<&Output, rodio::stream::DeviceSinkError> {
    if output.is_none() {
        let device = DeviceSinkBuilder::open_default_sink()?;
        let player = Player::connect_new(device.mixer());
        *output = Some(Output {
            _device: device,
            player,
        });
    }
    Ok(output.as_ref().expect("audio output initialized above"))
}

fn amplitude(volume: f32) -> f32 {
    volume.clamp(0.0, 1.0).powi(3)
}

#[cfg(test)]
mod tests {
    use super::amplitude;

    #[test]
    fn perceptual_volume_is_clamped_and_cubic() {
        assert!(amplitude(-1.0).abs() < f32::EPSILON);
        assert!((amplitude(0.5) - 0.125).abs() < f32::EPSILON);
        assert!((amplitude(2.0) - 1.0).abs() < f32::EPSILON);
    }
}
