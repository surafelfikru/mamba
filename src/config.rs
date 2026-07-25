use std::fmt;
use std::path::{Path, PathBuf};

/// The file whose presence marks a directory as a Mamba project.
pub const CONFIG_FILE: &str = ".mamba.toml";

/// An ssh destination: a bare hostname, a `user@host` pair, or an alias from
/// `~/.ssh/config`. The value is pasted onto an ssh and rsync command line, so the
/// constructor refuses anything a shell could reinterpret — that check happens once
/// here rather than at every use.
///
/// ```
/// let host = Host::new("gpu-box")?;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host(String);

impl Host {
    pub fn new(raw: &str) -> Result<Host, ConfigError> {
        let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@');
        if raw.is_empty() || !raw.chars().all(safe) {
            return Err(ConfigError::BadHost(raw.to_string()));
        }
        Ok(Host(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the project lands on the remote machine. A relative path is taken from the
/// remote user's home directory, which is how the `.mamba/<name>` default resolves
/// without ever needing a tilde. Single quotes are rejected so the value can be
/// wrapped in them safely when it is sent to a remote shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDir(String);

impl RemoteDir {
    pub fn new(raw: &str) -> Result<RemoteDir, ConfigError> {
        if raw.is_empty() || raw.contains('\'') {
            return Err(ConfigError::BadRemoteDir(raw.to_string()));
        }
        Ok(RemoteDir(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A directory that holds a `.mamba.toml`. Only [`Config::discover`] builds one, so
/// holding a `ProjectRoot` is proof the config file was actually found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot(PathBuf);

impl ProjectRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Everything Mamba needs to run a build somewhere else, read from `.mamba.toml`.
///
/// Start with [`Config::discover`], which searches upward from a directory the way
/// cargo looks for `Cargo.toml`:
///
/// ```
/// match Config::discover(&cwd) {
///     None => /* not a Mamba project — run cargo normally */,
///     Some(Err(e)) => /* the config exists but is broken — report it */,
///     Some(Ok(config)) => /* build remotely */,
/// }
/// ```
#[derive(Debug)]
pub struct Config {
    pub root: ProjectRoot,
    pub host: Host,
    pub remote_dir: RemoteDir,
    pub pull: bool,
}

impl Config {
    /// Searches `start` and each parent for a config file. The three-way return is the
    /// point: `None` means this is an ordinary Rust project and cargo should proceed
    /// untouched, while `Some(Err(..))` means someone asked for remote builds and got
    /// the settings wrong, which must be reported instead of silently ignored.
    pub fn discover(start: &Path) -> Option<Result<Config, ConfigError>> {
        let root = start.ancestors().find(|dir| dir.join(CONFIG_FILE).is_file())?;
        Some(Config::load(ProjectRoot(root.to_path_buf())))
    }

    /// Reads and validates the config file sitting in `root`.
    fn load(root: ProjectRoot) -> Result<Config, ConfigError> {
        let path = root.as_path().join(CONFIG_FILE);

        let text = std::fs::read_to_string(&path)
            .map_err(|e| ConfigError::Unreadable(path.clone(), e.to_string()))?;
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Syntax(path.clone(), e.to_string()))?;

        let host = table
            .get("host")
            .and_then(|v| v.as_str())
            .ok_or(ConfigError::MissingHost)?;
        let host = Host::new(host)?;

        let remote_dir = match table.get("remote_dir").and_then(|v| v.as_str()) {
            Some(dir) => RemoteDir::new(dir)?,
            None => default_remote_dir(&root)?,
        };

        let pull = match table.get("pull") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| ConfigError::BadPull(format!("{v:?}")))?,
        };

        Ok(Config { root, host, remote_dir, pull })
    }
}

/// Builds `.mamba/<project directory name>`, relative to the remote home directory.
fn default_remote_dir(root: &ProjectRoot) -> Result<RemoteDir, ConfigError> {
    let name = root
        .as_path()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    RemoteDir::new(&format!(".mamba/{name}"))
}

/// Every way `.mamba.toml` can be wrong.
#[derive(Debug)]
pub enum ConfigError {
    Unreadable(PathBuf, String),
    Syntax(PathBuf, String),
    MissingHost,
    BadHost(String),
    BadRemoteDir(String),
    BadPull(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Unreadable(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            ConfigError::Syntax(p, e) => write!(f, "{} is not valid TOML: {e}", p.display()),
            ConfigError::MissingHost => {
                write!(f, "{CONFIG_FILE} needs a `host` key, for example: host = \"gpu-box\"")
            }
            ConfigError::BadHost(h) => {
                write!(f, "host {h:?} is not a plain ssh destination (letters, digits, . - _ @ only)")
            }
            ConfigError::BadRemoteDir(d) => write!(f, "remote_dir {d:?} is empty or contains a quote"),
            ConfigError::BadPull(v) => write!(f, "pull must be true or false, got {v}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Makes a throwaway directory under the system temp dir. Named with the process
    /// id and a nanosecond stamp so parallel tests never collide.
    fn tmpdir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mamba-{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn host_accepts_plain_and_user_at_host() {
        assert_eq!(Host::new("gpu-box").unwrap().as_str(), "gpu-box");
        assert_eq!(Host::new("sura@10.0.0.4").unwrap().as_str(), "sura@10.0.0.4");
        assert_eq!(Host::new("build_1.lan").unwrap().as_str(), "build_1.lan");
    }

    #[test]
    fn host_rejects_shell_metacharacters_and_empty() {
        for bad in ["", "host; rm -rf /", "host $(id)", "host name", "a`b`", "h|c", "x'y"] {
            assert!(Host::new(bad).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn remote_dir_rejects_empty_and_single_quote() {
        assert!(RemoteDir::new("").is_err());
        assert!(RemoteDir::new("a'b").is_err());
        assert_eq!(RemoteDir::new(".mamba/x").unwrap().as_str(), ".mamba/x");
    }

    #[test]
    fn discover_finds_config_in_the_starting_directory() {
        let dir = tmpdir("discover-here");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let config = Config::discover(&dir).unwrap().unwrap();

        assert_eq!(config.host.as_str(), "gpu-box");
        assert_eq!(config.root.as_path(), dir);
    }

    #[test]
    fn discover_walks_up_to_a_parent() {
        let root = tmpdir("discover-parent");
        let nested = root.join("crates").join("inner").join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let config = Config::discover(&nested).unwrap().unwrap();

        assert_eq!(config.root.as_path(), root);
    }

    #[test]
    fn discover_returns_none_when_no_config_exists_anywhere_above() {
        let dir = tmpdir("discover-none");
        assert!(Config::discover(&dir).is_none());
    }

    #[test]
    fn default_remote_dir_is_dot_mamba_plus_directory_name() {
        let parent = tmpdir("default-dir");
        let root = parent.join("myproject");
        std::fs::create_dir_all(&root).unwrap();
        fs::write(root.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let config = Config::discover(&root).unwrap().unwrap();

        assert_eq!(config.remote_dir.as_str(), ".mamba/myproject");
    }

    #[test]
    fn explicit_remote_dir_overrides_the_default() {
        let dir = tmpdir("explicit-dir");
        fs::write(
            dir.join(CONFIG_FILE),
            "host = \"gpu-box\"\n# a comment\nremote_dir = \"builds/thing\"\n",
        )
        .unwrap();

        let config = Config::discover(&dir).unwrap().unwrap();

        assert_eq!(config.remote_dir.as_str(), "builds/thing");
    }

    #[test]
    fn missing_host_is_an_error_not_a_silent_pass() {
        let dir = tmpdir("no-host");
        fs::write(dir.join(CONFIG_FILE), "remote_dir = \"x\"\n").unwrap();
        assert!(Config::discover(&dir).unwrap().is_err());
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let dir = tmpdir("bad-toml");
        fs::write(dir.join(CONFIG_FILE), "host = \n").unwrap();
        assert!(Config::discover(&dir).unwrap().is_err());
    }

    #[test]
    fn host_of_the_wrong_type_is_an_error() {
        let dir = tmpdir("host-int");
        fs::write(dir.join(CONFIG_FILE), "host = 42\n").unwrap();
        assert!(Config::discover(&dir).unwrap().is_err());
    }

    #[test]
    fn pull_defaults_to_false_when_absent() {
        let dir = tmpdir("pull-absent");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let config = Config::discover(&dir).unwrap().unwrap();

        assert!(!config.pull);
    }

    #[test]
    fn explicit_pull_true_is_honoured() {
        let dir = tmpdir("pull-true");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\npull = true\n").unwrap();

        let config = Config::discover(&dir).unwrap().unwrap();

        assert!(config.pull);
    }

    #[test]
    fn explicit_pull_false_is_honoured() {
        let dir = tmpdir("pull-false");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\npull = false\n").unwrap();

        let config = Config::discover(&dir).unwrap().unwrap();

        assert!(!config.pull);
    }

    #[test]
    fn pull_of_the_wrong_type_is_an_error() {
        let dir = tmpdir("pull-string");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\npull = \"yes\"\n").unwrap();

        assert!(Config::discover(&dir).unwrap().is_err());
    }
}
