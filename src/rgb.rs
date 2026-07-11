/*
Synchronizes RGB lighting with the album's primary colors via OpenRGB.

When an OpenRGB SDK server is running we keep a persistent connection to it,
which is fast enough to fade smoothly between colors like the UI does. Without
a server we fall back to one-shot openrgb CLI calls (~1s each, so no fade).
On startup the current lighting is snapshotted into a profile and restored on
exit; both go through the CLI, which handles profiles reliably.

Each device shows a gradient between the album's two most prominent colors
(which collapse to a solid color when they match, or when openrgb_two_colors
is off). While playback is paused the LEDs fade to fully off and the song's
colors are remembered for resume. The CLI fallback can only set one color for
everything, so it uses the primary.

The SDK writes are hand-encoded onto a raw socket: the openrgb crate's write
encodings are subtly off-spec (internal size fields), and OpenRGB 1.0 servers
validate packets and silently drop offending clients. The crate's read path is
correct, so it is still used to discover the controller layout.

Commands are processed sequentially on a dedicated thread so the snapshot is
always taken before the first color is applied, and a command arriving
mid-fade retargets the fade from wherever it currently is.
*/

use std::io::{Read, Write};
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
const PACKET_REQUEST_PROTOCOL_VERSION: u32 = 40;
const PACKET_UPDATE_LEDS: u32 = 1050;
const PACKET_SET_CUSTOM_MODE: u32 = 1100;
// Protocol v3 is the newest version supported by the openrgb crate used for
// controller discovery below. Requesting it explicitly is also important for
// the raw persistent socket: newer OpenRGB servers may accept a TCP connection
// but ignore controller writes until the client has negotiated a protocol.
const SDK_PROTOCOL_VERSION: u32 = 3;

type Rgb = (f32, f32, f32);
type Rgb8 = (u8, u8, u8);
/// The colors at the two ends of each device's LED gradient.
type Pair = (Rgb, Rgb);

const OFF: Pair = ((0.0, 0.0, 0.0), (0.0, 0.0, 0.0));

enum RgbCommand {
    Color(Rgb8, Rgb8),
    Paused(bool),
    Restore,
}

pub struct RgbSync {
    tx: Option<Sender<RgbCommand>>,
    handle: Option<JoinHandle<()>>,
    last_color: Option<(Rgb8, Rgb8)>,
    paused: bool,
    sent_paused: bool,
}

impl RgbSync {
    pub fn new(enabled: bool, fade_ms: u64) -> Self {
        if !enabled || !openrgb_available() {
            return Self {
                tx: None,
                handle: None,
                last_color: None,
                paused: false,
                sent_paused: false,
            };
        }
        let (tx, rx) = std::sync::mpsc::channel::<RgbCommand>();
        let handle = std::thread::spawn(move || t_openrgb(rx, fade_ms));
        log::info!("OpenRGB found, lighting will follow the album color");
        Self {
            tx: Some(tx),
            handle: Some(handle),
            last_color: None,
            paused: false,
            sent_paused: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.tx.is_some()
    }

    /// Each device fades between primary and secondary along its LEDs; pass
    /// the same color twice for a solid fill.
    pub fn set_color(&mut self, primary: Rgb8, secondary: Rgb8) {
        if self.last_color == Some((primary, secondary)) {
            return;
        }
        let Some(tx) = &self.tx else { return };
        // the player can start paused (e.g. a restored queue); tell the worker
        // before the first color so the LEDs never flash on
        if self.last_color.is_none()
            && self.paused
            && !self.sent_paused
            && tx.send(RgbCommand::Paused(true)).is_ok()
        {
            self.sent_paused = true;
        }
        if tx.send(RgbCommand::Color(primary, secondary)).is_ok() {
            self.last_color = Some((primary, secondary));
        }
    }

    /// Turns the LEDs off while paused and back on when playback resumes.
    /// Idempotent, so it can be called every update tick.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        // until the first song color arrives the lighting isn't ours to touch
        if self.last_color.is_none() || self.sent_paused == paused {
            return;
        }
        if let Some(tx) = &self.tx {
            if tx.send(RgbCommand::Paused(paused)).is_ok() {
                self.sent_paused = paused;
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

fn to_pair(a: Rgb8, b: Rgb8) -> Pair {
    ((a.0 as f32, a.1 as f32, a.2 as f32), (b.0 as f32, b.1 as f32, b.2 as f32))
}

fn effective_target(song: Option<Pair>, paused: bool) -> Pair {
    if paused {
        OFF
    } else {
        song.unwrap_or(OFF)
    }
}

/// Applies the minimum brightness to a song's colors. It happens once, up
/// front, rather than per fade frame — otherwise fades to and from black jump
/// to the floor instead of ramping smoothly. The floor covers the pair as a
/// whole: if either end of the gradient is bright the LEDs read as on, and a
/// deliberately dark other end (red/black covers) must stay dark rather than
/// get boosted to gray.
fn floored(a: Rgb8, b: Rgb8) -> Pair {
    let (a, b) = to_pair(a, b);
    let max = a.0.max(a.1).max(a.2).max(b.0.max(b.1).max(b.2));
    if max <= 0.0 {
        let f = LED_MIN_BRIGHTNESS;
        return ((f, f, f), (f, f, f));
    }
    if max >= LED_MIN_BRIGHTNESS {
        return (a, b);
    }
    let s = LED_MIN_BRIGHTNESS / max;
    ((a.0 * s, a.1 * s, a.2 * s), (b.0 * s, b.1 * s, b.2 * s))
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

    let mut current: Option<Pair> = None;
    let mut song: Option<Pair> = None;
    let mut paused = false;
    'outer: while let Ok(first) = rx.recv() {
        // fold in any queued commands so only the newest state is applied
        let mut cmd = Some(first);
        while let Some(c) = cmd {
            match c {
                RgbCommand::Color(a, b) => song = Some(floored(a, b)),
                RgbCommand::Paused(p) => paused = p,
                RgbCommand::Restore => break 'outer,
            }
            cmd = rx.try_recv().ok();
        }
        if song.is_none() {
            continue;
        }

        // a lost connection is retried on the next command, not every tick
        if sdk.is_none() && server_seen {
            sdk = Sdk::connect();
        }

        let end = match &mut sdk {
            Some(s) => run_fade(s, &rx, &mut current, &mut song, &mut paused, fade_ms),
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
                let target = effective_target(song, paused);
                // one immediate retry over a fresh connection, else the CLI
                let applied = match &mut sdk {
                    Some(s) => s.apply(gamma_correct(target.0), gamma_correct(target.1)),
                    None => false,
                };
                if !applied {
                    sdk = None;
                    // the CLI sets everything at once, so only the primary is used
                    let (r, g, b) = gamma_correct(target.0);
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
    current: &mut Option<Pair>,
    song: &mut Option<Pair>,
    paused: &mut bool,
    fade_ms: u64,
) -> FadeEnd {
    let mut target = effective_target(*song, *paused);
    // no known starting color -> jump straight to the target
    let mut from = current.unwrap_or(target);
    let mut started = Instant::now();
    loop {
        let t = if fade_ms == 0 {
            1.0
        } else {
            (started.elapsed().as_millis() as f32 / fade_ms as f32).min(1.0)
        };
        let cur = (lerp(from.0, target.0, t), lerp(from.1, target.1, t));
        if !sdk.apply(gamma_correct(cur.0), gamma_correct(cur.1)) {
            return FadeEnd::SdkFailed;
        }
        *current = Some(cur);
        if t >= 1.0 {
            return FadeEnd::Completed;
        }
        std::thread::sleep(FADE_TICK);
        // retarget the fade from wherever it is now
        match rx.try_recv() {
            Ok(RgbCommand::Color(a, b)) => {
                *song = Some(floored(a, b));
                from = cur;
                target = effective_target(*song, *paused);
                started = Instant::now();
            }
            Ok(RgbCommand::Paused(p)) => {
                *paused = p;
                from = cur;
                target = effective_target(*song, *paused);
                started = Instant::now();
            }
            Ok(RgbCommand::Restore) | Err(TryRecvError::Disconnected) => {
                return FadeEnd::RestoreRequested
            }
            Err(TryRecvError::Empty) => {}
        }
    }
}

fn lerp(from: Rgb, to: Rgb, t: f32) -> Rgb {
    (
        from.0 + (to.0 - from.0) * t,
        from.1 + (to.1 - from.1) * t,
        from.2 + (to.2 - from.2) * t,
    )
}

fn gamma_correct((r, g, b): Rgb) -> Rgb8 {
    let max = r.max(g).max(b);
    if max <= 0.0 {
        return (0, 0, 0);
    }
    let correct = |c: f32| ((c / max).powf(LED_GAMMA) * max).round().min(255.0) as u8;
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
            negotiate_protocol(&mut stream)?;
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

    /// Fills each device with a gradient from color `a` to color `b`.
    fn apply(&mut self, a: Rgb8, b: Rgb8) -> bool {
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
                for led in 0..count {
                    let t = if count > 1 { led as f32 / (count - 1) as f32 } else { 0.0 };
                    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
                    payload.extend_from_slice(&[mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2), 0]);
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

/// Negotiate the SDK protocol on the persistent write connection.  The
/// discovery client above does this on its own connection, but protocol state
/// belongs to each TCP client.  Skipping it can leave OpenRGB accepting writes
/// at the socket level while discarding the lighting updates.
fn negotiate_protocol(stream: &mut TcpStream) -> std::io::Result<()> {
    send_packet(
        stream,
        0,
        PACKET_REQUEST_PROTOCOL_VERSION,
        &SDK_PROTOCOL_VERSION.to_le_bytes(),
    )?;

    let mut header = [0u8; 16];
    stream.read_exact(&mut header)?;
    if &header[..4] != b"ORGB" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "OpenRGB protocol response has an invalid magic value",
        ));
    }

    let device_id = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let packet_id = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let payload_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if device_id != 0 || packet_id != PACKET_REQUEST_PROTOCOL_VERSION || payload_len != 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unexpected OpenRGB protocol response (device={device_id}, packet={packet_id}, bytes={payload_len})"
            ),
        ));
    }

    let mut payload = [0u8; 4];
    stream.read_exact(&mut payload)?;
    let negotiated = u32::from_le_bytes(payload);
    if negotiated == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "OpenRGB negotiated protocol version 0",
        ));
    }
    Ok(())
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
