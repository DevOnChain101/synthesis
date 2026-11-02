//! The brand symbol as braille art (table in `logo_art.rs`, generated offline by
//! `tools/logo-art/gen.py`) with a per-cell sheen: dark cells `MUTED`, then `TEXT`, `ACCENT`,
//! `ACCENT_LIGHT` and `PEARL` for the lightest. On a 16-colour or colourless terminal (`plain`)
//! every cell is the one accent colour.

use crate::logo_art::{ART, COLS, ROWS, SHADE, SHADES};
use crate::theme::{ACCENT, ACCENT_LIGHT, MUTED, PEARL, Rgb, TEXT, lerp};

pub const ART_COLS: usize = COLS;
pub const ART_ROWS: usize = ROWS;

fn sheen(shade: u8, plain: bool) -> Rgb {
    if plain {
        return ACCENT;
    }
    let stops = [MUTED, TEXT, ACCENT, ACCENT_LIGHT, PEARL];
    let pos = f64::from(shade) / f64::from(SHADES - 1) * (stops.len() - 1) as f64;
    let i = (pos.floor() as usize).min(stops.len() - 2);
    lerp(stops[i], stops[i + 1], pos - i as f64)
}

/// `ROWS` rows of `COLS` cells: the character (a plain space where nothing is drawn) and its colour.
pub fn art_cells(plain: bool) -> Vec<Vec<(char, Rgb)>> {
    ART.iter()
        .zip(SHADE.iter())
        .map(|(line, shades)| {
            line.chars()
                .zip(shades)
                .map(|(c, s)| (if c == '\u{2800}' { ' ' } else { c }, sheen(*s, plain)))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_is_16_by_8_and_plain_is_one_colour() {
        let a = art_cells(false);
        assert_eq!((a.len(), a[0].len()), (ROWS, COLS));
        let plain = art_cells(true);
        let colours: std::collections::BTreeSet<_> = plain.iter().flatten().map(|c| c.1).collect();
        assert_eq!(colours.len(), 1);
        let sheen: std::collections::BTreeSet<_> = a.iter().flatten().map(|c| c.1).collect();
        assert!(sheen.len() > 3);
    }
}
