/* --------------------------
Audio capture for visualizer via cava
    - Creates a virtual PulseAudio sink so cava only captures jellyfin-tui audio
    - Spawns cava with a raw binary output config
    - Reads 16-bit bar values from cava's stdout
    - Sends bar data to the main thread
-------------------------- */

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

pub const BAR_COUNT: usize = 64;
const FRAMERATE: u32 = 60;

const NULL_SINK_NAME: &str = "jellyfin_tui_vis";
const COMBINE_SINK_NAME: &str = "jellyfin_tui_combined";

/// Virtual audio routing so cava only captures jellyfin-tui's audio.
///
/// On creation:
///   1. Loads `module-null-sink` → a silent sink whose `.monitor` cava reads from
///   2. Queries the default sink name
///   3. Loads `module-combine-sink` → fans mpv's audio to both the real output and the null sink
///
/// mpv outputs to the combine sink; cava reads the null sink's monitor.
/// No loopback module needed — no resampling glitches.
///
/// On drop the modules are unloaded.
pub struct VirtualSink {
    null_module: Option<u32>,
    combine_module: Option<u32>,
    /// The combine sink name for mpv's `audio-device` property.
    mpv_device: Option<String>,
}

impl VirtualSink {
    pub fn create() -> Self {
        // 1. Create a null sink (cava taps its monitor)
        let null_module = pactl_load_module(
            "module-null-sink",
            &[
                &format!("sink_name={NULL_SINK_NAME}"),
                "sink_properties=device.description=jellyfin-tui-visualizer",
            ],
        );

        if null_module.is_none() {
            log::warn!("Failed to create null sink — visualizer will capture all audio");
            return Self { null_module: None, combine_module: None, mpv_device: None };
        }

        // 2. Find the current default sink
        let default_sink = get_default_sink().unwrap_or_else(|| {
            log::warn!("Could not determine default sink, falling back to @DEFAULT_SINK@");
            "@DEFAULT_SINK@".to_string()
        });

        // 3. Create a combine sink that sends audio to both the real output and our null sink
        let combine_module = pactl_load_module(
            "module-combine-sink",
            &[
                &format!("sink_name={COMBINE_SINK_NAME}"),
                &format!("slaves={default_sink},{NULL_SINK_NAME}"),
                "sink_properties=device.description=jellyfin-tui-output",
            ],
        );

        let mpv_device = if combine_module.is_some() {
            Some(format!("pulse/{COMBINE_SINK_NAME}"))
        } else {
            log::warn!("Failed to create combine sink — visualizer will capture all audio");
            None
        };

        Self { null_module, combine_module, mpv_device }
    }

    /// The PulseAudio device string for mpv's `audio-device` property.
    pub fn mpv_audio_device(&self) -> Option<&str> {
        self.mpv_device.as_deref()
    }

    /// The PulseAudio source name for cava's input config.
    fn cava_source(&self) -> &str {
        if self.null_module.is_some() && self.combine_module.is_some() {
            concat!("jellyfin_tui_vis", ".monitor")
        } else {
            "auto"
        }
    }
}

impl Drop for VirtualSink {
    fn drop(&mut self) {
        if let Some(id) = self.combine_module {
            pactl_unload_module(id);
        }
        if let Some(id) = self.null_module {
            pactl_unload_module(id);
        }
    }
}

fn get_default_sink() -> Option<String> {
    let output = Command::new("pactl")
        .args(["get-default-sink"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() { None } else { Some(name) }
    } else {
        None
    }
}

fn pactl_load_module(module: &str, args: &[&str]) -> Option<u32> {
    let output = Command::new("pactl")
        .arg("load-module")
        .arg(module)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let id_str = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let id = id_str.parse::<u32>().ok();
            log::info!("Loaded {module} (id={id_str})");
            id
        }
        Ok(o) => {
            log::warn!(
                "pactl load-module {module} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            None
        }
        Err(e) => {
            log::warn!("pactl not available: {e}");
            None
        }
    }
}

fn pactl_unload_module(id: u32) {
    let _ = Command::new("pactl")
        .args(["unload-module", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub struct AudioCapture {
    pub spectrum_rx: mpsc::Receiver<Vec<f64>>,
}

impl AudioCapture {
    pub fn start(sink: &VirtualSink) -> Self {
        let (tx, rx) = mpsc::sync_channel(4);
        let cava_source = sink.cava_source().to_string();

        thread::spawn(move || {
            if let Err(e) = capture_loop(tx, &cava_source) {
                log::warn!("cava capture ended: {}", e);
            }
        });

        Self { spectrum_rx: rx }
    }
}

fn capture_loop(
    tx: mpsc::SyncSender<Vec<f64>>,
    source: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = format!(
        "[general]\n\
         bars = {BAR_COUNT}\n\
         framerate = {FRAMERATE}\n\
         \n\
         [input]\n\
         method = pulse\n\
         source = {source}\n\
         \n\
         [output]\n\
         method = raw\n\
         raw_target = /dev/stdout\n\
         data_format = binary\n\
         bit_format = 16bit\n"
    );

    let config_path = std::env::temp_dir().join("jellyfin-tui-cava.conf");
    std::fs::write(&config_path, &config)?;

    let mut child = Command::new("cava")
        .arg("-p")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().ok_or("cava: no stdout")?;
    let mut reader = std::io::BufReader::new(stdout);

    let frame_bytes = BAR_COUNT * 2; // 16-bit little-endian per bar
    let mut buf = vec![0u8; frame_bytes];
    let mut bars = vec![0.0f64; BAR_COUNT];

    loop {
        if let Err(e) = reader.read_exact(&mut buf) {
            // cava exited
            log::info!("cava exited: {}", e);
            break;
        }

        for (i, chunk) in buf.chunks_exact(2).enumerate() {
            let val = u16::from_le_bytes([chunk[0], chunk[1]]);
            bars[i] = val as f64 / 65535.0;
        }

        // Drop frames if the main thread is behind — never block
        let _ = tx.try_send(bars.clone());

        if matches!(
            tx.try_send(bars.clone()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_))
        ) {
            break;
        }
    }

    let _ = child.kill();
    Ok(())
}
