//! cmatrix-style digital rain.
//!
//! Each terminal column owns at most one falling stream at a time. A stream
//! is described by a fractional head row, a length, and a fall speed. Each
//! frame the head advances by `speed` rows; whenever its integer part moves
//! down by one, a fresh random glyph is stamped at the new head row and the
//! existing trail ages by one. Cells render with a three-band gradient
//! (head / body / dim tail) until their age exceeds the stream length, at
//! which point they go dark again.
//!
//! When a stream's head leaves the screen, the column waits a random
//! cooldown then spawns a fresh stream from above the top edge.

use std::time::{SystemTime, UNIX_EPOCH};

use rand::{RngExt, SeedableRng, prelude::StdRng};
use tui::{
  buffer::Buffer,
  layout::{Position, Rect},
  style::Color,
};

use super::Animation;

/// Default glyph palette: half-width katakana plus ASCII digits and a few
/// symbols. Half-width katakana (U+FF65..U+FF9F) are single-cell on
/// terminals that respect Unicode East Asian Width.
const DEFAULT_CHARSET: &[char] = &[
  'ｦ', 'ｧ', 'ｨ', 'ｩ', 'ｪ', 'ｫ', 'ｬ', 'ｭ', 'ｮ', 'ｯ', 'ｰ', 'ｱ', 'ｲ', 'ｳ', 'ｴ',
  'ｵ', 'ｶ', 'ｷ', 'ｸ', 'ｹ', 'ｺ', 'ｻ', 'ｼ', 'ｽ', 'ｾ', 'ｿ', 'ﾀ', 'ﾁ', 'ﾂ', 'ﾃ',
  'ﾄ', 'ﾅ', 'ﾆ', 'ﾇ', 'ﾈ', 'ﾉ', 'ﾊ', 'ﾋ', 'ﾌ', 'ﾍ', 'ﾎ', 'ﾏ', 'ﾐ', 'ﾑ', 'ﾒ',
  'ﾓ', 'ﾔ', 'ﾕ', 'ﾖ', 'ﾗ', 'ﾘ', 'ﾙ', 'ﾚ', 'ﾛ', 'ﾜ', 'ﾝ', '0', '1', '2', '3',
  '4', '5', '6', '7', '8', '9', '*', '+', '=', '%', '#', '$', '&', '@', '/',
  '\\', '|', '<', '>', '?',
];

/// Configurable parameters for the matrix effect.
#[derive(Debug, Clone)]
pub struct Options {
  /// Color of the leading glyph in each falling stream.
  pub head: Color,

  /// Color of the brightest part of the trail (just behind the head).
  pub bright: Color,

  /// Color of the dim tail before each glyph fades out.
  pub dim: Color,

  /// Inclusive minimum trail length, in rows.
  pub min_length: u16,

  /// Inclusive maximum trail length, in rows. Clamped to `>= min_length`.
  pub max_length: u16,

  /// Inclusive minimum stream speed, in rows-per-frame. Lower = slower.
  pub min_speed: f32,

  /// Inclusive maximum stream speed, in rows-per-frame. Clamped to
  /// `>= min_speed`.
  pub max_speed: f32,

  /// Probability per frame, per active cell, of replacing the glyph with a
  /// fresh random one — gives the trail its faint shimmer. `0.0` disables.
  pub mutate_chance: f32,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      head: Color::Rgb(0xCC, 0xFF, 0xCC),
      bright: Color::Rgb(0x33, 0xFF, 0x66),
      dim: Color::Rgb(0x00, 0x66, 0x22),
      min_length: 6,
      max_length: 18,
      min_speed: 0.30,
      max_speed: 1.10,
      mutate_chance: 0.02,
    }
  }
}

/// Per-column state. Each column has at most one active stream; when it
/// runs off the bottom of the screen we wait `cooldown` frames before
/// spawning a fresh one above the top edge.
#[derive(Clone, Copy, Debug)]
struct Column {
  /// Fractional head row. May be negative — that means the head is still
  /// above the top edge and the stream is "warming up".
  head_y: f32,
  /// Length of the trail in rows.
  length: u16,
  /// Rows per frame.
  speed: f32,
  /// `true` while the stream is on screen or warming up; `false` while in
  /// cooldown waiting to respawn.
  active: bool,
  /// Frames remaining before respawn when `!active`.
  cooldown: u16,
}

/// One painted cell. `age == 0` means empty; `age == 1` is the head row of
/// the column's stream; `age == n` is `n - 1` rows behind the head.
#[derive(Clone, Copy, Debug, Default)]
struct Cell {
  ch: char,
  age: u16,
}

pub struct Matrix {
  width: u16,
  height: u16,
  cols: Vec<Column>,
  cells: Vec<Cell>,
  opts: Options,
  rng: StdRng,
}

impl Matrix {
  pub fn new(mut opts: Options) -> Self {
    opts.max_length = opts.max_length.max(opts.min_length).max(1);
    opts.min_length = opts.min_length.max(1);
    opts.max_speed = opts.max_speed.max(opts.min_speed).max(0.05);
    opts.min_speed = opts.min_speed.max(0.05);
    opts.mutate_chance = opts.mutate_chance.clamp(0.0, 1.0);

    let seed = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_nanos() as u64)
      .unwrap_or(0);

    Self {
      width: 0,
      height: 0,
      cols: Vec::new(),
      cells: Vec::new(),
      opts,
      rng: StdRng::seed_from_u64(seed),
    }
  }

  fn random_glyph(&mut self) -> char {
    let i = self.rng.random_range(0..DEFAULT_CHARSET.len());
    DEFAULT_CHARSET[i]
  }

  /// Pick a fresh `(length, speed)` for a respawning stream.
  fn random_stream(&mut self) -> (u16, f32) {
    let length = self
      .rng
      .random_range(self.opts.min_length..=self.opts.max_length);
    let speed_lo = (self.opts.min_speed * 1000.0) as u32;
    let speed_hi = (self.opts.max_speed * 1000.0) as u32;
    let speed =
      self.rng.random_range(speed_lo..=speed_hi.max(speed_lo)) as f32 / 1000.0;
    (length, speed)
  }

  /// (Re)initialize all columns. Called from `resize` whenever the
  /// terminal area changes shape.
  fn init_columns(&mut self) {
    self.cols.clear();
    self.cells.clear();
    if self.width == 0 || self.height == 0 {
      return;
    }
    let cells_len = self.width as usize * self.height as usize;
    self.cells.resize(cells_len, Cell::default());

    let h = self.height as i16;
    for _ in 0..self.width as usize {
      let (length, speed) = self.random_stream();
      // Stagger initial heads across and above the screen so columns don't
      // all start in lockstep.
      let head_y = self.rng.random_range(-(h as i32)..(h as i32)) as f32;
      self.cols.push(Column {
        head_y,
        length,
        speed,
        active: true,
        cooldown: 0,
      });
    }
  }
}

impl Animation for Matrix {
  fn resize(&mut self, area: Rect) {
    if area.width == self.width
      && area.height == self.height
      && !self.cells.is_empty()
    {
      return;
    }
    self.width = area.width;
    self.height = area.height;
    self.init_columns();
  }

  fn step(&mut self) {
    if self.width == 0 || self.height == 0 {
      return;
    }
    let w = self.width as usize;
    let h = self.height as i32;

    // Snapshot column count to avoid borrowing self.cols while we mutate
    // self.rng / self.cells inside the loop.
    let col_count = self.cols.len();
    for x in 0..col_count {
      let col = self.cols[x];

      if !col.active {
        let cooldown = col.cooldown.saturating_sub(1);
        if cooldown == 0 {
          let (length, speed) = self.random_stream();
          self.cols[x] = Column {
            head_y: -(self.rng.random_range(0..(h.max(1) / 2 + 1)) as f32),
            length,
            speed,
            active: true,
            cooldown: 0,
          };
        } else {
          self.cols[x].cooldown = cooldown;
        }
        continue;
      }

      let prev_int = col.head_y.floor() as i32;
      let new_y = col.head_y + col.speed;
      let new_int = new_y.floor() as i32;

      // Stamp a glyph at every integer row the head crossed during this
      // step (the head can advance more than one row per frame at high
      // speeds).
      for row in (prev_int + 1)..=new_int {
        if row >= 0 && row < h {
          let ch = self.random_glyph();
          let idx = row as usize * w + x;
          self.cells[idx] = Cell { ch, age: 1 };
        }
      }

      self.cols[x].head_y = new_y;

      // Once the head falls past the bottom plus the trail, the column is
      // done — go dormant for a moment then respawn.
      if new_int >= h + col.length as i32 {
        let cooldown = self.rng.random_range(2..=20);
        self.cols[x].active = false;
        self.cols[x].cooldown = cooldown;
      }
    }

    // Age every painted cell, fade out anything older than the column's
    // current trail length, and occasionally mutate a glyph for shimmer.
    for x in 0..w {
      let max_age = self.cols[x].length;
      for y in 0..self.height as usize {
        let idx = y * w + x;
        let cell = self.cells[idx];
        if cell.age == 0 {
          continue;
        }
        let next_age = cell.age + 1;
        if next_age > max_age {
          self.cells[idx] = Cell::default();
          continue;
        }
        let ch = if self.opts.mutate_chance > 0.0
          && self.rng.random_bool(self.opts.mutate_chance as f64)
        {
          self.random_glyph()
        } else {
          cell.ch
        };
        self.cells[idx] = Cell { ch, age: next_age };
      }
    }
  }

  fn render(&self, area: Rect, buf: &mut Buffer) {
    if self.width == 0 || self.height == 0 {
      return;
    }
    let w = self.width as usize;
    for ly in 0..self.height {
      for lx in 0..self.width {
        let cell = self.cells[ly as usize * w + lx as usize];
        if cell.age == 0 {
          continue;
        }
        let length = self.cols[lx as usize].length.max(1);
        let fg = if cell.age == 1 {
          self.opts.head
        } else if cell.age * 3 <= length {
          self.opts.bright
        } else {
          self.opts.dim
        };
        let x = area.x + lx;
        let y = area.y + ly;
        if let Some(out) = buf.cell_mut(Position { x, y }) {
          out.set_char(cell.ch);
          out.set_fg(fg);
          out.set_bg(Color::Reset);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fixed(opts: Options) -> Matrix {
    let mut m = Matrix::new(opts);
    // Replace the time-seeded RNG with a deterministic one so the
    // assertions below are stable.
    m.rng = StdRng::seed_from_u64(42);
    m
  }

  #[test]
  fn options_normalize_inverted_ranges() {
    let m = Matrix::new(Options {
      min_length: 20,
      max_length: 5,
      min_speed: 2.0,
      max_speed: 0.1,
      ..Options::default()
    });
    assert!(m.opts.max_length >= m.opts.min_length);
    assert!(m.opts.max_speed >= m.opts.min_speed);
  }

  #[test]
  fn streams_paint_into_buffer_after_some_steps() {
    let mut m = fixed(Options::default());
    m.resize(Rect::new(0, 0, 40, 20));
    for _ in 0..30 {
      m.step();
    }
    let painted = m.cells.iter().filter(|c| c.age > 0).count();
    assert!(painted > 0, "matrix should paint something after 30 frames");
  }

  #[test]
  fn cells_fade_within_trail_length() {
    // After many frames every painted cell's age must be <= the column's
    // current trail length.
    let mut m = fixed(Options::default());
    m.resize(Rect::new(0, 0, 16, 12));
    for _ in 0..200 {
      m.step();
    }
    let w = 16usize;
    for x in 0..w {
      let max_age = m.cols[x].length;
      for y in 0..12 {
        let cell = m.cells[y * w + x];
        if cell.age > 0 {
          assert!(
            cell.age <= max_age,
            "col {x} row {y}: age {} > length {}",
            cell.age,
            max_age
          );
        }
      }
    }
  }

  #[test]
  fn resize_changes_buffer_shape() {
    let mut m = fixed(Options::default());
    m.resize(Rect::new(0, 0, 8, 4));
    assert_eq!(m.cells.len(), 32);
    assert_eq!(m.cols.len(), 8);
    m.resize(Rect::new(0, 0, 16, 6));
    assert_eq!(m.cells.len(), 96);
    assert_eq!(m.cols.len(), 16);
  }
}
