mod channel;
mod config;
mod daemon;
mod grpc;
mod ipc;
mod ssh;
mod transfer;

use config::Config;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();

    // The daemon runs from this same binary. It is spawned by the shim, never by a
    // person, so it stays out of the usage text.
    if argv.get(1).map(String::as_str) == Some("--daemon") {
        return daemon::run();
    }

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

    let request = ipc::Request {
        // Cargo's own flags, without the `build` subcommand the shim matched on.
        args: args[1..].to_vec(),
        root: config.root.as_path().display().to_string(),
        host: config.host.as_str().to_string(),
        project_id: config.project_id.clone(),
        pull: cli_pull || config.pull,
        symbols: cli_symbols || config.symbols,
    };

    let mut stream = match connect_or_spawn_daemon() {
        Ok(s) => s,
        Err(e) => return offer_local_build(&config, &args, &e),
    };

    if let Err(e) = ipc::write_request(&mut stream, &request) {
        return offer_local_build(&config, &args, &format!("could not send the request: {e}"));
    }

    match relay_frames(&mut stream) {
        Ok(Some(code)) => ExitCode::from(code.clamp(0, 255) as u8),
        Ok(None) => offer_local_build(&config, &args, "the daemon closed without answering"),
        Err(why) => offer_local_build(&config, &args, &why),
    }
}

/// Connects to the daemon, starting one if nothing answers.
///
/// The daemon is detached into its own process group so it outlives this shim and keeps
/// its connections warm for the next build. A socket file that exists but refuses a
/// connection is a leftover from a killed daemon; the new daemon clears it on bind.
fn connect_or_spawn_daemon() -> Result<std::os::unix::net::UnixStream, String> {
    use std::os::unix::net::UnixStream;

    let path = ipc::socket_path();
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }

    let me = std::env::current_exe().map_err(|e| format!("cannot find my own path: {e}"))?;
    Command::new(me)
        .arg("--daemon")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start the daemon: {e}"))?;

    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
    }
    Err("the daemon did not start within five seconds".to_string())
}

/// Writes the daemon's frames to this process's own stdio as they arrive.
///
/// Every frame is flushed immediately. The output has already crossed two hops to get
/// here, and buffering it a third time is what would make a live build feel batched.
///
/// Returns the build's exit code, or `None` if the stream ended without one.
fn relay_frames(stream: &mut std::os::unix::net::UnixStream) -> Result<Option<i32>, String> {
    let mut reader = stream
        .try_clone()
        .map_err(|e| format!("could not read from the daemon: {e}"))?;

    loop {
        match ipc::read_frame(&mut reader) {
            Err(e) => return Err(format!("the daemon connection broke: {e}")),
            Ok(None) => return Ok(None),
            Ok(Some(ipc::Frame::Stdout(bytes))) => {
                let mut out = io::stdout();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Ok(Some(ipc::Frame::Stderr(bytes))) => {
                let mut err = io::stderr();
                let _ = err.write_all(&bytes);
                let _ = err.flush();
            }
            Ok(Some(ipc::Frame::Status(verb, message))) => status(&verb, &message),
            Ok(Some(ipc::Frame::Failed(why))) => return Err(why),
            Ok(Some(ipc::Frame::Exit(code))) => return Ok(Some(code)),
        }
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

/// Explains how to install the shim, shown when the binary is run as `mamba`.
fn print_usage() {
    eprintln!(
        "\
Mamba(🐍) builds your Rust project on another machine.

Install the shim, once:
    ln -s \"$(command -v mamba)\" ~/.local/bin/cargo
and make sure ~/.local/bin comes before ~/.cargo/bin on your PATH.

Then, in any project you want built remotely, create .mamba.toml:
    host = \"gpu-box\"            # any ssh destination or ~/.ssh/config alias

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
    fn a_leftover_socket_file_does_not_masquerade_as_a_running_daemon() {
        // A killed daemon leaves its socket behind. Connecting must fail so the shim
        // starts a new one rather than hanging against a dead path.
        let dir = tmpdir("stale-socket");
        let socket = dir.join("daemon.sock");
        fs::write(&socket, "not a real socket").unwrap();

        assert!(std::os::unix::net::UnixStream::connect(&socket).is_err());
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
}
