//! Code page 437, the character set of the IBM PC.
//!
//! Scene `.nfo` files, `file_id.diz` and old ANSI art are written in it: the
//! bytes above 0x7F are box-drawing pieces, shading blocks and accented
//! letters, not UTF-8. Reading them as UTF-8 turns every border into a run of
//! replacement characters, which is what a `.nfo` preview looked like.
//!
//! The low half also matters. Bytes 0x01–0x1F are printable glyphs here — the
//! smileys, arrows and card suits — and art uses them freely. Only the two that
//! actually lay out the page, tab and newline, are left alone.

/// 0x00–0x1F as CP437 renders them, with tab, line feed and carriage return
/// kept as control characters so the text still has lines.
const LOW: [char; 32] = [
    ' ', '☺', '☻', '♥', '♦', '♣', '♠', '•', '◘', '\t', '\n', '♂', '♀', '\r', '♫', '☼', '►', '◄',
    '↕', '‼', '¶', '§', '▬', '↨', '↑', '↓', '→', '←', '∟', '↔', '▲', '▼',
];

/// 0x80–0xFF.
const HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

pub fn decode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 4);
    for &b in bytes {
        out.push(match b {
            0x00..=0x1F => LOW[b as usize],
            0x7F => '⌂',
            0x80..=0xFF => HIGH[(b - 0x80) as usize],
            _ => b as char,
        });
    }
    out
}

/// Files written in this character set rather than in UTF-8.
pub fn is_dos_art(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, e)| {
        matches!(e.to_ascii_lowercase().as_str(), "nfo" | "diz" | "asc" | "ans")
    }) || name.eq_ignore_ascii_case("file_id.diz")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tables_are_the_right_length() {
        assert_eq!(super::LOW.len(), 32);
        assert_eq!(super::HIGH.len(), 128);
    }

    #[test]
    fn box_drawing_bytes_become_box_drawing_characters() {
        // The double-line frame an .nfo header is built from.
        assert_eq!(super::decode(&[0xC9, 0xCD, 0xBB]), "╔═╗");
        assert_eq!(super::decode(&[0xC8, 0xCD, 0xBC]), "╚═╝");
        assert_eq!(super::decode(&[0xB0, 0xB1, 0xB2, 0xDB]), "░▒▓█");
    }

    #[test]
    fn ascii_passes_through_unchanged() {
        assert_eq!(super::decode(b"Hello, world!"), "Hello, world!");
    }

    /// Layout has to survive: tabs and newlines stay control characters even
    /// though CP437 has glyphs at those code points.
    #[test]
    fn line_breaks_are_preserved() {
        assert_eq!(super::decode(b"a\r\nb\tc"), "a\r\nb\tc");
    }

    #[test]
    fn low_bytes_are_the_printable_glyphs() {
        assert_eq!(super::decode(&[0x01, 0x02, 0x10, 0x11]), "☺☻►◄");
    }

    #[test]
    fn every_byte_decodes_to_exactly_one_character() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(super::decode(&all).chars().count(), 256);
    }

    #[test]
    fn extension_check() {
        assert!(super::is_dos_art("release.nfo"));
        assert!(super::is_dos_art("FILE_ID.DIZ"));
        assert!(super::is_dos_art("art.ANS"));
        assert!(!super::is_dos_art("readme.txt"));
        assert!(!super::is_dos_art("nfo"));
    }
}
