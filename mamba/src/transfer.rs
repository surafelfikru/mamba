//! Bulk file movement, driven entirely by targets the channel supplies.
//!
//! rsync keeps this job because its delta transfer and compression are why a rebuilt
//! binary costs a fraction of its size to move. What changed is where the far side comes
//! from: every host, user, and path here arrives in a [`TransferTarget`] and is used as
//! given, so a server can point at a different machine without the client knowing.
//!
//! All transfers share one multiplexed ssh connection, turning a per-command handshake
//! into a one-time cost.

use mamba_core::proto::TransferTarget;
use mamba_core::proto::artifact_request::Kind;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `-e` argument that makes rsync reuse one ssh connection for every transfer, and
/// honours the target's port when it names a nonstandard one.
///
/// `ControlPath` is a unix socket path, and `sockaddr_un` caps those at 108 bytes — a
/// descriptive path silently breaks multiplexing, so `%C` (a 40-character hash of the
/// destination) under a short directory is used instead.
///
/// Port 22 (and 0, the proto's unset default) is deliberately left off the command line:
/// it is what ssh does anyway, and forcing `-p 22` would clobber a port set for this host
/// in `~/.ssh/config`.
pub fn ssh_control_args(port: u32) -> Vec<OsString> {
    let control_path = crate::input::mamba_home().join("cm-%C");
    let control_path = control_path.display();

    let mut ssh =
        format!("ssh -o ControlMaster=auto -o ControlPath={control_path} -o ControlPersist=600");
    if port != 0 && port != 22 {
        ssh.push_str(&format!(" -p {port}"));
    }

    vec![OsString::from("-e"), OsString::from(ssh)]
}

/// Makes sure the directory holding multiplexing sockets exists.
fn ensure_control_dir() {
    let _ = std::fs::create_dir_all(crate::input::mamba_home());
}

/// Renders the remote side of an rsync command line. An empty user means the ssh config
/// already knows who to log in as.
fn remote_spec(target: &TransferTarget) -> String {
    if target.user.is_empty() {
        format!("{}:{}", target.host, target.path)
    } else {
        format!("{}@{}:{}", target.user, target.host, target.path)
    }
}

/// The rsync arguments for pushing a project's source to wherever the channel said.
///
/// `:- .gitignore` decides what is uploaded; `P /target/` tells the receiver never to
/// delete that path, which is what keeps the remote build cache alive across runs. The two
/// do different jobs — ignore rules apply only to the sender, so without the protect rule
/// `--delete` wipes the cache on every run.
fn push_args(root: &Path, target: &TransferTarget) -> Vec<OsString> {
    let mut source = root.as_os_str().to_os_string();
    source.push("/");

    let mut args = vec![
        OsString::from("-az"),
        OsString::from("--delete"),
        OsString::from("--filter=:- .gitignore"),
        OsString::from("--filter=P /target/"),
        OsString::from("--exclude=.git/"),
        OsString::from(format!("--rsync-path=mkdir -p '{}' && rsync", target.path)),
    ];
    args.extend(ssh_control_args(target.port));
    args.push(source);
    args.push(OsString::from(format!("{}/", remote_spec(target))));
    args
}

/// The rsync arguments for fetching one artifact, plus where it lands locally.
///
/// The destination is the target's `relative_path` joined to this project's root, which is
/// how the binary can travel as a stripped `.slim` file yet land under the plain name a
/// local build would have written.
fn fetch_args(root: &Path, target: &TransferTarget, kind: Kind) -> (Vec<OsString>, PathBuf) {
    let local = root.join(&target.relative_path);
    let mut args = vec![OsString::from("-az")];

    match kind {
        // deps/ is mostly rlibs that are statically linked into the binary and never
        // needed here. The include must precede the catch-all exclude — rsync takes the
        // first rule that matches, and reversing them copies the whole directory.
        Kind::ProcMacros => {
            args.push(OsString::from("--include=*.so"));
            args.push(OsString::from("--exclude=*"));
        }
        // Descend into every directory, take the .rs files, drop the rest. Without the
        // first rule rsync never enters the nested out/ directories and copies nothing.
        Kind::GeneratedSource => {
            args.push(OsString::from("--include=*/"));
            args.push(OsString::from("--include=*.rs"));
            args.push(OsString::from("--exclude=*"));
            args.push(OsString::from("--prune-empty-dirs"));
        }
        Kind::Binary | Kind::Symbols => {}
    }

    args.extend(ssh_control_args(target.port));

    let directory = matches!(kind, Kind::ProcMacros | Kind::GeneratedSource);
    let remote = if directory {
        format!("{}/", remote_spec(target))
    } else {
        remote_spec(target)
    };

    args.push(OsString::from(remote));
    args.push(local.clone().into_os_string());
    (args, local)
}

/// Runs rsync and turns its exit status into a message.
fn run(args: &[OsString]) -> Result<(), String> {
    ensure_control_dir();
    match Command::new("rsync").args(args).status() {
        Err(e) => Err(format!("could not run rsync: {e}")),
        Ok(status) if status.success() => Ok(()),
        Ok(status) => match status.code() {
            Some(code) => Err(format!("rsync exited with {code}")),
            None => Err("rsync was killed by a signal".to_string()),
        },
    }
}

/// Pushes this project's source to the destination the channel chose.
pub fn push(root: &Path, target: &TransferTarget) -> Result<(), String> {
    run(&push_args(root, target))
}

/// Fetches one artifact into this project, returning where it landed.
pub fn fetch(root: &Path, target: &TransferTarget, kind: Kind) -> Result<PathBuf, String> {
    let (args, local) = fetch_args(root, target, kind);

    let parent = if matches!(kind, Kind::ProcMacros | Kind::GeneratedSource) {
        Some(local.as_path())
    } else {
        local.parent()
    };
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }

    run(&args)?;
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(host: &str, path: &str, rel: &str) -> TransferTarget {
        TransferTarget {
            host: host.to_string(),
            port: 22,
            user: String::new(),
            path: path.to_string(),
            relative_path: rel.to_string(),
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_control_path_stays_under_the_sockaddr_un_limit() {
        let joined = strings(&ssh_control_args(22)).join(" ");
        let path = joined
            .split("ControlPath=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .expect("a ControlPath must be set");

        // %C expands to a 40-character hash; sockaddr_un caps the whole path at 108.
        assert!(
            path.replace("%C", &"x".repeat(40)).len() < 108,
            "ControlPath would overflow sockaddr_un: {path}"
        );
    }

    #[test]
    fn a_fetch_reads_the_targets_path_and_writes_the_relative_one() {
        let root = std::path::Path::new("/home/dev/proj");
        let t = target(
            "worker-7",
            ".mamba/proj/target/debug/app.slim",
            "target/debug/app",
        );

        let (args, local) = fetch_args(root, &t, Kind::Binary);

        assert!(
            strings(&args)
                .iter()
                .any(|a| a == "worker-7:.mamba/proj/target/debug/app.slim"),
            "the remote side must come verbatim from the target: {:?}",
            strings(&args)
        );
        assert_eq!(local, root.join("target/debug/app"));
    }

    #[test]
    fn a_target_on_a_different_host_than_the_control_endpoint_is_obeyed() {
        // The property that keeps the client topology-agnostic: whatever the far side
        // says, the client uses. It must never substitute the host it was configured with.
        let root = std::path::Path::new("/home/dev/proj");
        let t = target(
            "some-other-worker",
            "/elsewhere/app.slim",
            "target/debug/app",
        );

        let (args, _) = fetch_args(root, &t, Kind::Binary);

        assert!(
            strings(&args)
                .iter()
                .any(|a| a.contains("some-other-worker"))
        );
    }

    #[test]
    fn a_nonstandard_port_reaches_the_ssh_command_line() {
        // The contract the other tests advertise — the client obeys the target exactly —
        // has to include the port, or a server advertising one is silently ignored.
        let root = std::path::Path::new("/home/dev/proj");
        let mut t = target("h", "/p", "target/debug/app");
        t.port = 2222;

        let (args, _) = fetch_args(root, &t, Kind::Binary);

        assert!(
            strings(&args).iter().any(|a| a.contains("-p 2222")),
            "port 2222 must appear in the -e ssh line: {:?}",
            strings(&args)
        );
    }

    #[test]
    fn the_default_port_stays_off_the_command_line() {
        // Port 22 is what ssh does anyway, and omitting it keeps ~/.ssh/config free to
        // override the port for aliases — an explicit -p 22 would clobber that.
        let root = std::path::Path::new("/home/dev/proj");
        let t = target("h", "/p", "target/debug/app");

        let (args, _) = fetch_args(root, &t, Kind::Binary);

        assert!(
            !strings(&args).iter().any(|a| a.contains("-p ")),
            "the default port must not be forced: {:?}",
            strings(&args)
        );
    }

    #[test]
    fn a_user_is_included_only_when_the_target_names_one() {
        let root = std::path::Path::new("/home/dev/proj");

        let (bare, _) = fetch_args(root, &target("h", "/p", "target/debug/app"), Kind::Binary);
        assert!(strings(&bare).iter().any(|a| a == "h:/p"));

        let mut with_user = target("h", "/p", "target/debug/app");
        with_user.user = "ubuntu".to_string();
        let (named, _) = fetch_args(root, &with_user, Kind::Binary);
        assert!(strings(&named).iter().any(|a| a == "ubuntu@h:/p"));
    }

    #[test]
    fn proc_macros_take_only_dylibs_and_generated_source_only_rust_files() {
        let root = std::path::Path::new("/home/dev/proj");

        let (macros, _) = fetch_args(
            root,
            &target("h", "/d", "target/debug/deps"),
            Kind::ProcMacros,
        );
        let macros = strings(&macros);
        let include = macros.iter().position(|a| a == "--include=*.so").unwrap();
        let exclude = macros.iter().position(|a| a == "--exclude=*").unwrap();
        assert!(
            include < exclude,
            "the include must come first or rsync copies everything"
        );

        let (generated, _) = fetch_args(
            root,
            &target("h", "/b", "target/debug/build"),
            Kind::GeneratedSource,
        );
        let generated = strings(&generated);
        assert!(generated.contains(&"--include=*/".to_string()));
        assert!(generated.contains(&"--include=*.rs".to_string()));
        assert!(generated.contains(&"--prune-empty-dirs".to_string()));
    }

    #[test]
    fn a_push_protects_the_receivers_target_directory() {
        let args = strings(&push_args(
            std::path::Path::new("/home/dev/proj"),
            &target("h", ".mamba/proj", ""),
        ));

        assert!(args.contains(&"--filter=P /target/".to_string()));
        assert!(args.contains(&"--filter=:- .gitignore".to_string()));
        assert!(args.contains(&"--delete".to_string()));
        assert!(
            args.iter().any(|a| a.starts_with("--rsync-path=mkdir -p")),
            "a project's first push must create its directory: {args:?}"
        );
    }
}
