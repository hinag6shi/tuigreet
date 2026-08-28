//! GIF-backed animation: decodes a GIF once, downsamples it to the
//! terminal's character grid, maps each cell to a glyph + RGB color, and
//! caches the result to disk so subsequent runs just replay it.

use std::{
  fs,
  io::BufReader,
  path::PathBuf,
  time::{Instant, UNIX_EPOCH},
};

use image::{
  AnimationDecoder, ImageBuffer, Rgba, RgbaImage, codecs::gif::GifDecoder,
  imageops::FilterType,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tui::{
  buffer::Buffer,
  layout::{Position, Rect},
  style::Color,
};

use super::Animation;

/// Default glyph ramp, dark to bright.
const DEFAULT_CHARSET: &str = " .:-=+*#%@";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
  Fit,
  Fill,
  Stretch,
}

impl ScaleMode {
  pub fn from_name(name: &str) -> Self {
    match name.trim().to_ascii_lowercase().as_str() {
      "fill" => Self::Fill,
      "stretch" => Self::Stretch,
      _ => Self::Fit,
    }
  }

  fn as_str(&self) -> &'static str {
    match self {
      Self::Fit => "fit",
      Self::Fill => "fill",
      Self::Stretch => "stretch",
    }
  }
}

#[derive(Debug, Clone)]
pub struct Options {
  /// Path to the source GIF file.
  pub path: PathBuf,
  /// Glyph ramp from darkest to brightest.
  pub charset: String,
  /// How the source aspect ratio is fit into the terminal grid.
  pub scale_mode: ScaleMode,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      path: PathBuf::new(),
      charset: DEFAULT_CHARSET.to_string(),
      scale_mode: ScaleMode::Fit,
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CachedCell {
  ch: char,
  r: u8,
  g: u8,
  b: u8,
  a: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFrame {
  cells: Vec<CachedCell>,
  delay_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedVideo {
  cols: u16,
  rows: u16,
  frames: Vec<CachedFrame>,
}

pub struct Video {
  width: u16,
  height: u16,
  opts: Options,
  frames: Vec<CachedFrame>,
  cum_ms: Vec<u64>,
  total_ms: u64,
  start: Instant,
  cur: usize,
  load_error: bool,
}

impl Video {
  pub fn new(opts: Options) -> Self {
    Self {
      width: 0,
      height: 0,
      opts,
      frames: Vec::new(),
      cum_ms: Vec::new(),
      total_ms: 0,
      start: Instant::now(),
      cur: 0,
      load_error: false,
    }
  }

  fn cache_path(&self, cols: u16, rows: u16) -> Option<PathBuf> {
    let meta = fs::metadata(&self.opts.path).ok()?;
    let mtime = meta
      .modified()
      .ok()?
      .duration_since(UNIX_EPOCH)
      .ok()?
      .as_secs();
    let len = meta.len();

    let mut hasher = Sha256::new();
    hasher.update(self.opts.path.to_string_lossy().as_bytes());
    hasher.update(mtime.to_le_bytes());
    hasher.update(len.to_le_bytes());
    hasher.update(cols.to_le_bytes());
    hasher.update(rows.to_le_bytes());
    hasher.update(self.opts.charset.as_bytes());
    hasher.update(self.opts.scale_mode.as_str().as_bytes());
    let hash: String = hasher
      .finalize()
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect();

    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    Some(
      base
        .join("tuigreet")
        .join("video")
        .join(format!("{hash}.bin")),
    )
  }

  fn load_or_build(&mut self, cols: u16, rows: u16) {
    self.load_error = false;
    self.frames.clear();
    self.cum_ms.clear();
    self.total_ms = 0;
    self.cur = 0;
    self.start = Instant::now();

    if cols == 0 || rows == 0 || self.opts.path.as_os_str().is_empty() {
      self.load_error = true;
      return;
    }

    let cache_path = self.cache_path(cols, rows);

    if let Some(ref cp) = cache_path
      && let Ok(bytes) = fs::read(cp)
      && let Ok(cached) = bincode::deserialize::<CachedVideo>(&bytes)
      && cached.cols == cols
      && cached.rows == rows
    {
      self.finish_load(cached.frames);
      return;
    }

    match self.decode_and_build(cols, rows) {
      Ok(frames) => {
        if let Some(ref cp) = cache_path {
          let cached = CachedVideo {
            cols,
            rows,
            frames: frames.clone(),
          };
          if let Ok(bytes) = bincode::serialize(&cached) {
            if let Some(parent) = cp.parent() {
              let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(cp, bytes);
          }
        }
        self.finish_load(frames);
      },
      Err(_) => {
        self.load_error = true;
      },
    }
  }

  fn finish_load(&mut self, frames: Vec<CachedFrame>) {
    let mut acc = 0u64;
    let mut cum = Vec::with_capacity(frames.len());
    for f in &frames {
      cum.push(acc);
      acc += f.delay_ms.max(1) as u64;
    }
    self.total_ms = acc.max(1);
    self.cum_ms = cum;
    self.frames = frames;
    self.load_error = self.frames.is_empty();
  }

  fn decode_and_build(
    &self,
    cols: u16,
    rows: u16,
  ) -> Result<Vec<CachedFrame>, ()> {
    let file = fs::File::open(&self.opts.path).map_err(|_| ())?;
    let decoder = GifDecoder::new(BufReader::new(file)).map_err(|_| ())?;
    let charset: Vec<char> = self.opts.charset.chars().collect();
    let charset = if charset.is_empty() {
      DEFAULT_CHARSET.chars().collect::<Vec<_>>()
    } else {
      charset
    };

    let mut out = Vec::new();
    for frame in decoder.into_frames() {
      let frame = frame.map_err(|_| ())?;
      let delay_ms: u32 = {
        let (n, d) = frame.delay().numer_denom_ms();
        if d == 0 { 100 } else { n / d.max(1) }
      };
      let buf = frame.into_buffer();
      let cells =
        Self::build_cells(&buf, cols, rows, self.opts.scale_mode, &charset);
      out.push(CachedFrame {
        cells,
        delay_ms: delay_ms.max(20),
      });
    }
    if out.is_empty() {
      return Err(());
    }
    Ok(out)
  }

  fn build_cells(
    src: &RgbaImage,
    cols: u16,
    rows: u16,
    mode: ScaleMode,
    charset: &[char],
  ) -> Vec<CachedCell> {
    let canvas_w = cols.max(1) as u32;
    let canvas_h = rows.max(1) as u32 * 2;
    let src_w = src.width().max(1) as f32;
    let src_h = src.height().max(1) as f32;

    let canvas: RgbaImage = match mode {
      ScaleMode::Stretch => {
        image::imageops::resize(src, canvas_w, canvas_h, FilterType::Triangle)
      },
      ScaleMode::Fit => {
        let scale = (canvas_w as f32 / src_w).min(canvas_h as f32 / src_h);
        let rw = ((src_w * scale).round() as u32).max(1);
        let rh = ((src_h * scale).round() as u32).max(1);
        let resized =
          image::imageops::resize(src, rw, rh, FilterType::Triangle);
        let mut c: RgbaImage =
          ImageBuffer::from_pixel(canvas_w, canvas_h, Rgba([0, 0, 0, 0]));
        let ox = ((canvas_w as i64 - rw as i64) / 2).max(0) as i64;
        let oy = ((canvas_h as i64 - rh as i64) / 2).max(0) as i64;
        image::imageops::overlay(&mut c, &resized, ox, oy);
        c
      },
      ScaleMode::Fill => {
        let scale = (canvas_w as f32 / src_w).max(canvas_h as f32 / src_h);
        let rw = ((src_w * scale).round() as u32).max(canvas_w);
        let rh = ((src_h * scale).round() as u32).max(canvas_h);
        let resized =
          image::imageops::resize(src, rw, rh, FilterType::Triangle);
        let x = (rw - canvas_w) / 2;
        let y = (rh - canvas_h) / 2;
        image::imageops::crop_imm(&resized, x, y, canvas_w, canvas_h).to_image()
      },
    };

    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    for row in 0..rows {
      for col in 0..cols {
        let y0 = row as u32 * 2;
        let p0 = canvas.get_pixel(col as u32, y0);
        let p1 = canvas.get_pixel(col as u32, (y0 + 1).min(canvas_h - 1));

        let r = ((p0[0] as u16 + p1[0] as u16) / 2) as u8;
        let g = ((p0[1] as u16 + p1[1] as u16) / 2) as u8;
        let b = ((p0[2] as u16 + p1[2] as u16) / 2) as u8;
        let a = ((p0[3] as u16 + p1[3] as u16) / 2) as u8;

        let ch = if a < 16 {
          ' '
        } else {
          let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
          let idx =
            ((luma / 255.0) * (charset.len() - 1) as f32).round() as usize;
          charset[idx.min(charset.len() - 1)]
        };

        cells.push(CachedCell { ch, r, g, b, a });
      }
    }
    cells
  }

  fn current_frame_index(&self) -> usize {
    if self.frames.is_empty() {
      return 0;
    }
    let elapsed = (Instant::now().duration_since(self.start).as_millis()
      as u64)
      % self.total_ms;
    match self.cum_ms.binary_search(&elapsed) {
      Ok(i) => i,
      Err(i) => i.saturating_sub(1),
    }
  }
}

impl Animation for Video {
  fn resize(&mut self, area: Rect) {
    if area.width == self.width
      && area.height == self.height
      && !self.frames.is_empty()
    {
      return;
    }
    self.width = area.width;
    self.height = area.height;
    self.load_or_build(area.width, area.height);
  }

  fn step(&mut self) {
    if self.load_error || self.frames.is_empty() {
      return;
    }
    self.cur = self.current_frame_index();
  }

  fn render(&self, area: Rect, buf: &mut Buffer) {
    if self.load_error || self.frames.is_empty() {
      return;
    }
    let frame = &self.frames[self.cur.min(self.frames.len() - 1)];
    let w = self.width as usize;

    for ly in 0..self.height {
      for lx in 0..self.width {
        let idx = ly as usize * w + lx as usize;
        let Some(cell) = frame.cells.get(idx) else {
          continue;
        };
        if cell.a < 16 {
          continue;
        }
        let x = area.x + lx;
        let y = area.y + ly;
        if let Some(out) = buf.cell_mut(Position { x, y }) {
          out.set_char(cell.ch);
          out.set_fg(Color::Rgb(cell.r, cell.g, cell.b));
          out.set_bg(Color::Reset);
        }
      }
    }
  }
}
