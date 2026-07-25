use crate::config::{Config, RemoteDir};
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

/// A string already escaped for a POSIX shell.
///
/// Cargo flags travel from local `argv` into a command line that a remote shell parses,
/// which is exactly where an unescaped argument turns into command injection. The only
/// constructor is [`Quoted::new`], and the remote command is assembled purely from
/// `Quoted` values, so forgetting to escape something is a compile error rather than a
/// security hole.
///
/// ```
/// let flag = Quoted::new("--features a b");
/// assert_eq!(flag.as_str(), "'--features a b'");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quoted(String);

impl Quoted {
    /// Wraps the whole argument in single quotes, which suppress every shell
    /// expansion. A literal single quote cannot appear inside single quotes, so each
    /// one is closed, escaped, and reopened — `it's` becomes `'it'\''s'`.
    pub fn new(raw: &str) -> Quoted {
        Quoted(format!("'{}'", raw.replace('\'', r"'\''")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Builds the rsync command line for pushing a project to the remote machine.
///
/// Split out from [`sync`] so the flags can be asserted on directly, because two of
/// them are subtle and getting either wrong is silent rather than loud. `:- .gitignore`
/// reads your ignore rules to decide what to upload. `P /target/` is a protect rule
/// telling the receiving side never to delete that path — necessary because ignore
/// rules apply only to the sender, so without it `--delete` wipes the remote build
/// cache on every run.
pub fn rsync_args(root: &Path, host: &str, dir: &RemoteDir) -> Vec<OsString> {
    let mut source = root.as_os_str().to_os_string();
    source.push("/");

    let remote_path = format!(
        "--rsync-path=mkdir -p {} && rsync",
        Quoted::new(dir.as_str()).as_str()
    );

    vec![
        OsString::from("-az"),
        OsString::from("--delete"),
        OsString::from("--filter=:- .gitignore"),
        OsString::from("--filter=P /target/"),
        OsString::from("--exclude=.git/"),
        OsString::from(remote_path),
        source,
        OsString::from(format!("{host}:{}/", dir.as_str())),
    ]
}

/// Pushes the project to the remote machine, creating the destination if needed.
///
/// The first run copies everything; later runs send only what changed, so there is no
/// separate incremental mode to enable. An `Err` here always means the transfer could
/// not happen — a bad network, a missing rsync, a refused login — never that the code
/// failed to compile.
pub fn sync(config: &Config) -> Result<(), String> {
    let args = rsync_args(
        config.root.as_path(),
        config.host.as_str(),
        &config.remote_dir,
    );

    match Command::new("rsync").args(&args).status() {
        Err(e) => Err(format!("could not run rsync: {e}")),
        Ok(status) if status.success() => Ok(()),
        Ok(status) => match status.code() {
            Some(code) => Err(format!("rsync exited with {code}")),
            None => Err("rsync was killed by a signal".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    /// Runs `echo` in a real shell with `arg` already quoted, and returns what the
    /// shell actually produced. This is the only way to prove the escaping works —
    /// comparing against a hand-written expected string only tests our own opinion.
    fn round_trip(arg: &str) -> String {
        let script = format!("printf '%s' {}", Quoted::new(arg).as_str());
        let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert!(out.status.success(), "shell rejected: {script}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mamba-{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args_as_strings(root: &Path, host: &str, dir: &str) -> Vec<String> {
        rsync_args(root, host, &RemoteDir::new(dir).unwrap())
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn plain_argument_is_unchanged_after_the_shell_sees_it() {
        assert_eq!(round_trip("--release"), "--release");
    }

    #[test]
    fn spaces_survive() {
        assert_eq!(round_trip("--features foo bar"), "--features foo bar");
    }

    #[test]
    fn single_quotes_survive() {
        assert_eq!(round_trip("it's"), "it's");
        assert_eq!(round_trip("'"), "'");
        assert_eq!(round_trip("a'b'c"), "a'b'c");
    }

    #[test]
    fn shell_metacharacters_are_never_executed() {
        assert_eq!(round_trip("$(id)"), "$(id)");
        assert_eq!(round_trip("`id`"), "`id`");
        assert_eq!(round_trip("; rm -rf /"), "; rm -rf /");
        assert_eq!(round_trip("$HOME"), "$HOME");
        assert_eq!(round_trip("a|b&c"), "a|b&c");
    }

    #[test]
    fn empty_argument_stays_a_single_empty_argument() {
        assert_eq!(Quoted::new("").as_str(), "''");
        assert_eq!(round_trip(""), "");
    }

    #[test]
    fn rsync_args_carry_both_filter_rules() {
        let args = args_as_strings(Path::new("/home/me/proj"), "gpu-box", ".mamba/proj");

        assert!(args.contains(&"--filter=:- .gitignore".to_string()));
        assert!(args.contains(&"--filter=P /target/".to_string()));
        assert!(args.contains(&"--delete".to_string()));
        assert!(args.contains(&"--exclude=.git/".to_string()));
    }

    #[test]
    fn rsync_source_has_a_trailing_slash_so_contents_are_copied_not_the_directory() {
        let args = args_as_strings(Path::new("/home/me/proj"), "gpu-box", ".mamba/proj");
        assert!(args.contains(&"/home/me/proj/".to_string()), "got {args:?}");
    }

    #[test]
    fn rsync_destination_is_host_colon_dir() {
        let args = args_as_strings(Path::new("/home/me/proj"), "gpu-box", ".mamba/proj");
        assert!(args.contains(&"gpu-box:.mamba/proj/".to_string()), "got {args:?}");
    }

    #[test]
    fn rsync_creates_the_remote_directory_before_transferring() {
        let args = args_as_strings(Path::new("/home/me/proj"), "gpu-box", ".mamba/proj");
        assert!(
            args.iter()
                .any(|a| a.starts_with("--rsync-path=mkdir -p '.mamba/proj'")),
            "got {args:?}"
        );
    }

    /// The load-bearing test. Runs the real flags between two local directories and
    /// checks that the receiver's `target/` is left alone while genuinely stale files
    /// are removed. A per-directory `.gitignore` merge rule does NOT protect anything
    /// on the receiving side, so dropping `--filter=P /target/` silently destroys the
    /// remote build cache on every sync — which is the entire point of building
    /// remotely. This test is what catches that.
    #[test]
    fn delete_never_touches_the_receivers_target_directory() {
        let base = tmpdir("rsync-protect");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(src.join("target")).unwrap();
        fs::create_dir_all(dst.join("target")).unwrap();

        fs::write(src.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(src.join("debug.log"), "noise\n").unwrap();
        fs::write(src.join("target").join("local.o"), "local build\n").unwrap();

        fs::write(dst.join("target").join("cached.o"), "remote cache\n").unwrap();
        fs::write(dst.join("stale.rs"), "deleted upstream\n").unwrap();

        let args = rsync_args(&src, "", &RemoteDir::new("ignored").unwrap());
        // Swap the remote destination for a local one; every other flag is untouched.
        let mut local: Vec<OsString> = args
            .into_iter()
            .filter(|a| !a.to_string_lossy().starts_with("--rsync-path="))
            .collect();
        local.pop();
        local.push(dst.as_os_str().to_os_string());

        let status = Command::new("rsync").args(&local).status().unwrap();
        assert!(status.success(), "rsync failed");

        assert!(
            dst.join("target").join("cached.o").is_file(),
            "remote build cache was destroyed"
        );
        assert!(dst.join("main.rs").is_file(), "source was not copied");
        assert!(!dst.join("stale.rs").exists(), "stale file was not deleted");
        assert!(!dst.join("debug.log").exists(), "gitignored file was uploaded");
        assert!(
            !dst.join("target").join("local.o").exists(),
            "local target/ was uploaded"
        );
    }

    #[test]
    fn syncing_to_an_unreachable_host_reports_an_error_instead_of_panicking() {
        let dir = tmpdir("rsync-unreachable");
        fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "host = \"nonexistent.invalid\"\n",
        )
        .unwrap();
        let config = crate::config::Config::discover(&dir).unwrap().unwrap();

        assert!(sync(&config).is_err());
    }
}
