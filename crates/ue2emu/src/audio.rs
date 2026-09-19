//! SID audio out: `--audio on|off` through cpal and `--audio-wav <path>`. Spec: docs/specs/S14-c64-trx64.md §W4-SID;
//! status: docs/status/sid-audio.md.
//!
//! The emulation thread hands every block of mono samples reSID produced to a [`Sink`] (c64-bridge `AudioSink`), in
//! emulated-time order. The sink writes them to the WAV file unchanged and pushes them into a small [`Ring`] the cpal
//! callback plays. The ring follows emulated time and never blocks the emulator: past [`MAX_MS`] of backlog
//! (`--speed max`, a stalled device) the oldest samples are dropped down to [`PREROLL_MS`]; when it runs dry the
//! callback plays silence. The cpal stream lives on the thread that started it ([`Output`], held by `EmuHandle`).
//!
//! Without the `trx64` feature there is no SID to feed the sink (runner.rs starts audio only with the TRX64 C64).
#![cfg_attr(not(feature = "trx64"), allow(dead_code))]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// WAV sample rate without an audio device.
const WAV_RATE: u32 = 44_100;
/// Device rates tried after the device's own default, when that is neither.
const DEVICE_RATES: [u32; 2] = [48_000, 44_100];
/// Backlog the callback waits for before it starts playing.
const PREROLL_MS: usize = 60;
/// Backlog above which the oldest samples are dropped down to `PREROLL_MS`.
const MAX_MS: usize = 150;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Args)]
pub struct AudioArgs {
    /// SID audio through the default output device (needs --c64 trx64) [default: on with a window, off with --headless]
    #[arg(long, value_enum)]
    audio: Option<OnOff>,
    /// Write the SID sample stream to a WAV file: mono, 16 bit, the device rate with audio on, else 44100 Hz
    #[arg(long, value_name = "PATH")]
    audio_wav: Option<PathBuf>,
    /// What SID socket 1 holds (needs --c64 trx64). The firmware detects an ARMSID at boot; on a flash that has not
    /// saved it yet it asks to review the settings in the menu [default: none]
    #[arg(long, value_enum, value_name = "CHIP")]
    sid_socket1: Option<Socket>,
    /// Run the reSID engines on the emulation thread instead of their own, also when an audio device listens
    /// (docs/specs/S20-sid-thread.md)
    #[arg(long)]
    no_sid_thread: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Socket {
    None,
    Armsid,
}

/// What `--audio`, `--audio-wav` and `--sid-socket1` ask for.
#[derive(Clone, Debug, Default)]
pub struct AudioOptions {
    pub device: bool,
    pub wav: Option<PathBuf>,
    /// An ARMSID in SID socket 1.
    pub armsid: bool,
    /// Keep the reSID engines on the emulation thread even with a device (`--no-sid-thread`, S20 §5).
    pub no_sid_thread: bool,
}

pub fn configure(args: AudioArgs, headless: bool) -> AudioOptions {
    AudioOptions {
        device: args.audio.map_or(!headless, |a| a == OnOff::On),
        wav: args.audio_wav,
        armsid: args.sid_socket1 == Some(Socket::Armsid),
        no_sid_thread: args.no_sid_thread,
    }
}

/// The running device stream, if any; dropping it stops playback.
pub struct Output {
    _stream: Option<cpal::Stream>,
}

impl Output {
    pub fn none() -> Self {
        Output { _stream: None }
    }
}

/// Open what `opts` asks for. No device is a warning (the emulator runs muted); an uncreatable WAV file is an error.
pub fn start(opts: &AudioOptions) -> Result<(Output, Option<Sink>)> {
    let device = if opts.device {
        open_device().map_err(|e| eprintln!("audio: {e:#}; running without the audio device")).ok()
    } else {
        None
    };
    let (stream, ring, rate) = match device {
        Some((stream, ring, rate)) => (Some(stream), Some(ring), rate),
        None => (None, None, WAV_RATE),
    };
    let wav = opts.wav.as_deref().map(|path| Wav::create(path, rate)).transpose()?;
    let sink = (ring.is_some() || wav.is_some()).then_some(Sink { rate, ring, wav });
    Ok((Output { _stream: stream }, sink))
}

/// The emulation thread's end: WAV file and ring.
pub struct Sink {
    rate: u32,
    ring: Option<Arc<Ring>>,
    wav: Option<Wav>,
}

impl Sink {
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Whether an audio device listens, as opposed to a WAV file alone. Only then do the engines get their own
    /// thread (S20 §5).
    pub fn has_device(&self) -> bool {
        self.ring.is_some()
    }

    pub fn push(&mut self, pcm: &[i16]) {
        if let Some(wav) = &mut self.wav {
            wav.push(pcm);
        }
        if let Some(ring) = &self.ring {
            ring.push(pcm);
        }
    }
}

#[cfg(feature = "trx64")]
impl c64_bridge::AudioSink for Sink {
    fn samples(&mut self, pcm: &[i16]) {
        self.push(pcm);
    }
}

/// The default output device at its own rate when that is 44.1 or 48 kHz, else at 48 or 44.1 kHz, else at its own rate
/// whatever it is (WASAPI in shared mode takes only the device's mix rate, often 96 or 192 kHz; the engines render at
/// any rate); mono is copied to every channel.
fn open_device() -> Result<(cpal::Stream, Arc<Ring>, u32)> {
    let device = cpal::default_host().default_output_device().ok_or_else(|| anyhow!("no default output device"))?;
    let default = device.default_output_config().context("no default output config")?;
    let mut rates: Vec<u32> = Vec::new();
    let own = default.sample_rate().0;
    for rate in std::iter::once(own).filter(|r| DEVICE_RATES.contains(r)).chain(DEVICE_RATES).chain([own]) {
        if !rates.contains(&rate) {
            rates.push(rate);
        }
    }
    let mut last = anyhow!("no sample rate");
    for rate in rates {
        let config = cpal::StreamConfig {
            channels: default.channels(),
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let ring = Arc::new(Ring::new(rate));
        match build_stream(&device, &config, default.sample_format(), Arc::clone(&ring)) {
            Ok(stream) => return Ok((stream, ring, rate)),
            Err(e) => last = e.context(format!("{rate} Hz")),
        }
    }
    Err(last)
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    ring: Arc<Ring>,
) -> Result<cpal::Stream> {
    let channels = usize::from(config.channels.max(1));
    let error = |e| eprintln!("audio: stream error: {e}");
    let stream = match format {
        cpal::SampleFormat::I16 => {
            device.build_output_stream(config, move |out: &mut [i16], _: &_| ring.fill(out, channels), error, None)
        }
        cpal::SampleFormat::F32 => {
            device.build_output_stream(config, move |out: &mut [f32], _: &_| ring.fill(out, channels), error, None)
        }
        cpal::SampleFormat::U16 => {
            device.build_output_stream(config, move |out: &mut [u16], _: &_| ring.fill(out, channels), error, None)
        }
        other => bail!("unsupported sample format {other}"),
    }?;
    stream.play()?;
    Ok(stream)
}

/// Mono samples between the emulation thread and the device callback.
struct Ring {
    state: Mutex<RingState>,
    preroll: usize,
    max: usize,
}

#[derive(Default)]
struct RingState {
    buf: VecDeque<i16>,
    /// Set once the pre-roll is reached; an underrun plays silence without waiting for a new pre-roll.
    primed: bool,
}

impl Ring {
    fn new(rate: u32) -> Self {
        let per_ms = rate as usize / 1000;
        Ring {
            state: Mutex::new(RingState { buf: VecDeque::with_capacity(per_ms * MAX_MS * 2), primed: false }),
            preroll: per_ms * PREROLL_MS,
            max: per_ms * MAX_MS,
        }
    }

    /// Append, dropping the oldest samples down to the pre-roll past the maximum: never waits.
    fn push(&self, pcm: &[i16]) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.buf.extend(pcm);
        if s.buf.len() > self.max {
            let excess = s.buf.len() - self.preroll;
            s.buf.drain(..excess);
        }
    }

    /// Fill an interleaved device buffer; silence before the pre-roll and on underrun.
    fn fill<T: cpal::Sample + cpal::FromSample<i16>>(&self, out: &mut [T], channels: usize) {
        let silence = T::EQUILIBRIUM;
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !s.primed {
            if s.buf.len() < self.preroll {
                out.fill(silence);
                return;
            }
            s.primed = true;
        }
        for frame in out.chunks_mut(channels) {
            frame.fill(s.buf.pop_front().map_or(silence, T::from_sample));
        }
    }
}

/// A mono 16-bit PCM WAV file whose sizes are written when it is dropped.
struct Wav {
    out: BufWriter<File>,
    path: PathBuf,
    samples: u32,
    failed: bool,
}

impl Wav {
    fn create(path: &Path, rate: u32) -> Result<Wav> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("--audio-wav: creating {}", dir.display()))?;
        }
        let file = File::create(path).with_context(|| format!("--audio-wav: creating {}", path.display()))?;
        let mut wav = Wav { out: BufWriter::new(file), path: path.to_owned(), samples: 0, failed: false };
        wav.out.write_all(&header(rate, 0)).with_context(|| format!("--audio-wav: writing {}", path.display()))?;
        Ok(wav)
    }

    fn push(&mut self, pcm: &[i16]) {
        if self.failed {
            return;
        }
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        match self.out.write_all(&bytes) {
            Ok(()) => self.samples = self.samples.saturating_add(pcm.len() as u32),
            Err(e) => self.fail(&e),
        }
    }

    fn fail(&mut self, e: &io::Error) {
        eprintln!("audio: writing {}: {e}", self.path.display());
        self.failed = true;
    }

    /// Patch the RIFF and data sizes.
    fn finish(&mut self) -> io::Result<()> {
        let data = self.samples.saturating_mul(2);
        self.out.seek(SeekFrom::Start(4))?;
        self.out.write_all(&(36u32.saturating_add(data)).to_le_bytes())?;
        self.out.seek(SeekFrom::Start(40))?;
        self.out.write_all(&data.to_le_bytes())?;
        self.out.flush()
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            self.fail(&e);
        }
    }
}

/// 44-byte RIFF header of a mono 16-bit PCM file with `samples` samples.
fn header(rate: u32, samples: u32) -> [u8; 44] {
    let data = samples.saturating_mul(2);
    let mut h = [0u8; 44];
    let fields: [(usize, &[u8]); 13] = [
        (0, b"RIFF"),
        (4, &(36u32.saturating_add(data)).to_le_bytes()),
        (8, b"WAVE"),
        (12, b"fmt "),
        (16, &16u32.to_le_bytes()),
        (20, &1u16.to_le_bytes()),
        (22, &1u16.to_le_bytes()),
        (24, &rate.to_le_bytes()),
        (28, &(rate * 2).to_le_bytes()),
        (32, &2u16.to_le_bytes()),
        (34, &16u16.to_le_bytes()),
        (36, b"data"),
        (40, &data.to_le_bytes()),
    ];
    for (at, bytes) in fields {
        h[at..at + bytes.len()].copy_from_slice(bytes);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_holds_the_exact_stream_and_its_sizes() {
        let dir = std::env::temp_dir().join(format!("ue2-audio-test-{}", std::process::id()));
        let path = dir.join("out.wav");
        {
            let opts = AudioOptions { wav: Some(path.clone()), ..AudioOptions::default() };
            let mut sink = start(&opts).unwrap().1.unwrap();
            assert_eq!(sink.rate(), WAV_RATE);
            sink.push(&[1, -2]);
            sink.push(&[i16::MAX, i16::MIN, 0]);
        }
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(&bytes[..44], &header(WAV_RATE, 5));
        let pcm: Vec<i16> = bytes[44..].chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(pcm, [1, -2, i16::MAX, i16::MIN, 0]);
    }

    #[test]
    fn ring_prerolls_drops_backlog_and_plays_silence_on_underrun() {
        let ring = Ring::new(1000);
        let mut out = [7i16; 4];
        ring.push(&[1; 59]);
        ring.fill(&mut out, 2);
        assert_eq!(out, [0; 4], "below the 60-sample pre-roll");
        ring.push(&[2]);
        ring.fill(&mut out, 2);
        assert_eq!(out, [1, 1, 1, 1], "primed; mono to both channels");
        ring.push(&[3; 200]);
        assert_eq!(ring.state.lock().unwrap().buf.len(), 60, "backlog past 150 dropped to the pre-roll");
        let mut long = [9i16; 130];
        ring.fill(&mut long, 1);
        assert_eq!((long[59], long[60]), (3, 0), "underrun: silence, no new pre-roll");
        ring.push(&[4]);
        ring.fill(&mut out, 2);
        assert_eq!(out, [4, 4, 0, 0]);
    }

    #[test]
    fn audio_defaults_follow_the_frontend() {
        let args = |audio| AudioArgs { audio, audio_wav: None, sid_socket1: None, no_sid_thread: false };
        assert!(configure(args(None), false).device, "window: on");
        assert!(!configure(args(None), true).device, "headless: off");
        assert!(!configure(args(None), false).armsid, "socket 1 empty by default");
        let armsid = AudioArgs { sid_socket1: Some(Socket::Armsid), ..args(None) };
        assert!(configure(armsid, true).armsid);
        assert!(configure(args(Some(OnOff::On)), true).device);
        assert!(!configure(args(Some(OnOff::Off)), false).device);
        let (_, sink) = start(&configure(args(None), true)).unwrap();
        assert!(sink.is_none(), "nothing to feed");
    }
}
