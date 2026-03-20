/* --------------------------
Full-screen player view
    - Big album art centered
    - FFT visualizer bars in the background (via PulseAudio/PipeWire capture)
    - Track info + progress bar at the bottom
-------------------------- */

use crate::tui::App;
use ratatui::{prelude::*, widgets::*, Frame};
use ratatui_image::{Resize, StatefulImage};

/// Characters used for drawing visualizer bars (from empty to full)
const BAR_CHARS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

impl App {
    /// Toggle fullscreen mode and start/stop audio capture as needed.
    pub fn set_fullscreen(&mut self, enabled: bool) {
        self.fullscreen = enabled;
        self.dirty = true;

        #[cfg(feature = "visualizer")]
        if enabled {
            if self.audio_capture.is_none() {
                self.audio_capture =
                    Some(crate::audio_capture::AudioCapture::start(&self.virtual_sink));
            }
        } else {
            self.audio_capture = None;
            self.visualizer_bars.iter_mut().for_each(|b| *b = 0.0);
        }
    }

    pub fn render_fullscreen(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Background
        if let Some(bg) = self.theme.resolve_opt(&self.theme.background) {
            let background_block = Block::default().style(Style::default().bg(bg));
            frame.render_widget(background_block, area);
        }

        // Layout: visualizer fills the whole area, art + info layered on top
        // Bottom: 6 rows for track info + progress bar
        let layout = Layout::vertical([
            Constraint::Percentage(100), // art + visualizer area
            Constraint::Length(6),       // player info
        ])
        .split(area);

        let top_area = layout[0];
        let bottom_area = layout[1];

        // Render visualizer bars across the full top area (behind the art)
        self.render_visualizer_bars(frame, top_area);

        // Render album art centered in the top area
        let art_rect = self.render_fullscreen_art(frame, top_area);

        // Render track info + progress bar at the bottom, matched to art width
        self.render_fullscreen_player(frame, bottom_area, art_rect);

        // Temporarily show the top bar when a setting changes (volume, repeat, shuffle)
        if self.fullscreen_topbar_until.is_some_and(|t| t > tokio::time::Instant::now()) {
            let topbar_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            };
            // Clear the row first so visualizer bars don't bleed through
            let clear = Block::default().style(Style::default().bg(
                self.theme.resolve_opt(&self.theme.background).unwrap_or(Color::Black),
            ));
            frame.render_widget(clear, topbar_area);
            self.render_status_bar(topbar_area, frame.buffer_mut());
        }
    }

    fn render_visualizer_bars(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let bar_count = self.visualizer_bars.len();
        let primary = self.theme.primary_color;
        let buf = frame.buffer_mut();

        for col in 0..area.width {
            let bar_idx = ((col as f64 / area.width as f64) * bar_count as f64) as usize;
            let bar_val = self.visualizer_bars[bar_idx.min(bar_count - 1)];

            for row in 0..area.height {
                let row_from_bottom = area.height - 1 - row;
                let row_threshold = row_from_bottom as f64 / area.height as f64;

                let x = area.x + col;
                let y = area.y + row;

                if bar_val > row_threshold {
                    let cell_fill =
                        ((bar_val - row_threshold) * area.height as f64).clamp(0.0, 1.0);
                    let char_idx = (cell_fill * (BAR_CHARS.len() - 1) as f64) as usize;
                    let ch = BAR_CHARS[char_idx.min(BAR_CHARS.len() - 1)];

                    let brightness = 1.0 - (row_from_bottom as f64 / area.height as f64) * 0.9;
                    let color = dim_color(primary, brightness * 0.5);

                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_char(ch).set_fg(color);
                    }
                }
            }
        }
    }

    fn render_fullscreen_art(&mut self, frame: &mut Frame, area: Rect) -> Option<Rect> {
        if area.height < 4 {
            return None;
        }

        if let Some(cover_art) = self.cover_art_fullscreen.as_mut() {
            // Scale padding with terminal size; remove at small sizes
            let v_pad = if area.height < 20 { 1 } else if area.height < 30 { 2 } else { 6 };
            let h_pad = if area.width < 40 { 2 } else if area.width < 80 { 4 } else { 12 };
            let art_max_height = area.height.saturating_sub(v_pad);
            let art_max_width = area.width.saturating_sub(h_pad);

            let art_area = Rect {
                x: area.x,
                y: area.y,
                width: art_max_width,
                height: art_max_height,
            };

            let img = StatefulImage::default().resize(Resize::Scale(None));
            let img_size = cover_art.size_for(Resize::Scale(None), art_area);

            let art_y = (area.y + (area.height.saturating_sub(img_size.height)) / 2).max(area.y + 1);
            let centered = Rect {
                x: area.x + (area.width.saturating_sub(img_size.width)) / 2 + 1,
                y: art_y,
                width: img_size.width,
                height: img_size.height.min((area.y + area.height).saturating_sub(art_y + 1)),
            };

            frame.render_stateful_widget(img, centered, cover_art);
            Some(centered)
        } else {
            None
        }
    }

    fn render_fullscreen_player(&mut self, frame: &mut Frame, area: Rect, art_rect: Option<Rect>) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_type(self.border_type)
            .fg(self.theme.resolve(&self.theme.border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Padding based on width; shrinks at small sizes
        let h_pad = if inner.width < 40 {
            1
        } else if inner.width < 80 {
            (inner.width / 10).max(2)
        } else if let Some(art) = art_rect {
            let art_left = art.x.saturating_sub(inner.x);
            let art_right = inner.width.saturating_sub(art_left + art.width);
            let margin = art_left.min(art_right);
            // Shrink the margin by 20% so the info section is a bit wider than the art
            (margin * 4 / 5).max(4)
        } else {
            ((inner.width as u32 * 20 / 100) as u16).max(4)
        };

        let layout = Layout::vertical([
            Constraint::Length(2), // title + artist
            Constraint::Length(1), // progress bar
            Constraint::Length(1), // metadata
        ])
        .horizontal_margin(h_pad)
        .split(inner);

        let current_song = self.state.queue.get(self.state.current_playback_state.current_index);

        // Track info
        let lines = match current_song {
            Some(song) => {
                let artists = song.artists.join(", ");
                let mut title_spans = vec![
                    Span::styled(
                        &song.name,
                        Style::default()
                            .fg(self.theme.resolve(&self.theme.foreground))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " — ",
                        Style::default().fg(self.theme.resolve(&self.theme.foreground_dim)),
                    ),
                    Span::styled(
                        &song.album,
                        Style::default().fg(self.theme.resolve(&self.theme.foreground)),
                    ),
                ];
                if song.production_year > 0 {
                    title_spans.push(Span::styled(
                        format!(" ({})", song.production_year),
                        Style::default().fg(self.theme.resolve(&self.theme.foreground_dim)),
                    ));
                }

                let artist_line = if !artists.is_empty() {
                    Line::from(vec![
                        Span::styled(
                            "› ",
                            Style::default().fg(self.theme.resolve(&self.theme.foreground_dim)),
                        ),
                        Span::styled(
                            artists,
                            Style::default()
                                .fg(self.theme.resolve(&self.theme.foreground_secondary)),
                        ),
                    ])
                } else {
                    Line::default()
                };

                vec![Line::from(title_spans), artist_line]
            }
            None => vec![
                Line::from(Span::styled(
                    "No track playing",
                    Style::default().fg(self.theme.resolve(&self.theme.foreground)),
                )),
                Line::default(),
            ],
        };

        frame.render_widget(Paragraph::new(lines).left_aligned(), layout[0]);

        // Progress bar
        let total_seconds = current_song
            .map(|s| s.run_time_ticks as f64 / 10_000_000.0)
            .unwrap_or(self.state.current_playback_state.duration);

        let visible_position = if self.state.current_playback_state.seek_active {
            self.hard_seek_target
                .unwrap_or(self.state.current_playback_state.position)
        } else {
            self.state.current_playback_state.position
        };

        let percentage =
            if total_seconds > 0.0 { (visible_position / total_seconds) * 100.0 } else { 0.0 };

        let duration_str = if total_seconds == 0.0 {
            "0:00 / 0:00".to_string()
        } else {
            format!(
                "{}:{:02} / {}:{:02}",
                visible_position as u32 / 60,
                visible_position as u32 % 60,
                total_seconds as u32 / 60,
                total_seconds as u32 % 60,
            )
        };

        let progress_layout = Layout::horizontal([
            Constraint::Fill(100),
            Constraint::Min(duration_str.len() as u16 + 3),
        ])
        .split(layout[1]);

        frame.render_widget(
            LineGauge::default()
                .filled_style(if self.buffering {
                    Style::default()
                        .fg(self.theme.primary_color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(self.theme.resolve(&self.theme.progress_fill))
                        .add_modifier(Modifier::BOLD)
                })
                .unfilled_style(
                    Style::default()
                        .fg(self.theme.resolve(&self.theme.progress_track))
                        .add_modifier(Modifier::BOLD),
                )
                .style(Style::default().fg(self.theme.resolve(&self.theme.foreground)))
                .ratio(percentage.clamp(0.0, 100.0) / 100.0)
                .label(Line::from(format!(
                    "{}   {:.0}% ",
                    if self.buffering {
                        self.spinner_stages[self.spinner]
                    } else if self.paused ^ self.swap_play_pause {
                        "⏸︎"
                    } else {
                        "►"
                    },
                    percentage,
                ))),
            progress_layout[0],
        );

        frame.render_widget(
            Paragraph::new(duration_str)
                .right_aligned()
                .style(Style::default().fg(self.theme.resolve(&self.theme.foreground))),
            progress_layout[1],
        );

        // Metadata line
        let metadata_spans: Vec<Span> = current_song
            .map(|song| {
                if self.state.current_playback_state.audio_samplerate == 0
                    && self.state.current_playback_state.hr_channels.is_empty()
                {
                    return vec![Span::styled(
                        format!("{} Loading metadata", self.spinner_stages[self.spinner]),
                        Style::default().fg(self.theme.resolve(&self.theme.foreground)),
                    )];
                }

                let fg = self.theme.resolve(&self.theme.foreground);
                let sep = |s: &str| {
                    Span::styled(
                        format!(" {} ", s),
                        Style::default().fg(self.theme.resolve(&self.theme.foreground_dim)),
                    )
                };

                let sr = self.state.current_playback_state.audio_samplerate as f32;
                let khz = sr / 1000.0;
                let samplerate = if khz.fract() == 0.0 {
                    format!("{} kHz", khz as u32)
                } else {
                    format!("{:.1} kHz", khz)
                };

                let mut out = vec![
                    Span::styled(
                        &self.state.current_playback_state.file_format,
                        Style::default().fg(fg),
                    ),
                    sep("-"),
                    Span::styled(samplerate, Style::default().fg(fg)),
                    sep("-"),
                    Span::styled(
                        &self.state.current_playback_state.hr_channels,
                        Style::default().fg(fg),
                    ),
                    sep("-"),
                    Span::styled(
                        format!("{} kbps", self.state.current_playback_state.audio_bitrate),
                        Style::default().fg(fg),
                    ),
                ];

                if song.is_transcoded {
                    out.push(Span::styled(
                        " › tc",
                        Style::default().fg(fg).add_modifier(Modifier::DIM),
                    ));
                }
                out
            })
            .unwrap_or_else(|| vec![]);

        frame.render_widget(
            Paragraph::new(Line::from(metadata_spans)).left_aligned(),
            layout[2],
        );
    }
}

/// Dim a color by a factor (0.0 = black, 1.0 = original)
fn dim_color(color: Color, factor: f64) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f64 * factor) as u8,
            (g as f64 * factor) as u8,
            (b as f64 * factor) as u8,
        ),
        _ => color,
    }
}
