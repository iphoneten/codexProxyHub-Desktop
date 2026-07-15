use eframe::egui;
use image::AnimationDecoder;
use std::{
    io::{BufReader, Cursor},
    time::{Duration, Instant},
};

pub(super) struct AnimatedGif {
    frames: Vec<egui::TextureHandle>,
    delays: Vec<Duration>,
    total_duration: Duration,
    started_at: Instant,
}

impl AnimatedGif {
    pub(super) fn load(ctx: &egui::Context, name: &str, bytes: &'static [u8]) -> Option<Self> {
        let decoder =
            image::codecs::gif::GifDecoder::new(BufReader::new(Cursor::new(bytes))).ok()?;
        let frames = decoder.into_frames().collect_frames().ok()?;
        let mut textures = Vec::new();
        let mut delays = Vec::new();
        for (idx, frame) in frames.into_iter().enumerate() {
            let delay = frame.delay();
            let (numer, denom) = delay.numer_denom_ms();
            let millis = if denom == 0 {
                100
            } else {
                ((numer as f64 / denom as f64).round() as u64).max(20)
            };
            let image = frame.into_buffer();
            let size = [image.width() as usize, image.height() as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
            textures.push(ctx.load_texture(
                format!("{name}-{idx}"),
                color_image,
                egui::TextureOptions::LINEAR,
            ));
            delays.push(Duration::from_millis(millis));
        }
        let total_duration: Duration = delays.iter().copied().sum();
        if textures.is_empty() || total_duration.is_zero() {
            return None;
        }
        Some(Self {
            frames: textures,
            delays,
            total_duration,
            started_at: Instant::now(),
        })
    }

    pub(super) fn texture(&self) -> &egui::TextureHandle {
        let elapsed = self.started_at.elapsed().as_millis() as u64;
        let total = self.total_duration.as_millis().max(1) as u64;
        let mut cursor = Duration::from_millis(elapsed % total);
        for (idx, delay) in self.delays.iter().enumerate() {
            if cursor < *delay {
                return &self.frames[idx];
            }
            cursor = cursor.saturating_sub(*delay);
        }
        self.frames.last().unwrap_or(&self.frames[0])
    }
}
