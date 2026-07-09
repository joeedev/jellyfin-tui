/*
Synchronizes RGB lighting with the album's primary color via OpenRGB.

When an OpenRGB SDK server is running we keep a persistent connection to it,
which is fast enough to fade smoothly between colors like the UI does. Without
a server we fall back to one-shot openrgb CLI calls (~1s each, so no fade).
On startup the current lighting is snapshotted into a profile and restored on
exit; both go through the CLI, which handles profiles reliably.

The SDK writes are hand-encoded onto a raw socket: the openrgb crate's write
encodings are subtly off-spec (internal size fields), and OpenRGB 1.0 servers
validate packets and silently drop offending clients. The crate's read path is
correct, so it is still used to discover the controller layout.

Commands are processed sequentially on a dedicated thread so the snapshot is
always taken before the first color is applied, and a color arriving mid-fade
retargets the fade from wherever it currently is.
*/

use std::io::Write;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SDK_ADDR: &str = "127.0.0.1:6742";
// no .orp extension: openrgb appends it on save, and load accepts either form
const RESTORE_PROFILE: &str = "jellyfin-tui-restore";
// Screen colors are gamma-encoded sRGB but LED channels are linear in duty
// cycle, so weaker channels emit far more light on LEDs than on screen (red
// washes out to pink). Channels are corrected relative to the strongest one,
// preserving overall brightness.
const LED_GAMMA: f32 = 2.8;
// dark album colors are scaled up to this so the LEDs never look switched off
const LED_MIN_BRIGHTNESS: f32 = 60.0;
const FADE_TICK: Duration = Duration::from_millis(33);

// OpenRGB SDK packet ids
const PACKET_SET_CLIENT_NAME: u32 = 50;
const PACKET_UPDATE_LEDS: u32 = 1050;
const PACKET_SET_CUSTOM_MODE: u32 = 1100;

enum RgbCommand {
    Color(u8, u8, u8),
    Restore,
}

pub struct RgbSync {
    tx: Option<Sender<RgbCommand>>,
    handle: Option<JoinHandle<()>>,
    last_color: Option<(u8, u8, u8)>,
}

impl RgbSync {
    pub fn new(enabled: bool, fade_ms: u64) -> Self {
        if !enabled || !openrgb_available() {
            return Self { tx: None, handle: None, last_color: None };
        }
        let (tx, rx) = std::sync::mpsc::channel::<RgbCommand>();
        let handle = std::thread::spawn(move || t_openrgb(rx, fade_ms));
        log::info!("OpenRGB found, lighting will follow the album color");
        Self { tx: Some(tx), handle: Some(handle), last_color: None }
    }

    pub fn is_active(&self) -> bool {
        self.tx.is_some()
    }

    pub fn set_color(&mut self, r: u8, g: u8, b: u8) {
        if self.last_color == Some((r, g, b)) {
            return;
        }
        if let Some(tx) = &self.tx {
            if tx.send(RgbCommand::Color(r, g, b)).is_ok() {
                self.last_color = Some((r, g, b));
            }
        }
    }

    /// Restores the lighting captured at startup and waits for it to finish.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(RgbCommand::Restore);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum FadeEnd {
    Completed,
    SdkFailed,
    RestoreRequested,
}

fn t_openrgb(rx: Receiver<RgbCommand>, fade_ms: u64) {
    let mut sdk = Sdk::connect();
    let server_seen = sdk.is_some();

    // snapshot current lighting so we can restore it on exit;
    // the save can fail transiently, so retry
    for _ in 0..3 {
        if run_openrgb(&["--save-profile", RESTORE_PROFILE]) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let mut current: Option<(f32, f32, f32)> = None;
    'outer: while let Ok(cmd) = rx.recv() {
        let mut target = match cmd {
            RgbCommand::Color(r, g, b) => (r as f32, g as f32, b as f32),
            RgbCommand::Restore => break,
        };
        // apply only the newest of any queued updates
        loop {
            match rx.try_recv() {
                Ok(RgbCommand::Color(r, g, b)) => target = (r as f32, g as f32, b as f32),
                Ok(RgbCommand::Restore) => break 'outer,
                Err(_) => break,
            }
        }

        // a lost connection is retried on the next color, not every tick
        if sdk.is_none() && server_seen {
            sdk = Sdk::connect();
        }

        let end = match &mut sdk {
            Some(s) => run_fade(s, &rx, &mut current, &mut target, fade_ms),
            None => FadeEnd::SdkFailed,
        };
        match end {
            FadeEnd::Completed => {}
            FadeEnd::RestoreRequested => break 'outer,
            FadeEnd::SdkFailed => {
                if sdk.is_some() {
                    log::warn!("OpenRGB SDK connection lost, reconnecting");
                    sdk = Sdk::connect();
                }
                // one immediate retry over a fresh connection, else the CLI
                let applied = match &mut sdk {
                    Some(s) => s.apply(gamma_correct(target)),
                    None => false,
                };
                if !applied {
                    sdk = None;
                    let (r, g, b) = gamma_correct(target);
                    run_openrgb(&["--color", &format!("{:02X}{:02X}{:02X}", r, g, b)]);
                }
                current = Some(target);
            }
        }
    }

    run_openrgb(&["--profile", RESTORE_PROFILE]);
}

fn run_fade(
    sdk: &mut Sdk,
    rx: &Receiver<RgbCommand>,
    current: &mut Option<(f32, f32, f32)>,
    target: &mut (f32, f32, f32),
    fade_ms: u64,
) -> FadeEnd {
    // no known starting color -> jump straight to the target
    let mut from = current.unwrap_or(*target);
    let mut started = Instant::now();
    loop {
        let t = if fade_ms == 0 {
            1.0
        } else {
            (started.elapsed().as_millis() as f32 / fade_ms as f32).min(1.0)
        };
        let cur = (
            from.0 + (target.0 - from.0) * t,
            from.1 + (target.1 - from.1) * t,
            from.2 + (target.2 - from.2) * t,
        );
        if !sdk.apply(gamma_correct(cur)) {
            return FadeEnd::SdkFailed;
        }
        *current = Some(cur);
        if t >= 1.0 {
            return FadeEnd::Completed;
        }
        std::thread::sleep(FADE_TICK);
        match rx.try_recv() {
            Ok(RgbCommand::Color(r, g, b)) => {
                // retarget the fade from wherever it is now
                from = cur;
                *target = (r as f32, g as f32, b as f32);
                started = Instant::now();
            }
            Ok(RgbCommand::Restore) | Err(TryRecvError::Disconnected) => {
                return FadeEnd::RestoreRequested
            }
            Err(TryRecvError::Empty) => {}
        }
    }
}

fn gamma_correct((r, g, b): (f32, f32, f32)) -> (u8, u8, u8) {
    let max = r.max(g).max(b);
    if max <= 0.0 {
        let floor = LED_MIN_BRIGHTNESS as u8;
        return (floor, floor, floor);
    }
    let scale = if max < LED_MIN_BRIGHTNESS { LED_MIN_BRIGHTNESS / max } else { 1.0 };
    let correct = |c: f32| ((c / max).powf(LED_GAMMA) * max * scale).round().min(255.0) as u8;
    (correct(r), correct(g), correct(b))
}

/// Persistent connection to a running OpenRGB SDK server.
struct Sdk {
    stream: TcpStream,
    led_counts: Vec<usize>,
    custom_mode_set: bool,
}

impl Sdk {
    fn connect() -> Option<Self> {
        // discover the controller layout with the openrgb crate; reads are the
        // involved part of the protocol and the crate gets them right
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        let led_counts = rt.block_on(async {
            let client = openrgb::OpenRGB::connect().await?;
            let count = client.get_controller_count().await?;
            let mut led_counts = Vec::with_capacity(count as usize);
            for i in 0..count {
                led_counts.push(client.get_controller(i).await?.colors.len());
            }
            Ok::<_, openrgb::OpenRGBError>(led_counts)
        });
        let led_counts = match led_counts {
            Ok(counts) => counts,
            Err(e) => {
                log::info!("No OpenRGB SDK server ({}), using the CLI without fading", e);
                return None;
            }
        };

        let result = (|| {
            let mut stream = TcpStream::connect(SDK_ADDR)?;
            stream.set_nodelay(true)?;
            send_packet(&mut stream, 0, PACKET_SET_CLIENT_NAME, b"jellyfin-tui\0")?;
            Ok::<_, std::io::Error>(stream)
        })();
        match result {
            Ok(stream) => {
                log::info!("Connected to the OpenRGB SDK server, fading enabled");
                Some(Self { stream, led_counts, custom_mode_set: false })
            }
            Err(e) => {
                log::warn!("Failed to connect to the OpenRGB SDK server: {}", e);
                None
            }
        }
    }

    fn apply(&mut self, (r, g, b): (u8, u8, u8)) -> bool {
        for (i, &count) in self.led_counts.iter().enumerate() {
            let result = (|| {
                if !self.custom_mode_set {
                    send_packet(&mut self.stream, i as u32, PACKET_SET_CUSTOM_MODE, &[])?;
                }
                // payload: u32 data_size (including itself), u16 count, then
                // one (r, g, b, pad) per LED
                let mut payload = Vec::with_capacity(6 + 4 * count);
                payload.extend_from_slice(&((6 + 4 * count) as u32).to_le_bytes());
                payload.extend_from_slice(&(count as u16).to_le_bytes());
                for _ in 0..count {
                    payload.extend_from_slice(&[r, g, b, 0]);
                }
                send_packet(&mut self.stream, i as u32, PACKET_UPDATE_LEDS, &payload)
            })();
            if let Err(e) = result {
                log::warn!("OpenRGB update failed: {}", e);
                return false;
            }
        }
        self.custom_mode_set = true;
        true
    }
}

fn send_packet(
    stream: &mut TcpStream,
    device_id: u32,
    packet_id: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(16 + payload.len());
    buf.extend_from_slice(b"ORGB");
    buf.extend_from_slice(&device_id.to_le_bytes());
    buf.extend_from_slice(&packet_id.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf)
}

fn run_openrgb(args: &[&str]) -> bool {
    match Command::new("openrgb")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if !status.success() => {
            log::warn!("openrgb {:?} exited with {}", args, status);
            false
        }
        Err(e) => {
            log::warn!("Failed to run openrgb {:?}: {}", args, e);
            false
        }
        _ => true,
    }
}

fn openrgb_available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("openrgb").is_file()))
        .unwrap_or(false)
}
