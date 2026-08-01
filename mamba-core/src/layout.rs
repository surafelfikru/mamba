//! Where cargo puts things, and how debug info is separated from a finished binary.
//!
//! Shared rather than server-side because the SSH transport has no server to ask and
//! resolves the same paths itself. Keeping one copy is what stops the two transports
//! disagreeing about where a binary went.

use std::path::Path;

/// Reads the value of a flag written either as `--flag value` or `--flag=value`.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == flag {
            return rest.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(value);
        }
    }
    None
}

/// The profile directory cargo writes into, which is not always the profile's name.
///
/// `dev` and `test` both build into `debug`, and `bench` into `release`; every other name —
/// including a custom profile declared in `Cargo.toml` — uses itself.
fn profile_directory(args: &[String]) -> &str {
    if let Some(name) = flag_value(args, "--profile") {
        return match name {
            "dev" | "test" => "debug",
            "bench" => "release",
            other => other,
        };
    }
    if args.iter().any(|a| a == "--release") {
        "release"
    } else {
        "debug"
    }
}

/// Where under `target/` this build's artifacts land, as a relative path.
///
/// Three flags move it and all three must be read from the arguments the build actually
/// received: `--release`, `--profile <name>`, and `--target <triple>` — the last nesting
/// everything one level deeper.
pub fn target_subdir(args: &[String]) -> String {
    let profile = profile_directory(args);
    match flag_value(args, "--target") {
        Some(triple) => format!("{triple}/{profile}"),
        None => profile.to_string(),
    }
}

/// Reads the binary name straight from `Cargo.toml`'s `[package] name`.
///
/// Only the crate's default binary is resolved — a workspace or an explicit `[[bin]]`
/// target under a different name is not handled.
pub fn binary_name(project_dir: &Path) -> Result<String, String> {
    let path = project_dir.join("Cargo.toml");
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

/// The shell fragment that separates debug info from a built binary, run in the project
/// directory on whichever machine compiled.
///
/// Three steps: copy the symbols out, write a stripped binary beside them, then record a
/// link from the stripped one back to the symbols. gdb and `addr2line` follow that link
/// automatically when the two files sit together, so fetching symbols later needs no
/// configuration to take effect.
///
/// The trailing `|| cp` matters more than it looks: without it a host missing binutils
/// leaves no `.slim` file at all, and the fetch fails with a confusing "no such file"
/// instead of simply moving an unstripped binary.
pub fn split_script(subdir: &str, name: &str) -> String {
    let bin = format!("target/{subdir}/{name}");
    format!(
        "b={bin}; objcopy --only-keep-debug \"$b\" \"$b.debug\" 2>/dev/null \
         && strip --strip-debug -o \"$b.slim\" \"$b\" 2>/dev/null \
         && objcopy --add-gnu-debuglink=\"$b.debug\" \"$b.slim\" 2>/dev/null \
         || cp \"$b\" \"$b.slim\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
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

    #[test]
    fn plain_build_lands_in_debug() {
        assert_eq!(target_subdir(&args(&[])), "debug");
    }

    #[test]
    fn release_flag_lands_in_release() {
        assert_eq!(target_subdir(&args(&["--release"])), "release");
    }

    #[test]
    fn dev_and_test_profiles_both_land_in_debug() {
        assert_eq!(target_subdir(&args(&["--profile", "dev"])), "debug");
        assert_eq!(target_subdir(&args(&["--profile=test"])), "debug");
    }

    #[test]
    fn bench_profile_lands_in_release() {
        assert_eq!(target_subdir(&args(&["--profile", "bench"])), "release");
    }

    #[test]
    fn a_custom_profile_uses_its_own_directory() {
        assert_eq!(target_subdir(&args(&["--profile", "quick"])), "quick");
    }

    #[test]
    fn an_explicit_target_triple_nests_one_level_deeper() {
        assert_eq!(
            target_subdir(&args(&["--target", "aarch64-unknown-linux-gnu", "--release"])),
            "aarch64-unknown-linux-gnu/release"
        );
    }

    #[test]
    fn binary_name_comes_from_the_package_name() {
        let dir = tmpdir("binname");
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"my-app\"\n").unwrap();

        assert_eq!(binary_name(&dir).unwrap(), "my-app");
    }

    #[test]
    fn a_manifest_without_a_package_name_is_an_error() {
        let dir = tmpdir("no-name");
        fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();

        assert!(binary_name(&dir).is_err());
    }

    #[test]
    fn the_split_script_always_leaves_a_slim_file_behind() {
        let script = split_script("debug", "app");

        assert!(script.contains("--only-keep-debug"));
        assert!(script.contains("--add-gnu-debuglink"));
        assert!(
            script.contains("|| cp"),
            "a host without binutils must still produce a .slim, or the pull fails confusingly"
        );
    }
}
