mod artifact;
mod config;
mod remote;

use config::Config;
use remote::{BuildOutcome, Quoted};
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();

    let invoked_as = argv
        .first()
        .map(Path::new)
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");

    if invoked_as != "cargo" {
        print_usage();
        return ExitCode::SUCCESS;
    }

    // `--mamba-pull` is Mamba's own flag, not cargo's — strip it here, once, so it can
    // never reach a real cargo invocation, local or remote.
    let cli_symbols = argv[1..].iter().any(|a| a == "--mamba-symbols");
    let cli_pull = argv[1..].iter().any(|a| a == "--mamba-pull") || cli_symbols;
    let args: Vec<String> = argv[1..]
        .iter()
        .filter(|a| a.as_str() != "--mamba-pull" && a.as_str() != "--mamba-symbols")
        .cloned()
        .collect();

    // Only `build` goes remote. Everything else is cargo's business.
    if args.first().map(String::as_str) != Some("build") {
        return exec_real_cargo(&args);
    }

    let Ok(cwd) = std::env::current_dir() else {
        return exec_real_cargo(&args);
    };

    let config = match Config::discover(&cwd) {
        None => return exec_real_cargo(&args),
        Some(Err(e)) => {
            eprintln!("mamba: {e}");
            return ExitCode::from(1);
        }
        Some(Ok(config)) => config,
    };

    let project = project_name(&config);

    status("Syncing", &format!("{project} to {}", config.host.as_str()));
    if let Err(e) = remote::sync(&config) {
        return offer_local_build(&config, &args, &e);
    }

    let flags: Vec<Quoted> = args[1..].iter().map(|a| Quoted::new(a)).collect();

    // Remote cargo's own "Compiling"/"Finished" lines stream in right after this,
    // over the ssh child's inherited stdio — so the sync line above and the pull
    // line below read as one continuous build log, not three separate tools.
    // Splitting runs on the host, using the CPU we are already paying for. It is
    // skipped silently when the binary name cannot be resolved — a workspace, say —
    // because a missing optimisation must never fail a build.
    let post_build = match artifact::binary_name(config.root.as_path()) {
        Ok(name) => artifact::split_command(artifact::profile_of(&args[1..]), &name),
        Err(_) => String::new(),
    };
    let outcome = remote::build(&config, &flags, &post_build);

    if matches!(outcome, BuildOutcome::Finished(0)) && (cli_pull || config.pull) {
        status("Downloading", &format!("{project} from {}", config.host.as_str()));
        let started = std::time::Instant::now();
        match artifact::pull(&config, &args[1..]) {
            Ok(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                status(
                    "Downloaded",
                    &format!("{} ({}) in {:.2}s", path.display(), human_size(size), started.elapsed().as_secs_f64()),
                );
            }
            Err(e) => eprintln!("mamba: pull failed: {e}"),
        }

        if cli_symbols || config.symbols {
            match artifact::pull_symbols(&config, &args[1..]) {
                Ok(p) => {
                    let n = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                    status("Symbols", &format!("{} ({})", p.display(), human_size(n)));
                }
                Err(e) => eprintln!("mamba: symbols unavailable: {e}"),
            }
        }

        match artifact::sync_proc_macros(&config, &args[1..]) {
            Ok(0) => {}
            Ok(n) => status("Macros", &format!("{n} proc-macro libraries")),
            Err(e) => eprintln!("mamba: proc-macro sync failed: {e}"),
        }
    }

    match outcome {
        BuildOutcome::Finished(code) => ExitCode::from(code.clamp(0, 255) as u8),
        BuildOutcome::Unreachable(why) => offer_local_build(&config, &args, &why),
    }
}

/// Tells the user the remote is down and asks whether to fall back to a local build.
///
/// When there is no terminal to ask on — inside `make`, a build script, or an editor's
/// background check — it falls back without asking, because failing there would break
/// tooling that has nothing to do with Mamba.
fn offer_local_build(config: &Config, args: &[String], why: &str) -> ExitCode {
    eprintln!("mamba: {} unreachable ({why})", config.host.as_str());

    if !io::stdin().is_terminal() {
        eprintln!("mamba: no terminal to ask on, building locally");
        return exec_real_cargo(args);
    }

    eprint!("mamba: build locally instead? [Y/n] ");
    let _ = io::stderr().flush();

    let mut answer = String::new();
    match io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => return exec_real_cargo(args),
        Ok(_) => {}
    }

    if answer_means_yes(&answer) {
        exec_real_cargo(args)
    } else {
        ExitCode::from(1)
    }
}

/// Hands control to the real cargo, replacing this process entirely.
///
/// Using `exec` rather than spawning a child means cargo inherits this process's
/// terminal, signals, and exit status directly — from the caller's point of view the
/// shim was never there.
fn exec_real_cargo(args: &[String]) -> ExitCode {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let me = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_default();

    let Some(cargo) = find_real_cargo(&path_var, &me) else {
        eprintln!("mamba: no cargo found on PATH besides this shim");
        return ExitCode::from(127);
    };

    // Only returns if exec failed.
    let error = Command::new(cargo).args(args).exec();
    eprintln!("mamba: could not start cargo: {error}");
    ExitCode::from(126)
}

/// Prints a status line in cargo's own convention — a bold green verb right-aligned to
/// 12 columns, then the message — so Mamba's own steps (syncing, downloading) read as
/// part of the same build log as cargo's "Compiling"/"Finished" lines instead of a
/// different tool bolted on. Colour is skipped when stderr isn't a terminal, the same
/// call real cargo makes for its own output.
fn status(verb: &str, message: &str) {
    if io::stderr().is_terminal() {
        eprintln!("\x1b[1m\x1b[92m{verb:>12}\x1b[0m {message}");
    } else {
        eprintln!("{verb:>12} {message}");
    }
}

/// The project directory's name, used only to label status lines — falls back to
/// "project" on the off chance the path has no final component.
fn project_name(config: &Config) -> &str {
    config
        .root
        .as_path()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
}

/// Renders a byte count the way a human reads it, e.g. `4.16 MB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}

/// Explains how to install the shim, shown when the binary is run as `mamba`.
fn print_usage() {
    eprintln!(
        "\
mamba builds your Rust project on another machine.

Install the shim, once:
    ln -s \"$(command -v mamba)\" ~/.local/bin/cargo
and make sure ~/.local/bin comes before ~/.cargo/bin on your PATH.

Then, in any project you want built remotely, create .mamba.toml:
    host = \"gpu-box\"            # any ssh destination or ~/.ssh/config alias
    # remote_dir = \".mamba/proj\"  # optional, relative to the remote home directory

From then on `cargo build` in that project compiles on gpu-box.
    cargo build --mamba-pull      fetch the built binary back
    cargo build --mamba-symbols   also fetch debug symbols for it
Every other cargo command runs locally as usual."
    );
}

/// Finds a `cargo` on `PATH` that is not this executable.
///
/// Mamba installs itself as a symlink named `cargo` placed ahead of the real one, so
/// searching `PATH` the ordinary way finds the shim and calls it forever. Comparing
/// canonical paths — which resolves the symlink back to the mamba binary — makes that
/// loop impossible rather than merely unlikely, and works no matter where cargo is
/// installed.
fn find_real_cargo(path_var: &OsStr, me: &Path) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join("cargo"))
        .filter(|candidate| candidate.is_file())
        .find(|candidate| match candidate.canonicalize() {
            Ok(resolved) => resolved != me,
            Err(_) => false,
        })
}

/// Reads the answer to the `[Y/n]` fallback prompt. Anything other than an explicit
/// no is a yes, including an empty line.
fn answer_means_yes(input: &str) -> bool {
    !matches!(input.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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
    fn skips_the_shim_and_finds_the_next_cargo_on_path() {
        let base = tmpdir("real-cargo");
        let shim_dir = base.join("shim");
        let real_dir = base.join("real");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();

        // The mamba binary, and a symlink to it named `cargo` — the shim install.
        let mamba = base.join("mamba");
        fs::write(&mamba, "binary").unwrap();
        std::os::unix::fs::symlink(&mamba, shim_dir.join("cargo")).unwrap();

        // A different, genuine cargo further along PATH.
        fs::write(real_dir.join("cargo"), "the real one").unwrap();

        let path_var = std::env::join_paths([&shim_dir, &real_dir]).unwrap();
        let me = mamba.canonicalize().unwrap();

        let found = find_real_cargo(&path_var, &me).unwrap();

        assert_eq!(
            found.canonicalize().unwrap(),
            real_dir.join("cargo").canonicalize().unwrap()
        );
    }

    #[test]
    fn returns_none_when_the_only_cargo_on_path_is_the_shim() {
        let base = tmpdir("only-shim");
        let shim_dir = base.join("shim");
        fs::create_dir_all(&shim_dir).unwrap();

        let mamba = base.join("mamba");
        fs::write(&mamba, "binary").unwrap();
        std::os::unix::fs::symlink(&mamba, shim_dir.join("cargo")).unwrap();

        let path_var = std::env::join_paths([&shim_dir]).unwrap();
        let me = mamba.canonicalize().unwrap();

        assert!(find_real_cargo(&path_var, &me).is_none());
    }

    #[test]
    fn ignores_path_entries_that_do_not_contain_a_cargo() {
        let base = tmpdir("sparse-path");
        let empty = base.join("empty");
        let real_dir = base.join("real");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(real_dir.join("cargo"), "the real one").unwrap();

        let path_var = std::env::join_paths([&empty, &real_dir]).unwrap();
        let me = base.join("does-not-exist");

        assert!(find_real_cargo(&path_var, &me).is_some());
    }

    #[test]
    fn empty_answer_means_yes_because_the_prompt_defaults_to_yes() {
        assert!(answer_means_yes(""));
        assert!(answer_means_yes("\n"));
        assert!(answer_means_yes("  \n"));
    }

    #[test]
    fn only_an_explicit_no_declines() {
        assert!(!answer_means_yes("n"));
        assert!(!answer_means_yes("N"));
        assert!(!answer_means_yes("no\n"));
        assert!(!answer_means_yes(" NO "));
    }

    #[test]
    fn anything_else_means_yes() {
        assert!(answer_means_yes("y"));
        assert!(answer_means_yes("yes"));
        assert!(answer_means_yes("sure"));
    }

    #[test]
    fn human_size_stays_in_bytes_under_a_kilobyte() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
    }

    #[test]
    fn human_size_picks_the_largest_unit_that_keeps_it_readable() {
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(human_size(4_362_824), "4.16 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.00 GB");
    }
}
