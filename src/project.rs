//! Figure out which project a process belongs to by walking up from its
//! working directory until a project marker (package.json, Cargo.toml, ...)
//! is found.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    /// Name from the manifest, or the directory name as a fallback.
    pub name: String,
    /// Directory containing the marker file.
    pub root: PathBuf,
    /// The marker that identified the project, e.g. `package.json`.
    pub marker: &'static str,
}

/// Marker files in priority order. Manifests come before generic markers so
/// that a directory with both `package.json` and `.git` reports the npm name.
const MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "deno.json",
    "deno.jsonc",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "manage.py",
    "requirements.txt",
    ".git",
];

/// Detect the project for a process working directory, never treating the
/// user's home directory (or anything above it) as a project.
pub fn detect(cwd: &Path) -> Option<Project> {
    detect_with_home(cwd, dirs::home_dir().as_deref())
}

pub fn detect_with_home(cwd: &Path, home: Option<&Path>) -> Option<Project> {
    if is_ignored(cwd, home) {
        return None;
    }
    for dir in cwd.ancestors() {
        if dir.parent().is_none() || Some(dir) == home {
            break;
        }
        for &marker in MARKERS {
            let path = dir.join(marker);
            if path.exists() {
                let name = manifest_name(marker, &path).unwrap_or_else(|| dir_name(dir));
                return Some(Project {
                    name,
                    root: dir.to_path_buf(),
                    marker,
                });
            }
        }
    }
    None
}

/// True for system and app-data directories (`/usr`, `~/Library`, ...) that
/// are never user projects.
pub fn is_system_path(path: &Path) -> bool {
    path.parent().is_none() || is_ignored(path, dirs::home_dir().as_deref())
}

/// Directories where processes commonly run that are never user projects.
fn is_ignored(cwd: &Path, home: Option<&Path>) -> bool {
    const SYSTEM_PREFIXES: &[&str] = &[
        "/System",
        "/Library",
        "/Applications",
        "/usr",
        "/bin",
        "/sbin",
        "/proc",
        // Homebrew's prefix is itself a git checkout, so `brew services`
        // running in /opt/homebrew/var would otherwise look like a project.
        "/opt/homebrew",
        "/home/linuxbrew/.linuxbrew",
    ];
    if SYSTEM_PREFIXES.iter().any(|p| cwd.starts_with(p)) {
        return true;
    }
    if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX")
        && !prefix.is_empty()
        && cwd.starts_with(prefix)
    {
        return true;
    }
    match home {
        // Everything outside home is fair game (e.g. /srv/app, /tmp/demo), but
        // app support dirs inside home are not projects.
        Some(home) => {
            cwd.starts_with(home.join("Library"))
                || cwd.starts_with(home.join(".cache"))
                || cwd.starts_with(home.join(".local/share"))
        }
        None => false,
    }
}

/// `module github.com/acme/api/v2 // comment` -> `api`.
fn go_module_name(go_mod: &str) -> Option<String> {
    let line = go_mod.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("module")?;
        rest.starts_with([' ', '\t']).then_some(rest)
    })?;
    let path = line.split("//").next()?.trim().trim_matches('"');
    let mut segments = path.rsplit('/');
    let last = segments.next()?;
    let is_major_version =
        last.len() > 1 && last.starts_with('v') && last[1..].bytes().all(|b| b.is_ascii_digit());
    match segments.next() {
        Some(parent) if is_major_version => Some(parent.to_owned()),
        _ => Some(last.to_owned()),
    }
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// Read the project name out of a manifest, if the format has one.
fn manifest_name(marker: &str, path: &Path) -> Option<String> {
    let read = || fs::read_to_string(path).ok();
    let name = match marker {
        "package.json" | "composer.json" | "deno.json" => {
            let json: serde_json::Value = serde_json::from_str(&read()?).ok()?;
            json.get("name")?.as_str()?.to_owned()
        }
        "Cargo.toml" => {
            let doc: toml::Table = read()?.parse().ok()?;
            doc.get("package")?.get("name")?.as_str()?.to_owned()
        }
        "pyproject.toml" => {
            let doc: toml::Table = read()?.parse().ok()?;
            doc.get("project")
                .and_then(|p| p.get("name"))
                .or_else(|| doc.get("tool")?.get("poetry")?.get("name"))?
                .as_str()?
                .to_owned()
        }
        "go.mod" => go_module_name(&read()?)?,
        _ => return None,
    };
    let name = name.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn package_json_name() {
        let tmp = setup();
        let root = tmp.path().join("web");
        fs::create_dir_all(root.join("src/pages")).unwrap();
        fs::write(root.join("package.json"), r#"{"name": "@acme/web"}"#).unwrap();
        let p = detect_with_home(&root.join("src/pages"), None).unwrap();
        assert_eq!(p.name, "@acme/web");
        assert_eq!(p.root, root);
        assert_eq!(p.marker, "package.json");
    }

    #[test]
    fn innermost_marker_wins() {
        let tmp = setup();
        let repo = tmp.path().join("shop");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("client")).unwrap();
        fs::write(repo.join("client/package.json"), r#"{"name":"client"}"#).unwrap();
        let p = detect_with_home(&repo.join("client"), None).unwrap();
        assert_eq!(p.name, "client");
        let p = detect_with_home(&repo, None).unwrap();
        assert_eq!(p.name, "shop");
        assert_eq!(p.marker, ".git");
    }

    #[test]
    fn cargo_pyproject_gomod_names() {
        let tmp = setup();
        let c = tmp.path().join("c");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("Cargo.toml"), "[package]\nname = \"api-server\"\n").unwrap();
        assert_eq!(detect_with_home(&c, None).unwrap().name, "api-server");

        let p = tmp.path().join("p");
        fs::create_dir_all(&p).unwrap();
        fs::write(
            p.join("pyproject.toml"),
            "[tool.poetry]\nname = \"flaskapp\"\n",
        )
        .unwrap();
        assert_eq!(detect_with_home(&p, None).unwrap().name, "flaskapp");

        let g = tmp.path().join("g");
        fs::create_dir_all(&g).unwrap();
        fs::write(
            g.join("go.mod"),
            "module github.com/acme/gosvc\n\ngo 1.22\n",
        )
        .unwrap();
        assert_eq!(detect_with_home(&g, None).unwrap().name, "gosvc");
    }

    #[test]
    fn falls_back_to_dir_name_for_workspaces_and_bad_json() {
        let tmp = setup();
        let w = tmp.path().join("monorepo");
        fs::create_dir_all(&w).unwrap();
        fs::write(w.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        assert_eq!(detect_with_home(&w, None).unwrap().name, "monorepo");

        let b = tmp.path().join("broken");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("package.json"), "{ not json").unwrap();
        assert_eq!(detect_with_home(&b, None).unwrap().name, "broken");
    }

    #[test]
    fn home_is_never_a_project() {
        let tmp = setup();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("Downloads")).unwrap();
        fs::write(home.join("package.json"), r#"{"name":"dotfiles"}"#).unwrap();
        assert_eq!(detect_with_home(&home, Some(&home)), None);
        assert_eq!(detect_with_home(&home.join("Downloads"), Some(&home)), None);
        fs::create_dir_all(home.join("Library/App")).unwrap();
        fs::write(home.join("Library/App/package.json"), "{}").unwrap();
        assert_eq!(
            detect_with_home(&home.join("Library/App"), Some(&home)),
            None
        );
    }

    #[test]
    fn go_module_names() {
        assert_eq!(
            go_module_name("module github.com/acme/api/v2\n").as_deref(),
            Some("api")
        );
        assert_eq!(
            go_module_name("module\tgithub.com/acme/svc // x\n").as_deref(),
            Some("svc")
        );
        assert_eq!(
            go_module_name("module \"example.com/v2\"\n").as_deref(),
            Some("example.com")
        );
        assert_eq!(go_module_name("module v2\n").as_deref(), Some("v2"));
        assert_eq!(go_module_name("modules foo\n"), None);
    }

    #[test]
    fn system_dirs_ignored() {
        assert_eq!(detect_with_home(Path::new("/"), None), None);
        assert_eq!(detect_with_home(Path::new("/usr/local/bin"), None), None);
        assert!(is_ignored(
            Path::new("/opt/homebrew/var/postgresql@16"),
            None
        ));
        assert!(is_ignored(Path::new("/opt/homebrew"), None));
        assert!(!is_ignored(Path::new("/opt/homebrew-tools/app"), None));
    }
}
