//! The Steam login challenge drawn as a QR code for a terminal. The shell
//! draws the same payload with `CoreImage`; here it is Unicode half blocks,
//! which any monospace font has, so the Steam mobile app can scan the
//! terminal itself. The URL under it stays for a terminal that cannot.

use qrcode::{Color, QrCode};

/// Light modules around the code. The QR standard asks for four; one is
/// enough for a scanner when the terminal's own margin is behind it, and the
/// code stays small enough for a short terminal.
const QUIET_ZONE: usize = 1;

/// `payload` as rows of half-block characters. One character cell holds two
/// modules, the upper and the lower; a dark module is drawn with the
/// foreground, so the code reads with the terminal's text colour. `None` when
/// the payload is over what a QR code can hold.
#[must_use]
pub fn half_blocks(payload: &str) -> Option<String> {
    let code = QrCode::new(payload.as_bytes()).ok()?;
    Some(draw(code.width(), &code.to_colors()))
}

fn draw(width: usize, modules: &[Color]) -> String {
    let side = width + 2 * QUIET_ZONE;
    let dark = |row: usize, column: usize| {
        let (Some(row), Some(column)) =
            (row.checked_sub(QUIET_ZONE), column.checked_sub(QUIET_ZONE))
        else {
            return false;
        };
        row < width && column < width && modules[row * width + column] == Color::Dark
    };
    let rows = (0..side).step_by(2).map(|upper| {
        (0..side)
            .map(
                |column| match (dark(upper, column), dark(upper + 1, column)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                },
            )
            .collect::<String>()
    });
    rows.collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a challenge Steam issued live on 2026-11-06, with the
    /// digits replaced.
    const CHALLENGE: &str = "https://s.team/q/1/1234567890123456";

    /// The drawing read back as modules, row-major, `true` for dark.
    fn modules(text: &str) -> Vec<Vec<bool>> {
        let mut rows = Vec::new();
        for line in text.lines() {
            let (mut upper, mut lower) = (Vec::new(), Vec::new());
            for cell in line.chars() {
                upper.push(matches!(cell, '█' | '▀'));
                lower.push(matches!(cell, '█' | '▄'));
            }
            rows.push(upper);
            rows.push(lower);
        }
        rows
    }

    fn finder_at(rows: &[Vec<bool>], top: usize, left: usize) -> bool {
        (0..7).all(|y| {
            (0..7).all(|x| {
                let ring = y == 0 || y == 6 || x == 0 || x == 6;
                let centre = (2..=4).contains(&y) && (2..=4).contains(&x);
                rows[top + y][left + x] == (ring || centre)
            })
        })
    }

    #[test]
    fn the_challenge_draws_to_the_golden() {
        let drawn = half_blocks(CHALLENGE).unwrap();
        assert_eq!(
            format!("{drawn}\n"),
            include_str!("../tests/golden/qr_half_blocks.txt")
        );
    }

    #[test]
    fn the_drawing_is_a_qr_symbol_with_its_quiet_zone_and_finders() {
        let drawn = half_blocks(CHALLENGE).unwrap();
        let lines: Vec<&str> = drawn.lines().collect();
        let side = lines[0].chars().count();
        assert!(lines.iter().all(|line| line.chars().count() == side));
        assert_eq!(lines.len(), side.div_ceil(2));
        let width = side - 2 * QUIET_ZONE;
        assert!((1..=40).any(|version| 4 * version + 17 == width), "{width}");

        let rows = modules(&drawn);
        let edge = |row: &Vec<bool>| row.iter().all(|dark| !dark);
        assert!(edge(&rows[0]) && edge(&rows[side - 1]));
        assert!(rows[..side].iter().all(|row| !row[0] && !row[side - 1]));
        assert!(rows[side..].iter().all(edge), "the virtual row is light");

        let far = side - QUIET_ZONE - 7;
        assert!(finder_at(&rows, QUIET_ZONE, QUIET_ZONE));
        assert!(finder_at(&rows, QUIET_ZONE, far));
        assert!(finder_at(&rows, far, QUIET_ZONE));
        assert!(!finder_at(&rows, far, far), "no fourth finder");
    }

    #[test]
    fn a_payload_over_the_capacity_is_not_drawn() {
        assert!(half_blocks(&"x".repeat(3000)).is_none());
        assert!(half_blocks("").is_some());
    }
}
