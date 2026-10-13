//! Display width of text that carries ANSI escape sequences (SGR colours, OSC 8 links).

use unicode_width::UnicodeWidthChar;

/// Walks `line`, yielding escape sequences whole and visible characters one by one.
fn segments(line: &str) -> impl Iterator<Item = (&str, bool)> {
    let mut rest = line;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let len = escape_len(rest).unwrap_or_else(|| rest.chars().next().map_or(1, char::len_utf8));
        let is_escape = rest.starts_with('\x1b');
        let (head, tail) = rest.split_at(len);
        rest = tail;
        Some((head, !is_escape))
    })
}

/// Byte length of the escape sequence at the start of `s`, if any: CSI up to its final byte
/// in `@..~`, OSC up to BEL or `ESC \`.
fn escape_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&0x1b) {
        return None;
    }
    match bytes.get(1) {
        Some(b'[') => bytes[2..].iter().position(|b| (b'@'..=b'~').contains(b)).map(|i| i + 3).or(Some(bytes.len())),
        Some(b']') => {
            let body = &bytes[2..];
            let end = body.iter().enumerate().find_map(|(i, b)| match b {
                0x07 => Some(i + 1),
                0x1b if body.get(i + 1) == Some(&b'\\') => Some(i + 2),
                _ => None,
            });
            Some(end.map_or(bytes.len(), |e| e + 2))
        }
        Some(_) => Some(2),
        None => Some(1),
    }
}

/// Columns the text occupies on screen.
pub fn width(line: &str) -> usize {
    segments(line).filter(|(_, visible)| *visible).map(|(c, _)| c.chars().next().and_then(|c| c.width()).unwrap_or(0)).sum()
}

/// Cuts the text to `cols` display columns, keeping escape sequences intact.
pub fn fit(line: &str, cols: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut used = 0;
    for (segment, visible) in segments(line) {
        if visible {
            let w = segment.chars().next().and_then(|c| c.width()).unwrap_or(0);
            if used + w > cols {
                break;
            }
            used += w;
        }
        out.push_str(segment);
    }
    out
}

/// Splits the text into pieces of at most `cols` display columns. Each piece after the first
/// starts with the colours (SGR) and hyperlink still open where the previous one ended, so a
/// wrapped line keeps its look.
pub fn wrap(line: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut used = 0;
    // Escapes in effect: SGR since the last reset, and an open OSC 8 link.
    let mut sgr = String::new();
    let mut link: Option<&str> = None;
    for (segment, visible) in segments(line) {
        if visible {
            let w = segment.chars().next().and_then(|c| c.width()).unwrap_or(0);
            if used + w > cols && used > 0 {
                pieces.push(std::mem::take(&mut piece));
                piece.push_str(&sgr);
                piece.extend(link);
                used = 0;
            }
            used += w;
        } else if let Some(params) = segment.strip_prefix("\x1b[").and_then(|s| s.strip_suffix('m')) {
            if params.is_empty() || params == "0" {
                sgr.clear();
            } else {
                sgr.push_str(segment);
            }
        } else if let Some(osc) = segment.strip_prefix("\x1b]8;") {
            // `ESC ] 8 ; params ; URI ST`: an empty URI closes the link.
            let uri = osc.split_once(';').map_or("", |(_, rest)| rest.trim_end_matches(['\x07', '\\']).trim_end_matches('\x1b'));
            link = (!uri.is_empty()).then_some(segment);
        }
        piece.push_str(segment);
    }
    pieces.push(piece);
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_cuts_only_visible_columns() {
        let red = "\x1b[38;2;255;0;0m";
        assert_eq!(fit(&format!("{red}abcdef\x1b[0m"), 3), format!("{red}abc"));
        assert_eq!(fit("日本語", 5), "日本");
        let link = "\x1b]8;;https://x\x1b\\#12\x1b]8;;\x1b\\";
        assert_eq!(width(link), 3);
        assert_eq!(fit(link, 2), "\x1b]8;;https://x\x1b\\#1");
        assert_eq!(width(&format!("{red}日本\x1b[0m")), 4);
        assert_eq!(fit("short", 80), "short");
    }

    #[test]
    fn wrapped_pieces_carry_on_the_open_colours_and_link() {
        let red = "\x1b[31m";
        let bg = "\x1b[48;5;52m";
        assert_eq!(wrap(&format!("{red}{bg}abcde\x1b[0mfg"), 2), [format!("{red}{bg}ab"), format!("{red}{bg}cd"), format!("{red}{bg}e\x1b[0mf"), "g".to_string()]);
        assert_eq!(wrap("日本語", 3), ["日", "本", "語"], "a wide character that does not fit moves on");
        let open = "\x1b]8;;https://x\x1b\\";
        assert_eq!(wrap(&format!("{open}abc\x1b]8;;\x1b\\d"), 2), [format!("{open}ab"), format!("{open}c\x1b]8;;\x1b\\d")]);
        assert_eq!(wrap("", 4), [""]);
    }
}
