# unporter

[![CI](https://github.com/snari2/unporter/actions/workflows/ci.yml/badge.svg)](https://github.com/snari2/unporter/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Find and kill whatever is hogging your dev ports.**

`Error: listen EADDRINUSE: address already in use :::3000`. Which terminal tab
was that again? `unporter` tells you what's on every port: the process, the
framework, **the project it was started from**, how long it's been running.
Then it stops it cleanly.

```console
$ unporter
PORT  PID    PROCESS             KIND     PROJECT        PATH                     BIND       UPTIME  MEM
3000  48211  node                Next.js  acme-web       ~/code/acme-web          *          2h 13m  182.4 MB
5001  48502  Python              Flask    inventory-api  ~/code/inventory-api     localhost  12m     58.3 MB
5173  48390  node                Vite     client         ~/code/dashboard/client  localhost  41m     96.0 MB
5432  4371   com.docker.backend  Docker   pg (docker)    -                        *          3d 2h   76.9 MB
9 other listeners hidden (system services and apps); use --all to show

$ unporter kill 3000
→ node (pid 48211) on :3000 · Next.js · acme-web
✔ stopped node (pid 48211) — freed :3000
```

## Features

- **Project-aware.** Walks up from each process's working directory to the
  nearest `package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`, `Gemfile`,
  `.git`, ... and shows the project's real name.
- **Knows your stack.** Recognises Next.js, Vite, Create React App, Nuxt, Astro,
  Angular, webpack, nodemon, Flask, Django, FastAPI/Uvicorn, Rails, Laravel,
  Postgres, Redis, MongoDB, Docker and more, falling back to the runtime
  (Node.js, Python, Bun, Deno, Rust...).
- **Hides the noise.** AirPlay, Spotify, Tailscale and friends are hidden
  unless you pass `--all`.
- **Kills politely.** SIGTERM first, SIGKILL only if the process is still
  there after 3 seconds. It checks the pid wasn't reused before signalling.
- **Docker-aware.** Ports published by containers are stopped with
  `docker stop <container>` rather than by killing Docker itself.
- **Safe by default.** Dev servers you own are stopped straight away. Anything
  else (system services, other users' processes) needs confirmation or `--yes`.
- **Live TUI.** `unporter tui` gives a refreshing table with filter, kill,
  force kill and open-in-browser.
- **Scriptable.** `--json` output, meaningful exit codes, `wait` and `free`
  subcommands.
- One small binary with no runtime dependencies. macOS and Linux, plus
  best-effort Windows support.

## Install

From source (needs Rust 1.88+):

```bash
cargo install --git https://github.com/snari2/unporter
```

Or clone and build:

```bash
git clone https://github.com/snari2/unporter && cd unporter && cargo install --path .
```

## Usage

```text
unporter                    List dev servers (add --all for everything, --json for JSON)
unporter 3000               Everything about whatever is on :3000
unporter kill 3000 5173     Stop them (SIGTERM, then SIGKILL after 3s)
unporter kill 3000-3010 -f  Force kill a whole range straight away
unporter kill 8080 -n       Dry run: show what would be stopped
unporter tui                Live, interactive view
unporter free               Print the first free port from 3000 up
unporter wait 5173          Block until something listens on :5173
unporter completions zsh    Shell completions (bash, zsh, fish, powershell, elvish)
```

Ports can be written as `3000`, `:3000`, `localhost:3000` or
`http://localhost:3000/`, so you can paste straight from an error message.

### Details for one port

```console
$ unporter 3000
:3000  node · Next.js
  pid       48211
  parent    48190 npm run dev
  command   node /Users/me/code/acme-web/node_modules/.bin/next dev
  project   acme-web ~/code/acme-web (package.json)
  cwd       ~/code/acme-web
  listen    ::  reachable from other machines
  url       http://localhost:3000
  uptime    2h 13m
  memory    182.4 MB
  user      me
```

### Interactive view

```bash
unporter tui
```

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move (`g` / `G` for first / last) |
| `x` `d` `Del` | Stop the selected process (with confirmation) |
| `X` `D` | Force kill (SIGKILL) |
| `o` `Enter` | Open `http://localhost:<port>` in your browser |
| `/` | Filter by port, process, framework or project |
| `a` | Toggle system services and apps |
| `r` | Refresh now (it also refreshes every 2s) |
| `?` | Help |
| `q` `Esc` | Quit |

### Scripting

```bash
# Free the port, then start the dev server
unporter kill 3000 && npm run dev

# Start a server in the background and open it once it's up
npm run dev & unporter wait 3000 -t 60 && open http://localhost:3000

# Pick a free port
python3 -m http.server "$(unporter free --from 8000)"

# Everything as JSON
unporter --all --json | jq '.[] | {port, kind, project: .project.name}'
```

Exit codes:

| Command | 0 | 1 |
| --- | --- | --- |
| `unporter <port>` / `info` | something is listening (even if its owner is hidden from you) | nothing is listening |
| `kill` | every port is free afterwards (including ports that already were) | something couldn't be stopped, or was restarted |
| `wait` | the condition was met | timed out |
| `free` | found enough free ports | ran out of ports |

Invalid arguments exit with 2. Colour follows the terminal and respects
[`NO_COLOR`](https://no-color.org) and `CLICOLOR_FORCE`.

## How it decides what's a "dev server"

A listener is shown by default when any of these hold:

1. its working directory is inside a project (a directory with one of the
   marker files above, but never your home directory itself or `~/Library`),
   or
2. its command line matches a known framework, database or runtime, or
3. it is a Docker-published port.

Everything else (desktop apps, OS services) only shows with `--all`.

## Permissions

Without root, the OS only shows you sockets that belong to your own user. If a
port is busy but `unporter` can't see the owner, it says so and suggests
`sudo`. Stopping another user's process needs `sudo unporter kill <port>`.

## Platform support

| | macOS | Linux | Windows |
| --- | --- | --- | --- |
| List ports, process info | ✅ | ✅ | ✅ |
| Project detection (cwd) | ✅ | ✅ | best effort |
| Graceful stop (SIGTERM → SIGKILL) | ✅ | ✅ | terminates immediately |
| TUI | ✅ | ✅ | ✅ |

## Contributing

Issues and pull requests are welcome. To hack on it:

```bash
cargo test                                   # unit + end-to-end tests
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo run -- --all                           # try it
```

The code is small and split by concern: `scan.rs` (sockets to processes),
`project.rs` (project detection), `kind.rs` (framework detection), `kill.rs`,
`cli.rs`, `output.rs` and `tui.rs`. Adding a framework is usually a one-line
change to the tables at the top of `kind.rs`, plus a test.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
