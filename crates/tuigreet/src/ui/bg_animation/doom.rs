//! DOOM-style fire effect, ported from Ly's `src/animations/Doom.zig`.

use std::time::{SystemTime, UNIX_EPOCH};

use rand::{RngExt, SeedableRng, prelude::StdRng};
use tui::{
  buffer::Buffer,
  layout::{Position, Rect},
  style::Color,
};

use super::Animation;

/// Number of fire levels (matches Ly's `STEPS = 12`).
const STEPS: u8 = 12;

/// Upper bound for the decay roll (matches Ly's `HEIGHT_MAX = 9`).
const HEIGHT_MAX: u8 = 9;

/// Sentinel palette index meaning [`Color::Reset`].
const RESET: u8 = u8::MAX;

/// Per-level `(glyph, fg_palette_idx, bg_palette_idx)` lookup.
const GLYPHS: [(char, u8, u8); STEPS as usize + 1] = [
  (' ', RESET, RESET), // 0  unused, see render()
  ('░', 0, RESET),     // 1  top band
  ('▒', 0, RESET),     // 2
  ('▓', 0, RESET),     // 3
  ('█', 0, RESET),     // 4
  ('░', 1, 0),         // 5  middle band over top
  ('▒', 1, 0),         // 6
  ('▓', 1, 0),         // 7
  ('█', 1, 0),         // 8
  ('░', 2, 1),         // 9  bottom band over middle
  ('▒', 2, 1),         // 10
  ('▓', 2, 1),         // 11
  ('█', 2, 1),         // 12
];

/// Configurable parameters for the fire effect.
#[derive(Debug, Clone)]
pub struct Options {
  /// Decay control (taller flames at higher values). Clamped to 1..=9.
  pub height: u8,
  /// Horizontal jitter. Clamped to 0..=4.
  pub spread: u8,
  /// Color of the coolest flame tips.
  pub top: Color,
  /// Color of the mid-band flames.
  pub middle: Color,
  /// Color of the hottest flames.
  pub bottom: Color,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      height: 6,
      spread: 2,
      top: Color::Rgb(0x9F, 0x27, 0x07),
      middle: Color::Rgb(0xC7, 0x8F, 0x17),
      bottom: Color::Rgb(0xFF, 0xFF, 0xFF),
    }
  }
}

pub struct Doom {
  width: u16,
  height: u16,
  buf: Vec<u8>,
  opts: Options,
  rng: StdRng,
}

impl Doom {
  pub fn new(mut opts: Options) -> Self {
    opts.height = opts.height.clamp(1, HEIGHT_MAX);
    opts.spread = opts.spread.min(4);

    let seed = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_nanos() as u64)
      .unwrap_or(0);

    Self {
      width: 0,
      height: 0,
      buf: Vec::new(),
      opts,
      rng: StdRng::seed_from_u64(seed),
    }
  }

  /// Reset the buffer: cool everywhere, hottest along the bottom row.
  fn init_buffer(&mut self) {
    self.buf.fill(0);
    if self.width == 0 || self.height == 0 {
      return;
    }
    let w = self.width as usize;
    let bot = (self.height as usize - 1) * w;
    for cell in &mut self.buf[bot..bot + w] {
      *cell = STEPS;
    }
  }

  /// Resolve a fire level to its `(glyph, fg, bg)` triple.
  fn glyph(&self, level: u8) -> (char, Color, Color) {
    let palette = [self.opts.top, self.opts.middle, self.opts.bottom];
    let resolve = |i: u8| {
      if i == RESET {
        Color::Reset
      } else {
        palette[i as usize]
      }
    };
    let (ch, fg, bg) = GLYPHS[(level.min(STEPS)) as usize];
    (ch, resolve(fg), resolve(bg))
  }
}

impl Animation for Doom {
  fn resize(&mut self, area: Rect) {
    if area.width == self.width
      && area.height == self.height
      && !self.buf.is_empty()
    {
      return;
    }
    self.width = area.width;
    self.height = area.height;
    self
      .buf
      .resize(self.width as usize * self.height as usize, 0);
    self.init_buffer();
  }

  fn step(&mut self) {
    if self.width == 0 || self.height < 2 {
      return;
    }
    let w = self.width as usize;
    let h = self.height as usize;
    let spread = self.opts.spread as usize;
    let height = self.opts.height;

    for y in 1..h {
      for x in 0..w {
        let from = y * w + x;
        let level = self.buf[from];

        let rand_loss = self.rng.random_range(0..=HEIGHT_MAX);
        let rand_spread = self.rng.random_range(0..=2 * self.opts.spread);

        let to = from
          .saturating_sub(w)
          .saturating_add(spread)
          .saturating_sub(rand_spread as usize);

        let new_level = if rand_loss >= height {
          level.saturating_sub(1)
        } else {
          level
        };

        if to < self.buf.len() {
          self.buf[to] = new_level;
        }
      }
    }

    let bot = (h - 1) * w;
    for cell in &mut self.buf[bot..bot + w] {
      *cell = STEPS;
    }
  }

  fn render(&self, area: Rect, buf: &mut Buffer) {
    if self.width == 0 || self.height == 0 {
      return;
    }
    let w = self.width as usize;

    for ly in 0..self.height {
      for lx in 0..self.width {
        let level = self.buf[ly as usize * w + lx as usize];
        if level == 0 {
          continue;
        }
        let x = area.x + lx;
        let y = area.y + ly;
        if let Some(cell) = buf.cell_mut(Position { x, y }) {
          let (ch, fg, bg) = self.glyph(level);
          cell.set_char(ch);
          cell.set_fg(fg);
          cell.set_bg(bg);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bottom_row_is_hot_after_init_and_step() {
    let mut d = Doom::new(Options::default());
    d.resize(Rect::new(0, 0, 16, 8));
    let w = 16usize;
    let bot = 7 * w;
    for i in 0..w {
      assert_eq!(d.buf[bot + i], STEPS, "init: bottom row must be STEPS");
    }
    d.step();
    for i in 0..w {
      assert_eq!(d.buf[bot + i], STEPS, "step: bottom row must stay STEPS");
    }
  }

  #[test]
  fn heat_propagates_upward() {
    let mut d = Doom::new(Options {
      height: 9,
      spread: 2,
      ..Options::default()
    });
    d.resize(Rect::new(0, 0, 32, 12));
    for _ in 0..100 {
      d.step();
    }
    let w = 32usize;
    let top_has_fire = d.buf[0..w].iter().any(|&v| v > 0);
    assert!(top_has_fire, "fire should reach the top after many steps");
  }

  #[test]
  fn resize_changes_buffer() {
    let mut d = Doom::new(Options::default());
    d.resize(Rect::new(0, 0, 8, 4));
    assert_eq!(d.buf.len(), 32);
    d.resize(Rect::new(0, 0, 16, 8));
    assert_eq!(d.buf.len(), 128);
  }

  #[test]
  fn options_clamp_out_of_range_values() {
    let d = Doom::new(Options {
      height: 99,
      spread: 99,
      ..Options::default()
    });
    assert_eq!(d.opts.height, HEIGHT_MAX);
    assert_eq!(d.opts.spread, 4);
  }
}
