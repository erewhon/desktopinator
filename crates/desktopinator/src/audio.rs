//! Audio forwarding via RDPSND (MS-RDPEA).
//!
//! Captures system audio via PipeWire and forwards PCM data to the RDP client
//! through the RDPSND static virtual channel.

use std::sync::{Arc, Mutex};
use std::thread;

use ironrdp_rdpsnd::pdu::{AudioFormat, ClientAudioFormatPdu, WaveFormat};
use ironrdp_rdpsnd::server::RdpsndServerMessage;
use ironrdp_server::{RdpsndServerHandler, ServerEvent, ServerEventSender, SoundServerFactory};
use pipewire::properties::properties;
use pipewire::spa::param::audio::AudioFormat as SpaAudioFormat;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::builder::{builder_add, Builder};
use pipewire::spa::utils::Id;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Shared state for audio capture.
struct CaptureState {
    event_tx: Option<mpsc::UnboundedSender<ServerEvent>>,
    stop_tx: Option<pipewire::channel::Sender<()>>,
    capture_thread: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for CaptureState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureState")
            .field("has_event_tx", &self.event_tx.is_some())
            .finish()
    }
}

/// Factory that creates RDPSND backends per connection.
#[derive(Debug)]
pub struct DinatorSoundFactory {
    state: Arc<Mutex<CaptureState>>,
}

impl DinatorSoundFactory {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureState {
                event_tx: None,
                stop_tx: None,
                capture_thread: None,
            })),
        }
    }
}

impl SoundServerFactory for DinatorSoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(DinatorAudioHandler {
            state: self.state.clone(),
            formats: supported_formats(),
        })
    }
}

impl ServerEventSender for DinatorSoundFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.state.lock().unwrap().event_tx = Some(sender);
    }
}

/// RDPSND handler that negotiates formats and manages capture.
#[derive(Debug)]
struct DinatorAudioHandler {
    state: Arc<Mutex<CaptureState>>,
    formats: Vec<AudioFormat>,
}

impl RdpsndServerHandler for DinatorAudioHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn start(&mut self, client_format: &ClientAudioFormatPdu) -> Option<u16> {
        // Find the first matching format
        let our_formats = supported_formats();
        let mut best_match: Option<(u16, &AudioFormat)> = None;

        for client_fmt in &client_format.formats {
            for (i, our_fmt) in our_formats.iter().enumerate() {
                if client_fmt.format == our_fmt.format
                    && client_fmt.n_channels == our_fmt.n_channels
                    && client_fmt.n_samples_per_sec == our_fmt.n_samples_per_sec
                    && client_fmt.bits_per_sample == our_fmt.bits_per_sample
                {
                    if best_match.is_none() {
                        best_match = Some((i as u16, our_fmt));
                    }
                }
            }
        }

        let (format_no, matched) = best_match?;

        info!(
            format = ?matched.format,
            rate = matched.n_samples_per_sec,
            channels = matched.n_channels,
            bits = matched.bits_per_sample,
            format_no,
            "RDPSND: negotiated audio format"
        );

        let mut state = self.state.lock().unwrap();

        if let Some(ref event_tx) = state.event_tx {
            let event_tx = event_tx.clone();
            let sample_rate = matched.n_samples_per_sec;
            let channels = matched.n_channels;

            let (pw_sender, pw_receiver) = pipewire::channel::channel::<()>();
            state.stop_tx = Some(pw_sender);

            let handle = thread::Builder::new()
                .name("pw-audio-capture".into())
                .spawn(move || {
                    run_pipewire_capture(event_tx, pw_receiver, sample_rate, channels);
                })
                .expect("spawn audio capture thread");

            state.capture_thread = Some(handle);
        } else {
            warn!("RDPSND: no event sender available, audio will not be captured");
        }

        Some(format_no)
    }

    fn stop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if let Some(stop_tx) = state.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = state.capture_thread.take() {
            let _ = handle.join();
        }
        info!("RDPSND: audio capture stopped");
    }
}

/// Supported audio formats we advertise to the RDP client.
fn supported_formats() -> Vec<AudioFormat> {
    vec![
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: 44100,
            n_avg_bytes_per_sec: 44100 * 2 * 2,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        },
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: 48000,
            n_avg_bytes_per_sec: 48000 * 2 * 2,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        },
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: 22050,
            n_avg_bytes_per_sec: 22050 * 2 * 2,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        },
    ]
}

/// Run PipeWire main loop to capture audio from the default monitor source.
fn run_pipewire_capture(
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    stop_rx: pipewire::channel::Receiver<()>,
    sample_rate: u32,
    channels: u16,
) {
    pipewire::init();

    let mainloop = match pipewire::main_loop::MainLoopRc::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            error!(error = %e, "RDPSND: failed to create PipeWire main loop");
            return;
        }
    };

    // Attach stop signal to quit the main loop
    let mainloop_weak = mainloop.downgrade();
    let _stop_listener = stop_rx.attach(mainloop.loop_(), move |_| {
        info!("RDPSND: stop signal received, quitting PipeWire loop");
        if let Some(ml) = mainloop_weak.upgrade() {
            ml.quit();
        }
    });

    let context = match pipewire::context::ContextRc::new(&mainloop, None) {
        Ok(ctx) => ctx,
        Err(e) => {
            error!(error = %e, "RDPSND: failed to create PipeWire context");
            return;
        }
    };

    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "RDPSND: failed to connect to PipeWire");
            return;
        }
    };

    let props = properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Music",
        *pipewire::keys::NODE_NAME => "desktopinator-capture",
        // Capture from the default audio output's monitor
        *pipewire::keys::STREAM_CAPTURE_SINK => "true",
    };

    let stream = match pipewire::stream::StreamRc::new(core, "desktopinator-audio-capture", props) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "RDPSND: failed to create PipeWire stream");
            return;
        }
    };

    let start_time = std::time::Instant::now();
    let frame_size = channels as u32 * 2; // S16LE = 2 bytes per sample per channel
    // Send ~20ms chunks
    let chunk_bytes = (sample_rate / 50) * frame_size;

    let _listener = stream
        .add_local_listener_with_user_data(Vec::<u8>::new())
        .process(move |stream: &pipewire::stream::Stream, accumulator: &mut Vec<u8>| {
            match stream.dequeue_buffer() {
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    if let Some(data) = datas.first_mut() {
                        let chunk_offset = data.chunk().offset() as usize;
                        let chunk_size = data.chunk().size() as usize;

                        if chunk_size > 0 {
                            if let Some(raw) = data.data() {
                                let end = (chunk_offset + chunk_size).min(raw.len());
                                if chunk_offset < end {
                                    accumulator.extend_from_slice(&raw[chunk_offset..end]);
                                }
                            }
                        }

                        // Send complete chunks
                        while accumulator.len() >= chunk_bytes as usize {
                            let audio_chunk: Vec<u8> =
                                accumulator.drain(..chunk_bytes as usize).collect();
                            let ts = start_time.elapsed().as_millis() as u32;
                            if event_tx
                                .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                                    audio_chunk, ts,
                                )))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                None => {
                    debug!("RDPSND: no buffer available from PipeWire");
                }
            }
        })
        .register()
        .expect("register PipeWire stream listener");

    // Build audio format pod
    let mut params_buf = Vec::<u8>::new();
    let pod = build_audio_format_pod(&mut params_buf, sample_rate, channels);

    let flags = pipewire::stream::StreamFlags::AUTOCONNECT
        | pipewire::stream::StreamFlags::MAP_BUFFERS
        | pipewire::stream::StreamFlags::RT_PROCESS;

    if let Err(e) = stream.connect(
        pipewire::spa::utils::Direction::Input,
        None,
        flags,
        &mut [pod],
    ) {
        error!(error = %e, "RDPSND: failed to connect PipeWire stream");
        return;
    }

    info!(
        sample_rate,
        channels,
        "RDPSND: PipeWire audio capture started"
    );

    mainloop.run();
    info!("RDPSND: PipeWire audio capture loop ended");
}

/// Build a SPA pod describing the desired audio format (S16LE PCM).
fn build_audio_format_pod<'a>(
    buffer: &'a mut Vec<u8>,
    sample_rate: u32,
    channels: u16,
) -> &'a pipewire::spa::pod::Pod {
    let mut builder = Builder::new(buffer);
    builder_add!(
        &mut builder,
        Object(
            ParamType::EnumFormat.as_raw(),
            0,
        ) {
            FormatProperties::MediaType.as_raw() => Id(Id(MediaType::Audio.as_raw())),
            FormatProperties::MediaSubtype.as_raw() => Id(Id(MediaSubtype::Raw.as_raw())),
            FormatProperties::AudioFormat.as_raw() => Id(Id(SpaAudioFormat::S16LE.as_raw())),
            FormatProperties::AudioRate.as_raw() => Int(sample_rate as i32),
            FormatProperties::AudioChannels.as_raw() => Int(channels as i32),
        }
    )
    .expect("build audio format pod");

    unsafe {
        let pod_ptr = buffer.as_ptr() as *const pipewire::spa::sys::spa_pod;
        &*(pod_ptr as *const pipewire::spa::pod::Pod)
    }
}
