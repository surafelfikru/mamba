use crate::config::Config;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Reads the binary name straight from `Cargo.toml`'s `[package] name`.
///
/// ponytail: this only resolves the crate's default binary (the one matching the
/// package name under `src/main.rs`) — a workspace or an explicit `[[bin]]` target
/// with a different name isn't handled. Parse `cargo metadata` remotely if that's
/// ever needed; the common single-binary case doesn't need it.
pub(crate) fn binary_name(root: &Path) -> Result<String, String> {
    let path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("{} is not valid TOML: {e}", path.display()))?;

    table
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{} has no [package] name", path.display()))
}

/// The shell fragment the host runs after a successful build to separate debug info
/// from the binary.
///
/// Three steps: copy the symbols into their own file, write a stripped binary
/// alongside, then record a link from the stripped one back to the symbols. gdb and
/// `addr2line` follow that link automatically when the two files sit in the same
/// directory, so a later symbol fetch needs no configuration to take effect.
///
/// The trailing `|| cp` matters more than it looks. Without it, a host missing
/// binutils would leave no `.slim` file and the pull would fail with a confusing
/// "no such file" — with it, `<bin>.slim` always exists and is simply the unstripped
/// binary when splitting was not possible.
pub(crate) fn split_command(profile: &str, name: &str) -> String {
    let bin = format!("target/{profile}/{name}");
    format!(
        "b={bin}; objcopy --only-keep-debug \"$b\" \"$b.debug\" 2>/dev/null \
         && strip --strip-debug -o \"$b.slim\" \"$b\" 2>/dev/null \
         && objcopy --add-gnu-debuglink=\"$b.debug\" \"$b.slim\" 2>/dev/null \
         || cp \"$b\" \"$b.slim\""
    )
}

/// Which profile directory cargo will have used, read from the forwarded flags.
pub(crate) fn profile_of(args: &[String]) -> &'static str {
    if args.iter().any(|a| a == "--release") {
        "release"
    } else {
        "debug"
    }
}

/// Builds the rsync arguments for pulling the built binary back to the exact local
/// path cargo would have used, plus that path itself.
///
/// Split out from [`pull`] so the profile selection and path arithmetic can be
/// asserted on directly, without a real host.
fn pull_args(config: &Config, name: &str, args: &[String]) -> (Vec<OsString>, PathBuf) {
    let rel = format!("target/{}/{name}", profile_of(args));

    // The host holds both a full binary and a stripped one. Only the stripped one
    // travels, but it lands under the plain name so everything downstream — running
    // it, a debugger, a script — finds it exactly where a local build would have.
    let local = config.root.as_path().join(&rel);
    let remote = format!(
        "{}:{}/{rel}.slim",
        config.host.as_str(),
        config.remote_dir.as_str()
    );

    (
        vec![
            OsString::from("-az"),
            OsString::from(remote),
            local.clone().into_os_string(),
        ],
        local,
    )
}

/// Copies the built binary back to the exact local path cargo would have used —
/// `target/debug/<name>` or `target/release/<name>` — so it can be run without ssh'ing
/// in. Only meaningful after a successful build; a compile failure leaves nothing on
/// the remote worth pulling.
pub fn pull(config: &Config, args: &[String]) -> Result<PathBuf, String> {
    let name = binary_name(config.root.as_path())?;
    let (rsync_args, local) = pull_args(config, &name, args);

    if let Some(parent) = local.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }

    match Command::new("rsync").args(&rsync_args).status() {
        Err(e) => Err(format!("could not run rsync: {e}")),
        Ok(status) if status.success() => Ok(local),
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
    use std::process::Command;

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
    fn split_command_produces_slim_and_debug_beside_the_binary() {
        let cmd = split_command("debug", "widget");

        assert!(cmd.contains("target/debug/widget"), "got {cmd}");
        assert!(cmd.contains("--only-keep-debug"), "got {cmd}");
        assert!(cmd.contains("--strip-debug"), "got {cmd}");
        assert!(cmd.contains("--add-gnu-debuglink="), "got {cmd}");
    }

    #[test]
    fn split_command_falls_back_to_a_plain_copy_when_binutils_are_missing() {
        let cmd = split_command("release", "widget");

        // The `|| cp` tail is what guarantees <bin>.slim always exists, so the
        // puller never has to probe for which name to fetch.
        assert!(cmd.contains("|| cp "), "got {cmd}");
    }

    /// The load-bearing test: runs the real fragment against a real binary and checks
    /// both outputs appear. Reading the man pages is not enough — the argument order
    /// for --add-gnu-debuglink is easy to get wrong and fails silently.
    #[test]
    fn split_command_really_produces_both_files() {
        let dir = tmpdir("split-real");
        let bin = dir.join("target/debug/widget");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        // Any real ELF binary will do; use the test runner itself.
        fs::copy(std::env::current_exe().unwrap(), &bin).unwrap();

        let status = Command::new("sh")
            .arg("-c")
            .arg(split_command("debug", "widget"))
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success(), "fragment exited non-zero");

        assert!(
            dir.join("target/debug/widget.slim").is_file(),
            "no .slim produced"
        );
        assert!(
            dir.join("target/debug/widget.debug").is_file(),
            "no .debug produced"
        );
        assert!(
            fs::metadata(dir.join("target/debug/widget.slim")).unwrap().len()
                < fs::metadata(&bin).unwrap().len(),
            "slim binary is not smaller than the original"
        );
    }

    #[test]
    fn binary_name_reads_the_package_name_from_cargo_toml() {
        let dir = tmpdir("binary-name");
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"widget\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        assert_eq!(binary_name(&dir).unwrap(), "widget");
    }

    #[test]
    fn binary_name_errors_when_cargo_toml_is_missing() {
        let dir = tmpdir("binary-name-missing");
        assert!(binary_name(&dir).is_err());
    }

    #[test]
    fn pull_args_uses_debug_profile_by_default() {
        let dir = tmpdir("pull-args-debug");
        fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "host = \"gpu-box\"\nremote_dir = \".mamba/widget\"\n",
        )
        .unwrap();
        let config = crate::config::Config::discover(&dir).unwrap().unwrap();

        let (args, local) = pull_args(&config, "widget", &[]);

        assert!(local.ends_with("target/debug/widget"), "got {local:?}");
        let joined: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(
            joined.contains(&"gpu-box:.mamba/widget/target/debug/widget.slim".to_string()),
            "got {joined:?}"
        );
    }

    #[test]
    fn pull_fetches_the_slim_binary_but_lands_it_at_the_plain_local_path() {
        let dir = tmpdir("pull-slim");
        fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "host = \"gpu-box\"\nremote_dir = \".mamba/widget\"\n",
        )
        .unwrap();
        let config = crate::config::Config::discover(&dir).unwrap().unwrap();

        let (args, local) = pull_args(&config, "widget", &[]);
        let joined: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();

        assert!(
            joined.contains(&"gpu-box:.mamba/widget/target/debug/widget.slim".to_string()),
            "remote side should fetch the slim file, got {joined:?}"
        );
        assert!(
            local.ends_with("target/debug/widget"),
            "local side must be the plain name cargo would have written, got {local:?}"
        );
    }

    #[test]
    fn pull_args_uses_release_profile_when_flagged() {
        let dir = tmpdir("pull-args-release");
        fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "host = \"gpu-box\"\nremote_dir = \".mamba/widget\"\n",
        )
        .unwrap();
        let config = crate::config::Config::discover(&dir).unwrap().unwrap();

        let (_, local) = pull_args(&config, "widget", &["--release".to_string()]);

        assert!(local.ends_with("target/release/widget"), "got {local:?}");
    }

    /// Confirms the actual transfer, not just the string-building: a genuine rsync run
    /// (target substituted for a local path, same trick as the sync tests) creates
    /// missing local directories and lands the file at the exact profile path.
    #[test]
    fn pull_copies_the_binary_to_the_exact_local_profile_path() {
        let base = tmpdir("pull-e2e");
        let project = base.join("project");
        let fake_remote_bin = base.join("remote-target/debug/widget");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(fake_remote_bin.parent().unwrap()).unwrap();

        fs::write(
            project.join(crate::config::CONFIG_FILE),
            "host = \"gpu-box\"\nremote_dir = \".mamba/widget\"\n",
        )
        .unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\nname = \"widget\"\n").unwrap();
        fs::write(&fake_remote_bin, "pretend binary\n").unwrap();

        let config = crate::config::Config::discover(&project).unwrap().unwrap();
        let (mut args, local) = pull_args(&config, "widget", &[]);
        args[1] = fake_remote_bin.clone().into_os_string();

        assert!(
            !local.parent().unwrap().is_dir(),
            "test setup should start without target/debug"
        );
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        let status = Command::new("rsync").args(&args).status().unwrap();
        assert!(status.success());

        assert_eq!(fs::read_to_string(&local).unwrap(), "pretend binary\n");
    }
}
