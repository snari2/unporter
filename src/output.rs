//! Human-readable and JSON output for the CLI.

use std::io::{IsTerminal, Write};

use anstream::{eprintln, println};
use anstyle::{AnsiColor, Style};

use crate::scan::PortEntry;
use crate::util::{human_bytes, human_duration, tilde, truncate_left, truncate_right, width};

pub const BOLD: Style = Style::new().bold();
pub const DIM: Style = Style::new().dimmed();
pub const PORT: Style = AnsiColor::Cyan.on_default().bold();
pub const KIND: Style = AnsiColor::Magenta.on_default();
pub const PROJECT: Style = AnsiColor::Green.on_default();
pub const WARN: Style = AnsiColor::Yellow.on_default();
pub const OK: Style = AnsiColor::Green.on_default().bold();
pub const ERR: Style = AnsiColor::Red.on_default().bold();

/// Project label for an entry: project name, Docker container, or `-`.
pub fn project_label(e: &PortEntry) -> String {
    if let Some(c) = &e.container {
        format!("{c} ({})", e.container_runtime().unwrap_or("docker"))
    } else if let Some(p) = &e.project {
        p.name.clone()
    } else {
        "-".to_owned()
    }
}

/// Where the project lives (or the cwd when no project was found).
pub fn path_label(e: &PortEntry) -> String {
    match (&e.project, &e.cwd) {
        (Some(p), _) => tilde(&p.root),
        (None, Some(cwd)) if !crate::project::is_system_path(cwd) => tilde(cwd),
        _ => "-".to_owned(),
    }
}

struct Column {
    title: &'static str,
    style: Style,
    cells: Vec<String>,
    /// How to shorten this column when the terminal is too narrow.
    shrink: Shrink,
}

#[derive(Clone, Copy, PartialEq)]
enum Shrink {
    No,
    Left,
    Right,
}

pub fn print_table(entries: &[PortEntry], hidden: usize) {
    if entries.is_empty() {
        if hidden > 0 {
            println!(
                "No dev servers are listening. {DIM}({hidden} other listener{} hidden; use --all to show){DIM:#}",
                plural(hidden)
            );
        } else {
            println!("Nothing is listening on any TCP port that unporter can see.");
        }
        return;
    }

    let col = |title, style, shrink, f: &dyn Fn(&PortEntry) -> String| Column {
        title,
        style,
        shrink,
        cells: entries.iter().map(f).collect(),
    };
    let mut columns = vec![
        col("PORT", PORT, Shrink::No, &|e| e.port.to_string()),
        col("PID", Style::new(), Shrink::No, &|e| e.pid.to_string()),
        col("PROCESS", BOLD, Shrink::Right, &|e| e.process.clone()),
        col("KIND", KIND, Shrink::Right, &|e| {
            e.kind.unwrap_or("-").to_owned()
        }),
        col("PROJECT", PROJECT, Shrink::Right, &project_label),
        col("PATH", DIM, Shrink::Left, &path_label),
        col("BIND", Style::new(), Shrink::No, &PortEntry::bind_label),
        col("UPTIME", Style::new(), Shrink::No, &|e| {
            e.uptime_secs
                .map(human_duration)
                .unwrap_or_else(|| "-".into())
        }),
        col("MEM", Style::new(), Shrink::No, &|e| {
            e.memory_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "-".into())
        }),
    ];

    let gap = 2;
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|c| {
            c.cells
                .iter()
                .map(|s| width(s))
                .chain([c.title.len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    if let Some(term) = terminal_width() {
        fit_widths(&columns, &mut widths, term, gap);
    }

    let mut out = String::new();
    for (c, w) in columns.iter().zip(&widths) {
        out.push_str(&format!("{BOLD}{:<w$}{BOLD:#}{:gap$}", c.title, ""));
    }
    println!("{}", out.trim_end());
    for row in 0..entries.len() {
        let mut line = String::new();
        for (c, &w) in columns.iter_mut().zip(&widths) {
            let cell = std::mem::take(&mut c.cells[row]);
            let cell = match c.shrink {
                Shrink::Left => truncate_left(&cell, w),
                Shrink::Right => truncate_right(&cell, w),
                Shrink::No => cell,
            };
            let pad = w.saturating_sub(width(&cell));
            let style = if cell == "-" { DIM } else { c.style };
            line.push_str(&format!("{style}{cell}{style:#}{:pad$}{:gap$}", "", ""));
        }
        println!("{}", line.trim_end());
    }
    if hidden > 0 {
        println!(
            "{DIM}{hidden} other listener{} hidden (system services and apps); use --all to show{DIM:#}",
            plural(hidden)
        );
    }
}

/// Shrink the flexible columns (PATH first, then PROJECT, PROCESS, KIND)
/// until the table fits in `term` columns, never below a readable minimum.
fn fit_widths(columns: &[Column], widths: &mut [usize], term: usize, gap: usize) {
    let total = |w: &[usize]| w.iter().sum::<usize>() + gap * (w.len().saturating_sub(1));
    let order = ["PATH", "PROJECT", "PROCESS", "KIND"];
    for name in order {
        let excess = total(widths).saturating_sub(term);
        if excess == 0 {
            return;
        }
        if let Some(i) = columns.iter().position(|c| c.title == name) {
            let min = 8.max(columns[i].title.len());
            let can = widths[i].saturating_sub(min);
            widths[i] -= can.min(excess);
        }
    }
}

fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size().ok().map(|(w, _)| w as usize)
}

pub fn print_details(entries: &[PortEntry]) {
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let kind = e
            .kind
            .map(|k| format!(" · {KIND}{k}{KIND:#}"))
            .unwrap_or_default();
        println!(
            "{PORT}:{}{PORT:#}  {BOLD}{}{BOLD:#}{kind}",
            e.port, e.process
        );
        let row = |label: &str, value: String| println!("  {DIM}{label:<9}{DIM:#} {value}");
        row("pid", e.pid.to_string());
        if let Some(pp) = e.parent_pid {
            let cmd = e
                .parent_command
                .as_deref()
                .map(|c| format!(" {DIM}{}{DIM:#}", truncate_right(c, 100)))
                .unwrap_or_default();
            row("parent", format!("{pp}{cmd}"));
        }
        row("command", e.command_line());
        if let Some(c) = &e.container {
            row(
                "container",
                format!(
                    "{PROJECT}{c}{PROJECT:#} {DIM}({} stop {c}){DIM:#}",
                    e.container_runtime().unwrap_or("docker")
                ),
            );
        }
        if let Some(p) = &e.project {
            row(
                "project",
                format!(
                    "{PROJECT}{}{PROJECT:#} {DIM}{} ({}){DIM:#}",
                    p.name,
                    tilde(&p.root),
                    p.marker
                ),
            );
        }
        if let Some(cwd) = &e.cwd {
            row("cwd", tilde(cwd));
        }
        let addrs: Vec<String> = e.addresses.iter().map(|a| a.to_string()).collect();
        let exposed = if e.addresses.iter().any(|a| !a.is_loopback()) {
            format!("  {WARN}reachable from other machines{WARN:#}")
        } else {
            String::new()
        };
        row("listen", format!("{}{exposed}", addrs.join(", ")));
        row("url", e.url());
        if let Some(up) = e.uptime_secs {
            row("uptime", human_duration(up));
        }
        if let Some(m) = e.memory_bytes {
            row("memory", human_bytes(m));
        }
        if let Some(u) = &e.user {
            row("user", u.clone());
        }
    }
}

pub fn print_json<T: serde::Serialize + ?Sized>(value: &T) -> anyhow::Result<()> {
    // Serialize fully before writing so a failure never leaves half a document.
    let json = serde_json::to_string_pretty(value)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{json}")?;
    Ok(())
}

pub fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub fn error(msg: impl std::fmt::Display) {
    eprintln!("{ERR}error:{ERR:#} {msg}");
}

pub fn warn(msg: impl std::fmt::Display) {
    eprintln!("{WARN}warning:{WARN:#} {msg}");
}

/// Ask a yes/no question on stderr. Defaults to "no".
pub fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// True when we can ask the user questions.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(titles: &[&'static str]) -> Vec<Column> {
        titles
            .iter()
            .map(|t| Column {
                title: t,
                style: Style::new(),
                cells: vec![],
                shrink: Shrink::Right,
            })
            .collect()
    }

    #[test]
    fn fit_shrinks_path_first() {
        let columns = cols(&["PORT", "PROJECT", "PATH"]);
        let mut widths = vec![4, 20, 40];
        fit_widths(&columns, &mut widths, 50, 2);
        assert_eq!(widths, vec![4, 20, 22]);
        let mut widths = vec![4, 20, 40];
        fit_widths(&columns, &mut widths, 30, 2);
        assert_eq!(widths, vec![4, 14, 8]);
    }
}
