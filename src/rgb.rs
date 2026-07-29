/*
Synchronizes the fan lighting with the album's primary colors via PitRGB.

Jellyfin TUI owns one PitRGB layer while music is playing. Pausing or exiting
deletes that layer, revealing the next-highest layer. PitRGB is responsible for
transitioning between colors, so updates here are deliberately one-shot.
*/

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SOCKET_TIMEOUT: Duration = Duration::from_secs(1);

type Rgb8 = (u8, u8, u8);

#[derive(Debug, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Request {
    SetLayer { layer: u32, colors: FanColors },
    DeleteLayer { layer: u32 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FanColors {
    Solid { rgb: Rgb },
    Split { left: Rgb, right: Rgb },
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl From<Rgb8> for Rgb {
    fn from((r, g, b): Rgb8) -> Self {
        Self { r, g, b }
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    ok: bool,
    error: Option<String>,
}

struct PitRgbClient {
    socket: PathBuf,
    layer: u32,
}

impl PitRgbClient {
    fn connect(socket: PathBuf, layer: u32) -> std::io::Result<Self> {
        // Probe the socket now so is_active() accurately reflects whether fan
        // colors can be sent and album palettes need to be extracted.
        UnixStream::connect(&socket)?;
        Ok(Self { socket, layer })
    }

    fn set_color(&self, primary: Rgb8, secondary: Rgb8) -> Result<(), String> {
        let colors = if primary == secondary {
            FanColors::Solid { rgb: primary.into() }
        } else {
            FanColors::Split { left: primary.into(), right: secondary.into() }
        };
        self.send(&Request::SetLayer { layer: self.layer, colors })
    }

    fn delete_layer(&self) -> Result<(), String> {
        self.send(&Request::DeleteLayer { layer: self.layer })
    }

    fn send(&self, request: &Request) -> Result<(), String> {
        send_request(&self.socket, request)
    }
}

pub struct RgbSync {
    client: Option<PitRgbClient>,
    last_color: Option<(Rgb8, Rgb8)>,
    paused: bool,
    layer_active: bool,
}

impl RgbSync {
    pub fn new(enabled: bool, socket: PathBuf, layer: u32) -> Self {
        let client =
            enabled.then(|| PitRgbClient::connect(socket, layer)).and_then(|result| match result {
                Ok(client) => {
                    log::info!("Connected to PitRGB; fan lighting will follow the album color");
                    Some(client)
                }
                Err(error) => {
                    log::info!("PitRGB is unavailable: {error}");
                    None
                }
            });

        // App starts paused and may restore cover art before its first run-loop
        // update. Starting paused prevents that early color extraction from
        // briefly publishing the restored album layer.
        Self { client, last_color: None, paused: true, layer_active: false }
    }

    pub fn is_active(&self) -> bool {
        self.client.is_some()
    }

    /// Splits the fan LEDs between the primary and secondary colors. Passing
    /// the same color twice produces a solid fill.
    pub fn set_color(&mut self, primary: Rgb8, secondary: Rgb8) {
        let colors = (primary, secondary);
        if self.last_color == Some(colors) && (self.paused || self.layer_active) {
            return;
        }
        self.last_color = Some(colors);
        if self.paused {
            return;
        }

        let Some(client) = &self.client else { return };
        match client.set_color(primary, secondary) {
            Ok(()) => self.layer_active = true,
            Err(error) => {
                self.layer_active = false;
                log::warn!("Failed to set PitRGB layer: {error}");
            }
        }
    }

    /// Releases this application's layer while paused and restores it on
    /// resume. Idempotent, so it can be called on every update tick.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;

        let Some(client) = &self.client else { return };
        let result = if paused {
            client.delete_layer()
        } else if let Some((primary, secondary)) = self.last_color {
            client.set_color(primary, secondary)
        } else {
            return;
        };

        match result {
            Ok(()) => self.layer_active = !paused,
            Err(error) => {
                self.layer_active = false;
                log::warn!("Failed to update PitRGB layer: {error}");
            }
        }
    }

    /// Releases this application's layer before exiting.
    pub fn shutdown(&mut self) {
        let Some(client) = &self.client else { return };
        if let Err(error) = client.delete_layer() {
            log::warn!("Failed to delete PitRGB layer: {error}");
        }
        self.layer_active = false;
    }
}

fn send_request(socket: &Path, request: &Request) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT)).map_err(|error| error.to_string())?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT)).map_err(|error| error.to_string())?;

    serde_json::to_writer(&mut stream, request).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).map_err(|error| error.to_string())?;
    if response.is_empty() {
        return Err("PitRGB closed the connection without a response".to_string());
    }

    let response: Response = serde_json::from_str(&response).map_err(|error| error.to_string())?;
    if response.ok {
        Ok(())
    } else {
        Err(response.error.unwrap_or_else(|| "PitRGB rejected the request".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_split_layer_for_pitrgb() {
        let request = Request::SetLayer {
            layer: 10,
            colors: FanColors::Split {
                left: Rgb { r: 1, g: 2, b: 3 },
                right: Rgb { r: 4, g: 5, b: 6 },
            },
        };

        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"command":"set_layer","layer":10,"colors":{"type":"split","left":{"r":1,"g":2,"b":3},"right":{"r":4,"g":5,"b":6}}}"#
        );
    }

    #[test]
    fn serializes_layer_deletion_for_pitrgb() {
        let request = Request::DeleteLayer { layer: 10 };

        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"command":"delete_layer","layer":10}"#
        );
    }
}
