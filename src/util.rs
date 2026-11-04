use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// `3d 4h`, `2h 13m`, `12m`, `45s`.
pub fn human_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// `512 B`, `12.3 KB`, `182.4 MB`, `1.2 GB` (powers of 1024).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Serialize a path as a string even when it isn't valid UTF-8 (serde's
/// default errors out mid-document instead).
pub fn serialize_path<S: serde::Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.to_string_lossy())
}

pub fn serialize_opt_path<S: serde::Serializer>(
    path: &Option<std::path::PathBuf>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match path {
        Some(p) => s.serialize_some(&p.to_string_lossy()),
        None => s.serialize_none(),
    }
}

/// Replace the home directory prefix with `~`.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Shorten `s` to at most `max` display columns, keeping the end and
/// prefixing `…` (paths are most informative at their tail).
pub fn truncate_left(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: Vec<char> = Vec::new();
    let mut used = 1; // the ellipsis
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out.into_iter().rev().collect()
}

/// Shorten `s` to at most `max` display columns, keeping the start.
pub fn truncate_right(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

pub fn width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Run a command and return its stdout, or `None` if it fails, isn't
/// installed, or takes longer than `timeout`.
pub fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on a separate thread so a chatty child can't fill the pipe and
    // block while we wait for it to exit.
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).ok().map(|_| buf)
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok().flatten()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Open a URL in the default browser without blocking.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        Command::new("xdg-open")
    };
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_serialize() {
        use std::os::unix::ffi::OsStrExt;
        let p = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9"));
        #[derive(serde::Serialize)]
        struct W {
            #[serde(serialize_with = "serialize_opt_path")]
            p: Option<std::path::PathBuf>,
        }
        let json = serde_json::to_string(&W { p: Some(p) }).unwrap();
        assert_eq!(json, "{\"p\":\"/tmp/caf\u{fffd}\"}");
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(59), "59s");
        assert_eq!(human_duration(60), "1m");
        assert_eq!(human_duration(3600), "1h");
        assert_eq!(human_duration(3600 + 13 * 60 + 5), "1h 13m");
        assert_eq!(human_duration(86_400), "1d");
        assert_eq!(human_duration(2 * 86_400 + 5 * 3600 + 7), "2d 5h");
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(191_260_000), "182.4 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_left("~/code/shop/client", 10), "…op/client");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate_left("abc", 4), "abc");
        assert_eq!(truncate_right("abcdef", 4), "abc…");
        assert_eq!(truncate_right("abcdef", 0), "");
        assert_eq!(width(&truncate_left("日本語のパス", 5)), 5);
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout() {
        assert_eq!(
            output_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(5)).as_deref(),
            Some("hi\n")
        );
        assert_eq!(
            output_with_timeout(
                &mut Command::new("definitely-not-a-real-binary"),
                Duration::from_secs(1)
            ),
            None
        );
        let start = Instant::now();
        assert_eq!(
            output_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(200)),
            None
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
