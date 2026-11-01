//! Guess what kind of server a process is (Next.js, Vite, Flask, Postgres...)
//! from its name and command line.

use std::collections::HashSet;

/// Tool-specific rules, checked before generic runtimes. Each rule matches if
/// every token in `all` appears in the command line.
const RULES: &[(&[&str], &str)] = &[
    (&["next-server"], "Next.js"),
    (&["next"], "Next.js"),
    (&["vite"], "Vite"),
    (&["react-scripts"], "Create React App"),
    (&["webpack-dev-server"], "webpack"),
    (&["webpack", "serve"], "webpack"),
    (&["nuxt"], "Nuxt"),
    (&["nuxi"], "Nuxt"),
    (&["astro"], "Astro"),
    (&["remix"], "Remix"),
    (&["svelte-kit"], "SvelteKit"),
    (&["ng", "serve"], "Angular"),
    (&["gatsby"], "Gatsby"),
    (&["storybook"], "Storybook"),
    (&["expo"], "Expo"),
    (&["parcel"], "Parcel"),
    (&["nest", "start"], "NestJS"),
    (&["nodemon"], "nodemon"),
    (&["json-server"], "json-server"),
    (&["http-server"], "http-server"),
    (&["live-server"], "live-server"),
    (&["flask"], "Flask"),
    (&["uvicorn"], "Uvicorn"),
    (&["gunicorn"], "Gunicorn"),
    (&["hypercorn"], "Hypercorn"),
    (&["fastapi"], "FastAPI"),
    (&["manage.py", "runserver"], "Django"),
    (&["streamlit"], "Streamlit"),
    (&["jupyter-lab"], "Jupyter"),
    (&["jupyter-notebook"], "Jupyter"),
    (&["jupyter"], "Jupyter"),
    (&["http.server"], "http.server"),
    (&["rails"], "Rails"),
    (&["puma"], "Puma"),
    (&["artisan", "serve"], "Laravel"),
    (&["hugo"], "Hugo"),
    (&["jekyll"], "Jekyll"),
    (&["serve"], "serve"),
];

/// Daemons matched on the process name or argv[0] only, so `--db postgres`
/// in some other program's arguments doesn't count.
const DAEMONS: &[(&str, &str)] = &[
    ("postgres", "PostgreSQL"),
    ("mysqld", "MySQL"),
    ("mariadbd", "MariaDB"),
    ("mongod", "MongoDB"),
    ("redis-server", "Redis"),
    ("valkey-server", "Valkey"),
    ("memcached", "Memcached"),
    ("ollama", "Ollama"),
    ("com.docker.backend", "Docker"),
    ("com.docker.vpnkit", "Docker"),
    ("vpnkit", "Docker"),
    ("docker-proxy", "Docker"),
    ("orbstack helper", "Docker"),
    ("podman", "Podman"),
    ("gvproxy", "Podman"),
    ("caddy", "Caddy"),
    ("nginx", "nginx"),
    ("httpd", "Apache"),
];

/// Bare runtimes, used when nothing more specific matched. Only the process
/// name or argv[0] is checked so `node` in a path elsewhere doesn't count.
const RUNTIMES: &[(&str, &str)] = &[
    ("node", "Node.js"),
    ("bun", "Bun"),
    ("deno", "Deno"),
    ("python", "Python"),
    ("python3", "Python"),
    ("ruby", "Ruby"),
    ("php", "PHP"),
    ("java", "Java"),
    ("dotnet", ".NET"),
    ("beam.smp", "Erlang/Elixir"),
];

pub fn detect(process_name: &str, cmd: &[String]) -> Option<&'static str> {
    let head = [
        normalize(process_name),
        cmd.first()
            .map(|a| normalize(basename(a)))
            .unwrap_or_default(),
    ];
    for (bin, kind) in DAEMONS {
        if head.iter().any(|h| h == bin || is_versioned(h, bin)) {
            return Some(kind);
        }
    }
    let tokens = tokenize(process_name, cmd);
    for (all, kind) in RULES {
        if all.iter().all(|t| tokens.contains(*t)) {
            return Some(kind);
        }
    }
    for (bin, kind) in RUNTIMES {
        if head.iter().any(|h| h == bin || is_versioned(h, bin)) {
            return Some(kind);
        }
    }
    if cmd.first().is_some_and(|a| is_cargo_target(a)) {
        return Some("Rust");
    }
    None
}

/// `python3.12` or `node22` count as `python` / `node`.
fn is_versioned(head: &str, bin: &str) -> bool {
    head.strip_prefix(bin).is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

fn is_cargo_target(path: &str) -> bool {
    path.contains("/target/debug/") || path.contains("/target/release/")
}

fn basename(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}

/// Lowercase and strip script/binary extensions: `vite.js` -> `vite`.
fn normalize(s: &str) -> String {
    let s = s.to_ascii_lowercase();
    for ext in [".js", ".mjs", ".cjs", ".ts", ".exe", ".cmd"] {
        if let Some(stripped) = s.strip_suffix(ext) {
            return stripped.to_owned();
        }
    }
    s
}

/// Split the process name and every argument into path components and words.
/// `node_modules/.bin/vite --port 5173` yields `vite`, `--port`, `5173`, ...
fn tokenize(process_name: &str, cmd: &[String]) -> HashSet<String> {
    let mut tokens = HashSet::new();
    for arg in std::iter::once(process_name).chain(cmd.iter().map(String::as_str)) {
        for piece in arg.split(['/', '\\', ' ', '=', ':']) {
            if piece.is_empty() {
                continue;
            }
            tokens.insert(piece.to_ascii_lowercase());
            tokens.insert(normalize(piece));
            // "next-server (v15.1.0)" style process titles
            if let Some((head, _)) = piece.split_once('(') {
                tokens.insert(normalize(head.trim()));
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(name: &str, cmd: &str) -> Option<&'static str> {
        let cmd: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
        detect(name, &cmd)
    }

    #[test]
    fn js_frameworks() {
        assert_eq!(
            d("node", "node /app/node_modules/.bin/next dev"),
            Some("Next.js")
        );
        assert_eq!(
            d("next-server (v15.1.0)", "next-server (v15.1.0)"),
            Some("Next.js")
        );
        assert_eq!(
            d(
                "node",
                "node /app/node_modules/vite/bin/vite.js --port 5173"
            ),
            Some("Vite")
        );
        assert_eq!(
            d(
                "node",
                "node /app/node_modules/react-scripts/scripts/start.js"
            ),
            Some("Create React App")
        );
        assert_eq!(d("node", "node /x/.bin/nodemon index.js"), Some("nodemon"));
        assert_eq!(
            d("node", "node /x/@angular/cli/bin/ng serve"),
            Some("Angular")
        );
    }

    #[test]
    fn python_and_others() {
        assert_eq!(d("Python", "/usr/bin/python3 -m flask run"), Some("Flask"));
        assert_eq!(
            d("python3.12", "python3.12 manage.py runserver"),
            Some("Django")
        );
        assert_eq!(
            d("Python", "python3 -m http.server 8080"),
            Some("http.server")
        );
        assert_eq!(
            d("uvicorn", "/venv/bin/uvicorn main:app --reload"),
            Some("Uvicorn")
        );
        assert_eq!(
            d(
                "postgres",
                "/opt/homebrew/opt/postgresql@16/bin/postgres -D /x"
            ),
            Some("PostgreSQL")
        );
        assert_eq!(
            d(
                "com.docker.backend",
                "/Applications/Docker.app/Contents/MacOS/com.docker.backend"
            ),
            Some("Docker")
        );
    }

    #[test]
    fn runtime_fallbacks() {
        assert_eq!(d("node", "node server.js"), Some("Node.js"));
        assert_eq!(d("node22", "node22 index.js"), Some("Node.js"));
        assert_eq!(d("python3", "python3 app.py"), Some("Python"));
        assert_eq!(d("myapi", "/home/me/api/target/debug/myapi"), Some("Rust"));
    }

    #[test]
    fn no_false_positives_from_substrings() {
        assert_eq!(d("invite-service", "/srv/invite-service --port 9000"), None);
        assert_eq!(
            d(
                "Spotify",
                "/Applications/Spotify.app/Contents/MacOS/Spotify"
            ),
            None
        );
        assert_eq!(d("nodeweaver", "nodeweaver"), None);
        assert_eq!(d("node", "node app.js --db postgres"), Some("Node.js"));
        assert_eq!(d("hugo", "hugo serve"), Some("Hugo"));
    }
}
