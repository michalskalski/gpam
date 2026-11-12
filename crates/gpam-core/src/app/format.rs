/// Human-readable elapsed seconds.
pub fn format_age(secs: i64) -> String {
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Cut `s` to at most `n` characters, marking a cut with a trailing `~`.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('~');
        out
    }
}

/// Text from outside made safe to show: control characters, which include
/// terminal escape sequences and line breaks, become spaces, and characters
/// that reorder or hide text are dropped. Use it on anything another person
/// wrote, such as a grant justification.
pub fn plain_text(s: &str) -> String {
    s.chars()
        .filter(|c| !is_invisible_format(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Bidirectional overrides and isolates, zero-width characters and the byte
/// order mark. They change how the text around them reads without showing.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_neutralises_control_and_reordering_characters() {
        assert_eq!(plain_text("fix outage"), "fix outage");
        // An escape sequence loses its escape and so its effect.
        assert_eq!(plain_text("ok\u{1b}[2Jdone"), "ok [2Jdone");
        assert_eq!(plain_text("line one\nline two\r\n"), "line one line two  ");
        // A right-to-left override would show the text after it reversed.
        assert_eq!(plain_text("user\u{202E}nimda"), "usernimda");
        assert_eq!(plain_text("a\u{200B}b\u{FEFF}"), "ab");
        // Ordinary non-ASCII text is kept.
        assert_eq!(plain_text("zażółć 日本語"), "zażółć 日本語");
    }

    #[test]
    fn format_age_units() {
        assert_eq!(format_age(30), "30s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(3700), "1h");
        assert_eq!(format_age(90000), "1d");
    }

    #[test]
    fn truncate_marks_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefgh", 5), "abcd~");
    }
}
