//! Command-line interface: argument parsing and the subcommands.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::ops::RangeInclusive;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anstream::{eprintln, println};
use anyhow::{Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::kill::{self, Action, KillOptions, Outcome};
use crate::output::{self, BOLD, DIM, ERR, KIND, OK, PORT, PROJECT, plural};
use crate::scan::{self, PortEntry};

const EXAMPLES: &str = "\
Examples:
  unporter                 List dev servers and the projects they belong to
  unporter 3000            Show everything about whatever is on :3000
  unporter kill 3000       Stop it (SIGTERM, then SIGKILL after 3s)
  unporter kill 3000-3005 8080 -f
  unporter tui             Live, interactive view
  unporter free            Print the first free port from 3000 up
  unporter wait 5173 && open http://localhost:5173";

#[derive(Debug, Parser)]
#[command(
    name = "unporter",
    version,
    about = "Find and kill whatever is hogging your dev ports",
    long_about = "Find and kill whatever is hogging your dev ports.\n\n\
        unporter lists every listening TCP port with the process, framework, project \
        and uptime behind it, and stops it cleanly when you ask.",
    after_help = EXAMPLES,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Show details for one port (same as `unporter info <PORT>`)
    #[arg(value_parser = parse_port)]
    pub port: Option<u16>,

    #[command(flatten)]
    pub list: ListArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List listening ports (the default command)
    #[command(visible_alias = "ls")]
    List(ListArgs),

    /// Show details for whatever listens on a port
    Info {
        #[arg(value_parser = parse_port)]
        port: u16,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },

    /// Stop whatever listens on one or more ports
    #[command(visible_alias = "k")]
    Kill(KillArgs),

    /// Print the first free port(s), starting from --from
    Free {
        /// Port to start searching from
        #[arg(long, default_value_t = 3000, value_parser = parse_port)]
        from: u16,
        /// How many free ports to print
        #[arg(short = 'n', long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
        count: u16,
    },

    /// Wait until something is listening on a port (or until it's free)
    Wait {
        #[arg(value_parser = parse_port)]
        port: u16,
        /// Wait for the port to become free instead
        #[arg(long)]
        free: bool,
        /// Give up after this many seconds (0 waits forever)
        #[arg(short, long, default_value_t = 30)]
        timeout: u64,
    },

    /// Open the interactive, auto-refreshing view
    #[command(visible_aliases = ["ui", "i"])]
    Tui {
        /// Include system services and desktop apps
        #[arg(short, long)]
        all: bool,
    },

    /// Print a shell completion script
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Args, Default)]
pub struct ListArgs {
    /// Include system services and desktop apps, not just dev servers
    #[arg(short, long)]
    pub all: bool,
    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct KillArgs {
    /// Ports to free: `3000`, `:3000`, `localhost:3000` or a range like `3000-3005`
    #[arg(required = true, num_args = 1.., value_parser = parse_port_spec)]
    pub ports: Vec<RangeInclusive<u16>>,
    /// Send SIGKILL immediately instead of SIGTERM first
    #[arg(short, long)]
    pub force: bool,
    /// Don't ask before stopping processes that aren't dev servers
    #[arg(short, long)]
    pub yes: bool,
    /// Show what would be stopped without doing it
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Seconds to wait after SIGTERM before sending SIGKILL
    #[arg(short, long, default_value_t = 3)]
    pub timeout: u64,
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    match (cli.command, cli.port) {
        (Some(Command::List(args)), _) => list(&args),
        (Some(Command::Info { port, json }), _) => info(port, json),
        (None, Some(port)) => info(port, cli.list.json),
        (None, None) => list(&cli.list),
        (Some(Command::Kill(args)), _) => kill_ports(&args),
        (Some(Command::Free { from, count }), _) => free(from, count),
        (
            Some(Command::Wait {
                port,
                free,
                timeout,
            }),
            _,
        ) => wait(port, free, timeout),
        (Some(Command::Tui { all }), _) => {
            if !std::io::stdout().is_terminal() {
                bail!("the interactive view needs a terminal; use `unporter list` in scripts");
            }
            crate::tui::run(all)?;
            Ok(ExitCode::SUCCESS)
        }
        (Some(Command::Completions { shell }), _) => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "unporter",
                &mut std::io::stdout(),
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn list(args: &ListArgs) -> Result<ExitCode> {
    let entries = scan::scan()?;
    let total = entries.len();
    let shown: Vec<PortEntry> = entries.into_iter().filter(|e| args.all || e.dev).collect();
    if args.json {
        output::print_json(&shown)?;
    } else {
        output::print_table(&shown, total - shown.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn info(port: u16, json: bool) -> Result<ExitCode> {
    let entries = scan::scan_port(port)?;
    // Busy even when the owner is invisible to us (another user's process).
    let busy = !entries.is_empty() || scan::is_listening(port);
    let owner_hidden = entries.is_empty() && busy;
    if json {
        output::print_json(&entries)?;
        if owner_hidden {
            output::warn(format!(
                ":{port} is in use, but unporter can't see which process owns it (try sudo)"
            ));
        }
    } else if owner_hidden {
        println!(
            "{PORT}:{port}{PORT:#} is in use, but unporter can't see which process owns it \
             {DIM}(it probably belongs to another user; try sudo){DIM:#}"
        );
    } else if entries.is_empty() {
        println!("Nothing is listening on {PORT}:{port}{PORT:#}.");
    } else {
        output::print_details(&entries);
    }
    Ok(exit(!busy))
}

fn kill_ports(args: &KillArgs) -> Result<ExitCode> {
    let ports = expand_ports(&args.ports);
    let entries = scan::scan()?;
    let mut failed = false;
    let mut targets: Vec<&PortEntry> = Vec::new();
    for &port in &ports {
        let on_port: Vec<&PortEntry> = entries.iter().filter(|e| e.port == port).collect();
        if !on_port.is_empty() {
            targets.extend(on_port);
        } else if scan::is_listening(port) {
            output::error(format!(
                ":{port} is in use, but unporter can't see which process owns it \
                 (it probably belongs to another user; try sudo)"
            ));
            failed = true;
        } else if ports.len() <= 20 {
            println!("{DIM}·{DIM:#} {PORT}:{port}{PORT:#} is already free");
        }
    }
    if targets.is_empty() {
        return Ok(exit(failed));
    }

    let actions = kill::plan(targets.iter().copied());
    let runnable: Vec<&Action> = actions
        .iter()
        .filter(|a| !matches!(a, Action::Skip { .. }))
        .collect();
    for action in &actions {
        match action {
            Action::Skip { reason, .. } => {
                output::warn(reason);
                failed = true;
            }
            _ => println!(
                "{} {}",
                if args.dry_run { "would stop" } else { "→" },
                describe(action, &targets)
            ),
        }
    }
    if args.dry_run || runnable.is_empty() {
        return Ok(exit(failed));
    }

    // Dev servers you own are stopped without ceremony; anything else
    // (AirPlay on :5000, a database run by another user, ...) needs a yes.
    let risky = runnable.iter().any(|a| {
        targets
            .iter()
            .filter(|e| a.ports().contains(&e.port))
            .any(|e| !e.dev || !e.mine)
    });
    if risky && !args.yes {
        if !output::interactive() {
            output::error(
                "some of these don't look like dev servers; pass --yes to stop them anyway",
            );
            return Ok(ExitCode::FAILURE);
        }
        let n = runnable.len();
        if !output::confirm(&format!(
            "Some of these aren't dev servers. Stop {n} process{}?",
            if n == 1 { "" } else { "es" }
        )) {
            eprintln!("Aborted.");
            return Ok(ExitCode::FAILURE);
        }
    }

    let opts = KillOptions {
        force: args.force,
        timeout: Duration::from_secs(args.timeout),
    };
    let mut freed: Vec<u16> = Vec::new();
    for action in runnable {
        let outcome = kill::execute(action, opts);
        let ports = ports_label(action.ports());
        match &outcome {
            Outcome::Exited | Outcome::Stopped => {
                println!(
                    "{OK}✔{OK:#} stopped {} {DIM}— freed {ports}{DIM:#}",
                    action.describe()
                );
            }
            Outcome::Killed => println!(
                "{OK}✔{OK:#} {} ignored SIGTERM, sent SIGKILL {DIM}— freed {ports}{DIM:#}",
                action.describe()
            ),
            Outcome::Gone => println!("{OK}✔{OK:#} {} had already exited", action.describe()),
            Outcome::Skipped(msg) => output::warn(msg),
            Outcome::Failed(msg) => {
                println!("{ERR}✖{ERR:#} {msg}");
                failed = true;
            }
        }
        if outcome.is_success() {
            freed.extend(action.ports());
        }
    }

    // Supervisors (pm2, nodemon, docker restart policies) may bring it back.
    if !freed.is_empty() {
        thread::sleep(Duration::from_millis(400));
        for port in freed {
            if scan::is_bound(port) {
                output::warn(format!(
                    ":{port} is in use again; something restarted it (check pm2, nodemon, launchd or a docker restart policy)"
                ));
                failed = true;
            }
        }
    }
    Ok(exit(failed))
}

fn describe(action: &Action, targets: &[&PortEntry]) -> String {
    let entry = targets.iter().find(|e| action.ports().contains(&e.port));
    let mut s = format!(
        "{BOLD}{}{BOLD:#} on {}",
        action.describe(),
        ports_label(action.ports())
    );
    if let Some(e) = entry {
        if let Some(k) = e.kind {
            s.push_str(&format!(" {DIM}·{DIM:#} {KIND}{k}{KIND:#}"));
        }
        if e.container.is_none()
            && let Some(p) = &e.project
        {
            s.push_str(&format!(" {DIM}·{DIM:#} {PROJECT}{}{PROJECT:#}", p.name));
        }
    }
    s
}

fn ports_label(ports: &[u16]) -> String {
    let parts: Vec<String> = ports
        .iter()
        .map(|p| format!("{PORT}:{p}{PORT:#}"))
        .collect();
    parts.join(" ")
}

fn free(from: u16, count: u16) -> Result<ExitCode> {
    let known: HashSet<u16> = scan::raw_listeners()?.into_iter().map(|l| l.port).collect();
    let mut found = 0;
    for port in from..=u16::MAX {
        if scan::is_free(port, &known) {
            println!("{port}");
            found += 1;
            if found == count {
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
    output::error(format!(
        "only found {found} free port{} at or above {from}",
        plural(found as usize)
    ));
    Ok(ExitCode::FAILURE)
}

fn wait(port: u16, until_free: bool, timeout: u64) -> Result<ExitCode> {
    let start = Instant::now();
    // `checked_add` so an absurd timeout means "forever" rather than a panic.
    let deadline = (timeout > 0)
        .then(|| start.checked_add(Duration::from_secs(timeout)))
        .flatten();
    loop {
        if scan::is_bound(port) != until_free {
            let what = if until_free { "is free" } else { "is up" };
            eprintln!(
                "{OK}✔{OK:#} {PORT}:{port}{PORT:#} {what} {DIM}({:.1}s){DIM:#}",
                start.elapsed().as_secs_f64()
            );
            return Ok(ExitCode::SUCCESS);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            let what = if until_free {
                "to be freed"
            } else {
                "for a listener"
            };
            output::error(format!(
                "timed out after {timeout}s waiting {what} on :{port}"
            ));
            return Ok(ExitCode::FAILURE);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn exit(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Accept `3000`, `:3000`, `localhost:3000`, `http://localhost:3000/`.
pub fn parse_port(s: &str) -> Result<u16, String> {
    let s = s.trim().trim_end_matches('/');
    let digits = s.rsplit(':').next().unwrap_or(s);
    match digits.parse::<u16>() {
        Ok(0) => Err("port 0 isn't a real port".to_owned()),
        Ok(p) => Ok(p),
        Err(_) => Err(format!("`{s}` isn't a port number (1-65535)")),
    }
}

/// One `kill` argument: a port in any form [`parse_port`] accepts, or a
/// range like `3000-3005` (also `localhost:3000-3005`).
pub fn parse_port_spec(s: &str) -> Result<RangeInclusive<u16>, String> {
    let tail = s
        .trim()
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap_or("");
    let is_digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
    if let Some((a, b)) = tail.split_once('-')
        && is_digits(a)
        && is_digits(b)
    {
        let (a, b) = (parse_port(a)?, parse_port(b)?);
        if a > b {
            return Err(format!("`{s}`: range start is after its end"));
        }
        if b - a >= 1000 {
            return Err(format!("`{s}`: ranges are limited to 1000 ports"));
        }
        return Ok(a..=b);
    }
    parse_port(s).map(|p| p..=p)
}

/// Flatten port ranges in order, without duplicates.
pub fn expand_ports(specs: &[RangeInclusive<u16>]) -> Vec<u16> {
    let mut seen = HashSet::new();
    specs
        .iter()
        .flat_map(|r| r.clone())
        .filter(|p| seen.insert(*p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_parsing() {
        assert_eq!(parse_port("3000"), Ok(3000));
        assert_eq!(parse_port(":3000"), Ok(3000));
        assert_eq!(parse_port("localhost:5173"), Ok(5173));
        assert_eq!(parse_port("http://localhost:8080/"), Ok(8080));
        assert!(parse_port("0").is_err());
        assert!(parse_port("70000").is_err());
        assert!(parse_port("abc").is_err());
    }

    #[test]
    fn port_ranges() {
        let specs = |v: &[&str]| -> Result<Vec<_>, String> {
            v.iter().map(|s| parse_port_spec(s)).collect()
        };
        assert_eq!(
            expand_ports(&specs(&["3000-3002", "3001", "8080"]).unwrap()),
            vec![3000, 3001, 3002, 8080]
        );
        assert_eq!(expand_ports(&specs(&[":5173"]).unwrap()), vec![5173]);
        assert_eq!(parse_port_spec("localhost:3000-3002"), Ok(3000..=3002));
        assert_eq!(parse_port_spec("my-host:3000"), Ok(3000..=3000));
        assert_eq!(
            parse_port_spec("http://dev-box.local:8080/"),
            Ok(8080..=8080)
        );
        assert!(parse_port_spec("3005-3000").is_err());
        assert!(parse_port_spec("1-5000").is_err());
        assert!(parse_port_spec("nope").is_err());
        assert!(parse_port_spec("0").is_err());
    }

    #[test]
    fn cli_shapes() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["unporter", "3000"]).unwrap();
        assert_eq!(cli.port, Some(3000));
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from(["unporter", "--all", "--json"]).unwrap();
        assert!(cli.list.all && cli.list.json);
        let cli = Cli::try_parse_from(["unporter", "kill", "3000", ":5173", "-fy"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Kill(KillArgs {
                force: true,
                yes: true,
                ..
            }))
        ));
        assert!(Cli::try_parse_from(["unporter", "kill"]).is_err());
    }
}
