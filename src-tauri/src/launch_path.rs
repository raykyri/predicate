use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const HOME_FALLBACK_DIRS: &[&str] = &[
    ".local/bin",
    "bin",
    ".npm-global/bin",
    ".bun/bin",
    "Library/pnpm",
    ".cargo/bin",
    ".deno/bin",
    "go/bin",
];

const SYSTEM_FALLBACK_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// Resolves the login-shell PATH cache on a background thread. The probe runs
/// the user's shell as an interactive login shell (`-ilc`) and can take
/// hundreds of milliseconds to seconds under nvm/pyenv-style profiles; its
/// first caller used to be the first recovered pane's spawn, which sat on the
/// main thread inside the startup hook. Warming it as startup's first act
/// overlaps the probe with the rest of setup, and any spawn that arrives early
/// simply blocks on the same `OnceLock` init instead of starting a second probe.
pub(crate) fn warm_login_shell_path() {
    std::thread::spawn(|| {
        let _ = login_shell_path_dirs();
    });
}

/// The qmux CLI path advertised to child processes — `QMUX_CLI`, hook
/// commands, and shell wrapper functions all call back into qmux through this
/// binary. Centralized so exactly one place decides which CLI a launch target
/// gets: the local target reuses this process's executable (the app binary
/// doubles as the CLI), while a remote target must instead resolve a
/// standalone `qmux-cli` shipped to its host.
pub(crate) fn qmux_cli_path() -> Result<PathBuf, String> {
    env::current_exe().map_err(|err| format!("failed to resolve the qmux executable: {err}"))
}

pub(crate) fn resolve_binary(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH");
    let home = env::var_os("HOME").map(PathBuf::from);
    resolve_binary_from(
        binary,
        path.as_deref(),
        home.as_deref(),
        login_shell_path_dirs(),
    )
}

pub(crate) fn child_path() -> Option<String> {
    let path = env::var_os("PATH");
    let home = env::var_os("HOME").map(PathBuf::from);
    child_path_from(path.as_deref(), home.as_deref(), login_shell_path_dirs())
}

fn resolve_binary_from(
    binary: &str,
    path: Option<&OsStr>,
    home: Option<&Path>,
    login_dirs: &[PathBuf],
) -> Option<PathBuf> {
    let binary_path = Path::new(binary);
    if binary_path.components().count() > 1 {
        return binary_path.is_file().then(|| binary_path.to_path_buf());
    }

    launch_path_dirs(path, home, login_dirs)
        .into_iter()
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

fn child_path_from(
    path: Option<&OsStr>,
    home: Option<&Path>,
    login_dirs: &[PathBuf],
) -> Option<String> {
    env::join_paths(launch_path_dirs(path, home, login_dirs))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

fn launch_path_dirs(
    path: Option<&OsStr>,
    home: Option<&Path>,
    login_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();

    if let Some(path) = path {
        for dir in env::split_paths(path) {
            push_unique_path(&mut dirs, &mut seen, dir);
        }
    }

    // The user's real login-shell PATH. A GUI app launched from Finder/Dock only
    // inherits the bare launchd PATH, so without this the dirs where tools like
    // `claude` actually live (custom npm prefixes, version-manager shims, paths
    // exported in ~/.zprofile or ~/.zshrc) would be missing entirely.
    for dir in login_dirs {
        push_unique_path(&mut dirs, &mut seen, dir.clone());
    }

    if let Some(home) = home {
        for relative in HOME_FALLBACK_DIRS {
            push_unique_path(&mut dirs, &mut seen, home.join(relative));
        }
    }

    for absolute in SYSTEM_FALLBACK_DIRS {
        push_unique_path(&mut dirs, &mut seen, PathBuf::from(absolute));
    }

    dirs
}

/// The PATH directories reported by the user's login shell, resolved once and
/// cached. Empty when no shell is configured or the probe fails/ times out.
fn login_shell_path_dirs() -> &'static [PathBuf] {
    static CACHE: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(|| {
        env::var_os("SHELL")
            .and_then(|shell| login_shell_path(&shell))
            .map(|path| {
                env::split_paths(&path)
                    .filter(|dir| !dir.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Runs the login shell as an interactive login shell and captures its `$PATH`.
///
/// `-ilc` makes zsh/bash source the same startup files a real terminal would
/// (.zshenv/.zprofile/.zshrc, .bash_profile/.bashrc), which is where PATH is
/// typically set. stdout is framed with markers so any banner an rc file prints
/// is discarded, stdin is /dev/null so an rc that reads input can't hang, and a
/// timeout guards against a misbehaving profile stalling startup.
fn login_shell_path(shell: &OsStr) -> Option<String> {
    const MARKER_START: &str = "__QMUX_PATH_START__";
    const MARKER_END: &str = "__QMUX_PATH_END__";
    let script = format!("printf '%s%s%s' '{MARKER_START}' \"$PATH\" '{MARKER_END}'");

    let mut child = Command::new(shell)
        .arg("-ilc")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        let _ = tx.send(buffer);
    });

    let output = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(output) => {
            let _ = child.wait();
            output
        }
        Err(_) => {
            // Kill the child so its stdout closes and the reader's blocking
            // read_to_string returns EOF; joining below then reaps the thread
            // rather than detaching it to linger past this call.
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
    };

    let _ = reader.join();
    extract_between(&output, MARKER_START, MARKER_END)
}

/// Returns the substring framed by `start`/`end` markers, if both are present.
fn extract_between(haystack: &str, start: &str, end: &str) -> Option<String> {
    let from = haystack.find(start)? + start.len();
    let rest = &haystack[from..];
    let to = rest.find(end)?;
    Some(rest[..to].to_string())
}

fn push_unique_path(dirs: &mut Vec<PathBuf>, seen: &mut HashSet<OsString>, dir: PathBuf) {
    if seen.insert(dir.as_os_str().to_os_string()) {
        dirs.push(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("qmux-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test").unwrap();
    }

    #[test]
    fn resolves_binary_from_user_fallback_dirs_when_path_is_minimal() {
        let home = temp_root("home-fallback");
        let binary = home.join(".local/bin/claude");
        touch(&binary);

        let resolved = resolve_binary_from(
            "claude",
            Some(OsStr::new("/usr/bin:/bin")),
            Some(&home),
            &[],
        );

        assert_eq!(resolved, Some(binary));
    }

    #[test]
    fn path_entries_take_precedence_over_fallback_dirs() {
        let root = temp_root("path-precedence");
        let path_bin = root.join("path-bin");
        let home = root.join("home");
        let path_binary = path_bin.join("codex");
        let fallback_binary = home.join(".local/bin/codex");
        touch(&path_binary);
        touch(&fallback_binary);
        let path = env::join_paths([path_bin]).unwrap();

        let resolved = resolve_binary_from("codex", Some(path.as_os_str()), Some(&home), &[]);

        assert_eq!(resolved, Some(path_binary));
    }

    #[test]
    fn slash_containing_binary_is_checked_directly() {
        let root = temp_root("direct-binary");
        let binary = root.join("tools/claude");
        touch(&binary);

        let resolved = resolve_binary_from(binary.to_str().unwrap(), None, None, &[]);

        assert_eq!(resolved, Some(binary));
        assert!(resolve_binary_from("/missing/claude", None, None, &[]).is_none());
    }

    #[test]
    fn child_path_appends_user_and_system_fallback_dirs() {
        let home = PathBuf::from("/Users/tester");
        let child_path =
            child_path_from(Some(OsStr::new("/usr/bin:/bin")), Some(&home), &[]).unwrap();
        let dirs = env::split_paths(OsStr::new(&child_path)).collect::<Vec<_>>();

        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert_eq!(dirs[1], PathBuf::from("/bin"));
        assert!(dirs.contains(&PathBuf::from("/Users/tester/.local/bin")));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn login_shell_dirs_take_precedence_over_fallback_dirs() {
        let root = temp_root("login-precedence");
        let login_bin = root.join("login-bin");
        let home = root.join("home");
        let login_binary = login_bin.join("claude");
        let fallback_binary = home.join(".local/bin/claude");
        touch(&login_binary);
        touch(&fallback_binary);

        let resolved = resolve_binary_from(
            "claude",
            Some(OsStr::new("/usr/bin:/bin")),
            Some(&home),
            std::slice::from_ref(&login_bin),
        );

        assert_eq!(resolved, Some(login_binary));
    }

    #[test]
    fn login_shell_dirs_land_in_child_path() {
        let home = PathBuf::from("/Users/tester");
        let login_dirs = vec![
            PathBuf::from("/Users/tester/.bun/bin"),
            PathBuf::from("/custom/bin"),
        ];
        let child_path =
            child_path_from(Some(OsStr::new("/usr/bin")), Some(&home), &login_dirs).unwrap();
        let dirs = env::split_paths(OsStr::new(&child_path)).collect::<Vec<_>>();

        // Process PATH still leads; the login-shell dirs follow before the
        // hardcoded fallbacks.
        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert_eq!(dirs[1], PathBuf::from("/Users/tester/.bun/bin"));
        assert_eq!(dirs[2], PathBuf::from("/custom/bin"));
    }

    #[test]
    fn extract_between_pulls_framed_value() {
        let raw = "welcome banner\n__S__/opt/homebrew/bin:/usr/bin__E__trailing";
        assert_eq!(
            extract_between(raw, "__S__", "__E__"),
            Some("/opt/homebrew/bin:/usr/bin".to_string())
        );
        assert_eq!(extract_between("no markers here", "__S__", "__E__"), None);
    }
}
