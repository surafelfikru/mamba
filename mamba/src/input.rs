//! Everything a build's parameters come from, in one place.
//!
//! Mamba reads what it needs from two sources — the `cargo` command line and
//! `.mamba.toml` — and most switches can be written in either. [`SETTINGS`] is the only
//! place that knows how each one is spelled, so adding a switch is a row in that table and
//! nothing else: the flag, the config key, and the usage line all follow from it.
//!
//! [`plan`] is the single entry point. It turns a raw command line and a directory into
//! the one decision Mamba makes — run this locally, run it elsewhere, or refuse because
//! the settings are wrong:
//!
//! ```
//! match plan(&cwd, &argv[1..]) {
//!     Plan::Local(args) => exec_real_cargo(&args),
//!     Plan::Remote(invocation) => build_remotely(&invocation),
//!     Plan::Broken(e) => eprintln!("mamba: {e}"),
//! }
//! ```
//!
//! Values that leave this module carry types rather than strings — [`Host`],
//! [`ProjectId`], [`Settings`] — so the checks each one needs happen once, here, instead
//! of at every place they are used.

use std::fmt;
use std::ops::BitOr;
use std::path::{Path, PathBuf};

/// The file whose presence marks a directory as a Mamba project.
pub const CONFIG_FILE: &str = ".mamba.toml";

/// Where Mamba keeps what is not part of any project: the daemon's socket and the ssh
/// multiplexing sockets. `/tmp` stands in for a process with no `HOME`, which is worse but
/// still works.
pub fn mamba_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".mamba")
}

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
    pub fn new(raw: &str) -> Result<Host, InputError> {
        let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@');
        if raw.is_empty() || !raw.chars().all(safe) {
            return Err(InputError::BadHost(raw.to_string()));
        }
        Ok(Host(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Names a project on the far side: it identifies a tenant in a request and becomes a
/// directory on another machine. The only constructor replaces anything that is not a
/// plain name, so no caller has to remember to check.
///
/// Derived from the project directory rather than configured — it says who you are, and a
/// developer building on their own machine does not get to pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn new(raw: &str) -> ProjectId {
        let cleaned: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        match cleaned.as_str() {
            "" | "." | ".." => ProjectId("project".to_string()),
            _ => ProjectId(cleaned),
        }
    }

    /// A project is named after the directory holding its config file.
    fn from_root(root: &ProjectRoot) -> ProjectId {
        ProjectId::new(
            root.as_path()
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project"),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The directory holding a project's `.mamba.toml`, and the root of everything that gets
/// uploaded. [`plan`] builds one when it finds the file; the daemon rebuilds one from the
/// path the shim sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot(PathBuf);

impl ProjectRoot {
    pub fn new(path: PathBuf) -> ProjectRoot {
        ProjectRoot(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// One of Mamba's optional behaviours. The value is the bit it occupies in a [`Settings`]
/// set, which is also the bit it travels to the daemon as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch(u8);

pub const PULL: Switch = Switch(1);
pub const SYMBOLS: Switch = Switch(1 << 1);

/// How one switch is written in each place a person can write it.
pub struct Setting {
    pub switch: Switch,
    /// On a `cargo build` command line. Stripped before cargo ever sees it.
    pub flag: &'static str,
    /// In `.mamba.toml`, as a boolean.
    pub key: &'static str,
    /// How the usage text explains it.
    pub help: &'static str,
}

/// Every switch Mamba has, and the only place any of them is named.
pub const SETTINGS: &[Setting] = &[
    Setting {
        switch: PULL,
        flag: "--mamba-pull",
        key: "pull",
        help: "fetch the built binary back",
    },
    Setting {
        switch: SYMBOLS,
        flag: "--mamba-pull-symbols",
        key: "pull-symbols",
        help: "also fetch debug symbols for it",
    },
];

/// The switches turned on for one build, whichever source asked for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settings(u8);

impl Settings {
    /// Builds a set from raw bits, dropping any that name no switch and applying the one
    /// rule that holds between switches: symbols are useless without the binary they
    /// describe, because the stripped binary carries a link to them and debuggers follow
    /// it. Every set is made here, so that rule holds however a switch arrived — a flag, a
    /// config key, or a request read off the socket.
    fn new(bits: u8) -> Settings {
        let known = SETTINGS.iter().fold(0, |all, s| all | s.switch.0);
        let bits = bits & known;

        Settings(if bits & SYMBOLS.0 != 0 {
            bits | PULL.0
        } else {
            bits
        })
    }

    /// The switches named on a command line.
    pub fn from_flags(args: &[String]) -> Settings {
        Settings::new(
            SETTINGS
                .iter()
                .filter(|s| args.iter().any(|a| a == s.flag))
                .fold(0, |bits, s| bits | s.switch.0),
        )
    }

    /// The switches turned on in a parsed config file. A key of the wrong type is an error
    /// rather than a silent false — whoever wrote it meant something by it.
    fn from_table(table: &toml::Table) -> Result<Settings, InputError> {
        let mut bits = 0;
        for setting in SETTINGS {
            let Some(value) = table.get(setting.key) else {
                continue;
            };
            let on = value
                .as_bool()
                .ok_or_else(|| InputError::BadSetting(setting.key, format!("{value:?}")))?;
            if on {
                bits |= setting.switch.0;
            }
        }
        Ok(Settings::new(bits))
    }

    pub fn has(self, switch: Switch) -> bool {
        self.0 & switch.0 != 0
    }

    /// The single byte a set travels to the daemon as.
    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn from_bits(bits: u8) -> Settings {
        Settings::new(bits)
    }
}

/// Merging two sources of switches is a union — anything asked for anywhere is on. This is
/// how the command line beats the config file without either side knowing about the other.
impl BitOr for Settings {
    type Output = Settings;

    fn bitor(self, other: Settings) -> Settings {
        Settings::new(self.0 | other.0)
    }
}

/// Everything one remote build needs, resolved from both input sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub root: ProjectRoot,
    pub host: Host,
    pub project_id: ProjectId,
    pub settings: Settings,
    /// Cargo's own flags, with the `build` subcommand and Mamba's own flags removed.
    pub flags: Vec<String>,
}

impl Invocation {
    /// The command line that runs this same build locally instead, used when the remote
    /// cannot be reached.
    pub fn local_argv(&self) -> Vec<String> {
        std::iter::once("build".to_string())
            .chain(self.flags.iter().cloned())
            .collect()
    }
}

/// What one `cargo` command line means to Mamba.
#[derive(Debug)]
pub enum Plan {
    /// Not Mamba's business. Hand these arguments to the real cargo.
    Local(Vec<String>),
    /// Build this somewhere else.
    Remote(Invocation),
    /// Someone asked for remote builds and got the settings wrong. Saying so beats
    /// quietly building locally, which looks like it worked.
    Broken(InputError),
}

/// Decides what to do with one `cargo` command line, `args` being that line with the
/// program name already removed.
///
/// Mamba's own flags are stripped here, once, so they can never reach a real cargo —
/// local or remote. The project is then looked for by walking up from `cwd` the way cargo
/// looks for `Cargo.toml`; a directory with no config file anywhere above it is an
/// ordinary Rust project and cargo runs untouched.
pub fn plan(cwd: &Path, args: &[String]) -> Plan {
    let from_flags = Settings::from_flags(args);
    let cargo: Vec<String> = args
        .iter()
        .filter(|a| !SETTINGS.iter().any(|s| s.flag == a.as_str()))
        .cloned()
        .collect();

    // Only `build` goes remote. Everything else is cargo's business.
    if cargo.first().map(String::as_str) != Some("build") {
        return Plan::Local(cargo);
    }

    let Some(root) = cwd.ancestors().find(|dir| dir.join(CONFIG_FILE).is_file()) else {
        return Plan::Local(cargo);
    };

    match load(
        ProjectRoot(root.to_path_buf()),
        from_flags,
        cargo[1..].to_vec(),
    ) {
        Ok(invocation) => Plan::Remote(invocation),
        Err(e) => Plan::Broken(e),
    }
}

/// Reads the config file sitting in `root` and folds it together with what the command
/// line already asked for.
fn load(
    root: ProjectRoot,
    from_flags: Settings,
    flags: Vec<String>,
) -> Result<Invocation, InputError> {
    let path = root.as_path().join(CONFIG_FILE);

    let text = std::fs::read_to_string(&path)
        .map_err(|e| InputError::Unreadable(path.clone(), e.to_string()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| InputError::Syntax(path.clone(), e.to_string()))?;

    let host = Host::new(
        table
            .get("host")
            .and_then(|v| v.as_str())
            .ok_or(InputError::MissingHost)?,
    )?;

    Ok(Invocation {
        project_id: ProjectId::from_root(&root),
        host,
        settings: from_flags | Settings::from_table(&table)?,
        root,
        flags,
    })
}

/// Every way the inputs can be wrong.
#[derive(Debug)]
pub enum InputError {
    Unreadable(PathBuf, String),
    Syntax(PathBuf, String),
    MissingHost,
    BadHost(String),
    /// A switch's key was there but was not a boolean.
    BadSetting(&'static str, String),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::Unreadable(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            InputError::Syntax(p, e) => write!(f, "{} is not valid TOML: {e}", p.display()),
            InputError::MissingHost => {
                write!(
                    f,
                    "{CONFIG_FILE} needs a `host` key, for example: host = \"gpu-box\""
                )
            }
            InputError::BadHost(h) => {
                write!(
                    f,
                    "host {h:?} is not a plain ssh destination (letters, digits, . - _ @ only)"
                )
            }
            InputError::BadSetting(key, value) => {
                write!(f, "{key} must be true or false, got {value}")
            }
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

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The invocation `plan` produced, or a panic naming what it produced instead.
    fn remote(cwd: &Path, argv: &[&str]) -> Invocation {
        match plan(cwd, &args(argv)) {
            Plan::Remote(invocation) => invocation,
            other => panic!("expected a remote build, got {other:?}"),
        }
    }

    #[test]
    fn host_accepts_plain_and_user_at_host() {
        assert_eq!(Host::new("gpu-box").unwrap().as_str(), "gpu-box");
        assert_eq!(
            Host::new("sura@10.0.0.4").unwrap().as_str(),
            "sura@10.0.0.4"
        );
        assert_eq!(Host::new("build_1.lan").unwrap().as_str(), "build_1.lan");
    }

    #[test]
    fn host_rejects_shell_metacharacters_and_empty() {
        for bad in [
            "",
            "host; rm -rf /",
            "host $(id)",
            "host name",
            "a`b`",
            "h|c",
            "x'y",
        ] {
            assert!(Host::new(bad).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn a_project_id_is_always_a_plain_name() {
        // It crosses the wire and becomes a directory on another machine, so the
        // constructor has to leave nothing a shell or a path could reinterpret.
        for raw in ["my project!", "../../etc", "", ".", "..", "a;b"] {
            let id = ProjectId::new(raw);
            assert!(
                id.as_str()
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
                "{raw:?} produced {id:?}"
            );
            assert!(!id.as_str().is_empty());
        }
    }

    #[test]
    fn project_id_is_the_directory_name_and_is_never_configured() {
        let parent = tmpdir("pid-derived");
        let root = parent.join("myproject");
        fs::create_dir_all(&root).unwrap();
        // A project_id key in the file must be ignored, not honoured.
        fs::write(
            root.join(CONFIG_FILE),
            "host = \"gpu-box\"\nproject_id = \"ignored\"\n",
        )
        .unwrap();

        assert_eq!(remote(&root, &["build"]).project_id.as_str(), "myproject");
    }

    #[test]
    fn every_setting_is_spelled_once_and_owns_a_bit_of_its_own() {
        // The whole point of the table: a new switch cannot silently collide with an
        // existing one, and cannot be given two different names in two different places.
        let mut seen = 0u8;
        for setting in SETTINGS {
            assert_eq!(
                setting.switch.0.count_ones(),
                1,
                "{} must own exactly one bit",
                setting.key
            );
            assert_eq!(seen & setting.switch.0, 0, "{} reuses a bit", setting.key);
            seen |= setting.switch.0;

            assert!(setting.flag.starts_with("--mamba-"));
            assert!(!setting.help.is_empty());
        }
    }

    #[test]
    fn mamba_flags_are_stripped_before_cargo_ever_sees_them() {
        // They are Mamba's flags, not cargo's. A real cargo given one fails the build.
        let dir = tmpdir("strip");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let flags = remote(&dir, &["build", "--mamba-pull", "--release"]).flags;

        assert_eq!(flags, args(&["--release"]));
    }

    #[test]
    fn a_mamba_flag_on_a_command_that_stays_local_is_stripped_too() {
        let dir = tmpdir("strip-local");

        match plan(&dir, &args(&["test", "--mamba-pull"])) {
            Plan::Local(args) => assert_eq!(args, vec!["test".to_string()]),
            other => panic!("expected a local run, got {other:?}"),
        }
    }

    #[test]
    fn a_flag_turns_a_switch_on_that_the_config_file_left_off() {
        let dir = tmpdir("flag-wins");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\npull = false\n").unwrap();

        let settings = remote(&dir, &["build", "--mamba-pull"]).settings;

        assert!(settings.has(PULL), "what someone just typed has to win");
    }

    #[test]
    fn a_config_file_turns_a_switch_on_without_any_flag() {
        let dir = tmpdir("file-alone");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\npull = true\n").unwrap();

        assert!(remote(&dir, &["build"]).settings.has(PULL));
    }

    #[test]
    fn switches_default_to_off() {
        let dir = tmpdir("defaults");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let settings = remote(&dir, &["build"]).settings;

        assert!(!settings.has(PULL));
        assert!(!settings.has(SYMBOLS));
    }

    #[test]
    fn asking_for_symbols_asks_for_the_binary_they_belong_to() {
        // Symbols alone are useless: the stripped binary carries the debuglink that points
        // at them. This has to hold from either source, or the two disagree.
        let dir = tmpdir("symbols-imply-pull");
        fs::write(
            dir.join(CONFIG_FILE),
            "host = \"gpu-box\"\npull-symbols = true\n",
        )
        .unwrap();

        assert!(remote(&dir, &["build"]).settings.has(PULL));
        assert!(Settings::from_flags(&args(&["--mamba-pull-symbols"])).has(PULL));
    }

    #[test]
    fn a_switch_of_the_wrong_type_is_an_error_not_a_silent_false() {
        for line in ["pull = \"yes\"", "pull-symbols = 1"] {
            let dir = tmpdir("bad-switch");
            fs::write(
                dir.join(CONFIG_FILE),
                format!("host = \"gpu-box\"\n{line}\n"),
            )
            .unwrap();

            assert!(
                matches!(plan(&dir, &args(&["build"])), Plan::Broken(_)),
                "{line} should have been rejected"
            );
        }
    }

    #[test]
    fn bits_survive_the_trip_through_the_wire_and_drop_ones_that_name_nothing() {
        let both = Settings::from_flags(&args(&["--mamba-pull-symbols"]));

        assert_eq!(Settings::from_bits(both.bits()), both);
        assert_eq!(Settings::from_bits(0b1111_1100), Settings::default());
    }

    #[test]
    fn discovery_walks_up_to_a_parent_the_way_cargo_does() {
        let root = tmpdir("discover-parent");
        let nested = root.join("crates").join("inner").join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let invocation = remote(&nested, &["build"]);

        assert_eq!(invocation.root.as_path(), root);
        assert_eq!(invocation.host.as_str(), "gpu-box");
    }

    #[test]
    fn a_directory_with_no_config_anywhere_above_it_just_runs_cargo() {
        let dir = tmpdir("discover-none");

        assert!(matches!(plan(&dir, &args(&["build"])), Plan::Local(_)));
    }

    #[test]
    fn only_build_goes_remote() {
        let dir = tmpdir("only-build");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        for command in ["test", "check", "run", "clippy"] {
            assert!(
                matches!(plan(&dir, &args(&[command])), Plan::Local(_)),
                "{command} is cargo's business"
            );
        }
    }

    #[test]
    fn a_bare_cargo_with_no_arguments_at_all_stays_local() {
        let dir = tmpdir("bare");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        assert!(matches!(plan(&dir, &[]), Plan::Local(_)));
    }

    #[test]
    fn a_broken_config_is_reported_rather_than_ignored() {
        // Silently building locally would look like it worked, which is the one outcome
        // someone who configured a remote build must not get.
        for contents in [
            "pull = true\n",
            "host = \n",
            "host = 42\n",
            "host = \"a b\"\n",
        ] {
            let dir = tmpdir("broken");
            fs::write(dir.join(CONFIG_FILE), contents).unwrap();

            assert!(
                matches!(plan(&dir, &args(&["build"])), Plan::Broken(_)),
                "{contents:?} should have been reported"
            );
        }
    }

    #[test]
    fn the_local_fallback_runs_the_same_build_that_was_asked_for() {
        let dir = tmpdir("local-argv");
        fs::write(dir.join(CONFIG_FILE), "host = \"gpu-box\"\n").unwrap();

        let invocation = remote(&dir, &["build", "--mamba-pull", "--release", "-p", "app"]);

        assert_eq!(
            invocation.local_argv(),
            args(&["build", "--release", "-p", "app"]),
            "the same flags, minus Mamba's own"
        );
    }

    #[test]
    fn mamba_home_is_one_directory_every_caller_agrees_on() {
        assert!(mamba_home().ends_with(".mamba"));
    }
}
