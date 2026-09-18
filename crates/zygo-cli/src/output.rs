//! Terminal output helpers.
//!
//! Two rules, both from the design doc's DX goals: every failure names the
//! primitive that failed and how to fix it, and every command can produce JSON
//! so a platform can consume it.

use std::io::IsTerminal;

/// ANSI styling, suppressed when stdout is not a terminal or `NO_COLOR` is set.
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn stdout() -> Self {
        Self {
            enabled: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn stderr() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn green(&self, t: &str) -> String {
        self.paint("32", t)
    }
    pub fn yellow(&self, t: &str) -> String {
        self.paint("33", t)
    }
    pub fn red(&self, t: &str) -> String {
        self.paint("31", t)
    }
    pub fn dim(&self, t: &str) -> String {
        self.paint("2", t)
    }
    pub fn bold(&self, t: &str) -> String {
        self.paint("1", t)
    }
}

/// Print an error and its cause chain to stderr.
pub fn error(e: &anyhow::Error) {
    let s = Style::stderr();
    eprintln!("{} {e}", s.red("error:"));
    for cause in e.chain().skip(1) {
        eprintln!("  {} {cause}", s.dim("caused by:"));
    }
}

/// Print a warning line.
pub fn warn(message: &str) {
    let s = Style::stderr();
    eprintln!("{} {message}", s.yellow("warning:"));
}

/// Render rows as a left-aligned table with a header.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
    }

    let mut out = String::new();
    let s = Style::stdout();
    let header: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad(&h.to_uppercase(), widths[i]))
        .collect();
    out.push_str(&s.bold(header.join("  ").trim_end()));
    out.push('\n');

    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths.get(i).copied().unwrap_or(0)))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let w = display_width(text);
    if w >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}

/// Visible width, ignoring ANSI escape sequences so coloured cells still align.
fn display_width(text: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for c in text.chars() {
        if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            width += 1;
        }
    }
    width
}

/// Human-readable byte count.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1u64 << 40, "TB"),
        (1 << 30, "GB"),
        (1 << 20, "MB"),
        (1 << 10, "kB"),
    ];
    for (mult, unit) in UNITS {
        if bytes >= mult {
            return format!("{:.1} {unit}", bytes as f64 / mult as f64);
        }
    }
    format!("{bytes} B")
}

/// Relative time, for "pulled 3 hours ago".
pub fn human_age(unix_seconds: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(unix_seconds);
    match age {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} minutes ago", age / 60),
        3600..=86_399 => format!("{} hours ago", age / 3600),
        _ => format!("{} days ago", age / 86_400),
    }
}

/// Print a value as pretty JSON.
pub fn json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_columns_line_up() {
        let out = table(
            &["name", "size"],
            &[
                vec!["python:3.12".into(), "120 MB".into()],
                vec!["a".into(), "1 B".into()],
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        // Every row starts its second column at the same offset.
        let col = lines[0].find("SIZE").unwrap();
        assert_eq!(lines[1].find("120 MB"), Some(col));
        assert_eq!(lines[2].find("1 B"), Some(col));
    }

    #[test]
    fn table_handles_no_rows() {
        let out = table(&["a", "b"], &[]);
        assert_eq!(out.trim(), "A  B");
    }

    #[test]
    fn ansi_sequences_do_not_break_alignment() {
        assert_eq!(display_width("\x1b[32mok\x1b[0m"), 2);
        assert_eq!(display_width("ok"), 2);
        assert_eq!(
            pad("\x1b[32mok\x1b[0m", 4).len(),
            "\x1b[32mok\x1b[0m".len() + 2
        );
    }

    #[test]
    fn byte_counts_read_naturally() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 kB");
        assert_eq!(human_bytes(120 * (1 << 20)), "120.0 MB");
    }

    #[test]
    fn ages_read_naturally() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(human_age(now), "just now");
        assert_eq!(human_age(now - 120), "2 minutes ago");
        assert_eq!(human_age(now - 7200), "2 hours ago");
        assert_eq!(human_age(now - 200_000), "2 days ago");
        // A timestamp in the future must not underflow into "584 million years".
        assert_eq!(human_age(now + 10_000), "just now");
    }

    #[test]
    fn styling_is_a_no_op_when_disabled() {
        let s = Style { enabled: false };
        assert_eq!(s.green("ok"), "ok");
        let s = Style { enabled: true };
        assert!(s.green("ok").contains("\x1b[32m"));
    }
}
