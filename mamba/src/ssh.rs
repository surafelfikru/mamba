//! A control channel that needs no server.
//!
//! Everything here is answered by convention rather than by asking: a project lives at
//! `.mamba/<id>` in the remote home directory, and its artifacts sit where cargo puts
//! them. That is a legitimate implementation of the trait, not a violation of the rule
//! that the daemon must not model the far side — the daemon still only knows the trait,
//! and this transport is the one that has nobody to ask.
//!
//! This is the zero-setup path: an ordinary ssh host and nothing installed on it.

use crate::channel::{BuildStream, ChannelError, ControlChannel};
use async_trait::async_trait;
use mamba_core::layout;
use mamba_core::proto::artifact_request::Kind;
use mamba_core::proto::{build_event, BuildEvent, TransferTarget};
use std::path::Path;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// A string already escaped for a POSIX shell.
///
/// Cargo flags travel from local `argv` into a command line a remote shell parses, which
/// is exactly where an unescaped argument becomes command injection. The only constructor
/// is [`Quoted::new`], and the remote command is assembled purely from `Quoted` values, so
/// forgetting to escape something is a compile error rather than a security hole.
///
/// ```
/// let flag = Quoted::new("--features a b");
/// assert_eq!(flag.as_str(), "'--features a b'");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quoted(String);

impl Quoted {
    /// Wraps the argument in single quotes, which suppress every shell expansion. A
    /// literal single quote cannot appear inside single quotes, so each one is closed,
    /// escaped, and reopened — `it's` becomes `'it'\''s'`.
    pub fn new(raw: &str) -> Quoted {
        Quoted(format!("'{}'", raw.replace('\'', r"'\''")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Assembles the one-line shell command ssh runs on the far side.
///
/// `CARGO_TERM_COLOR=always` is needed because cargo turns colour off when its output is
/// not a terminal, and over ssh it never is. The leading `. "$HOME/.cargo/env"` matters
/// just as much: `ssh host command` runs a non-interactive shell, and rustup wires
/// `~/.cargo/bin` onto `PATH` only from interactive rc files — without it, a cargo a human
/// can run fine over an interactive login is invisible here.
///
/// `$PWD` is expanded by the remote shell after the `cd`, so the path rewrite needs no
/// knowledge of the remote home directory.
fn build_command(remote_dir: &str, local_root: &str, args: &[String]) -> String {
    let remap = format!(
        "RUSTFLAGS=\"--remap-path-prefix=$PWD=\"{}",
        Quoted::new(local_root).as_str()
    );

    let mut command = format!(
        ". \"$HOME/.cargo/env\" 2>/dev/null; cd {} && {remap} CARGO_TERM_COLOR=always cargo build",
        Quoted::new(remote_dir).as_str()
    );
    for arg in args {
        command.push(' ');
        command.push_str(Quoted::new(arg).as_str());
    }
    command
}

/// Talks to an ordinary ssh host, with no Mamba software installed on it.
pub struct SshControlChannel {
    host: String,
    /// Each project's most recent build — its arguments and the client's project root —
    /// so a later artifact request resolves against the profile that build used. Keyed by
    /// project because one channel instance is shared by every project building against
    /// this host, concurrently; a single slot here let interleaved builds overwrite each
    /// other's memory. The gRPC server keeps per-project args for the same reason, though
    /// not the root — the server reads Cargo.toml from its own disk, this channel reads
    /// the client's.
    last: Mutex<HashMap<String, (Vec<String>, String)>>,
}

impl SshControlChannel {
    pub fn new(host: &str) -> SshControlChannel {
        SshControlChannel {
            host: host.to_string(),
            last: Mutex::new(HashMap::new()),
        }
    }

    /// Records what a project's most recent build was, for artifact resolution afterwards.
    pub fn remember(&self, project_id: &str, args: &[String], local_root: &str) {
        if let Ok(mut last) = self.last.lock() {
            last.insert(
                project_id.to_string(),
                (args.to_vec(), local_root.to_string()),
            );
        }
    }

    fn last_args(&self, project_id: &str) -> Vec<String> {
        self.last
            .lock()
            .ok()
            .and_then(|l| l.get(project_id).map(|(args, _)| args.clone()))
            .unwrap_or_default()
    }

    fn last_root(&self, project_id: &str) -> String {
        self.last
            .lock()
            .ok()
            .and_then(|l| l.get(project_id).map(|(_, root)| root.clone()))
            .unwrap_or_default()
    }

    /// Where a project lives on the far side. Relative paths are taken from the remote
    /// home directory, which is how this resolves without ever needing a tilde.
    fn remote_dir(&self, project_id: &str) -> String {
        format!(".mamba/{project_id}")
    }

    fn target(&self, path: String, relative_path: String) -> TransferTarget {
        TransferTarget {
            host: self.host.clone(),
            port: 22,
            // Left empty on purpose: `host` may be an alias from ~/.ssh/config, which
            // already carries the user. Naming one here would override it.
            user: String::new(),
            path,
            relative_path,
        }
    }
}

#[async_trait]
impl ControlChannel for SshControlChannel {
    async fn request_upload(&self, project_id: &str) -> Result<TransferTarget, ChannelError> {
        Ok(self.target(self.remote_dir(project_id), String::new()))
    }

    async fn start_build(
        &self,
        project_id: &str,
        args: &[String],
        local_root: &str,
    ) -> Result<BuildStream, ChannelError> {
        self.remember(project_id, args, local_root);

        let command = build_command(&self.remote_dir(project_id), local_root, args);
        let mut child = tokio::process::Command::new("ssh")
            .arg(&self.host)
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ChannelError(format!("could not run ssh: {e}")))?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            let out_tx = tx.clone();
            let out = tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let payload = build_event::Payload::Stdout(format!("{line}\n").into_bytes());
                    if out_tx
                        .send(Ok(BuildEvent {
                            payload: Some(payload),
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            let err_tx = tx.clone();
            let err = tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let payload = build_event::Payload::Stderr(format!("{line}\n").into_bytes());
                    if err_tx
                        .send(Ok(BuildEvent {
                            payload: Some(payload),
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            let _ = tokio::join!(out, err);

            // 255 is ssh's own connection failure. Reporting it as an error rather than an
            // exit code is what lets the shim offer a local build instead of pretending
            // cargo said something.
            match child.wait().await {
                Ok(status) if status.code() == Some(255) => {
                    let _ = tx
                        .send(Err(ChannelError("ssh could not connect".to_string())))
                        .await;
                }
                Ok(status) => {
                    let code = status.code().unwrap_or(130);
                    let _ = tx
                        .send(Ok(BuildEvent {
                            payload: Some(build_event::Payload::ExitCode(code)),
                        }))
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(Err(ChannelError(format!("ssh failed: {e}")))).await;
                }
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn request_artifact(
        &self,
        project_id: &str,
        kind: Kind,
    ) -> Result<TransferTarget, ChannelError> {
        let dir = self.remote_dir(project_id);
        let args = self.last_args(project_id);
        let subdir = layout::target_subdir(&args);

        let (remote_rel, local_rel) = match kind {
            Kind::ProcMacros => {
                let p = format!("target/{subdir}/deps");
                (p.clone(), p)
            }
            Kind::GeneratedSource => {
                let p = format!("target/{subdir}/build");
                (p.clone(), p)
            }
            Kind::Binary => {
                let root = self.last_root(project_id);
                let name = layout::binary_name(Path::new(&root)).map_err(ChannelError)?;

                // The split runs where the binary is, using CPU already paid for.
                let script = format!(
                    "cd {} && {}",
                    Quoted::new(&dir).as_str(),
                    layout::split_script(&subdir, &name)
                );
                let status = tokio::process::Command::new("ssh")
                    .arg(&self.host)
                    .arg(&script)
                    .status()
                    .await
                    .map_err(|e| ChannelError(format!("could not run the split: {e}")))?;
                if !status.success() {
                    return Err(ChannelError(format!(
                        "splitting debug info failed with {status}"
                    )));
                }

                (
                    format!("target/{subdir}/{name}.slim"),
                    format!("target/{subdir}/{name}"),
                )
            }
            Kind::Symbols => {
                let root = self.last_root(project_id);
                let name = layout::binary_name(Path::new(&root)).map_err(ChannelError)?;
                let p = format!("target/{subdir}/{name}.debug");
                (p.clone(), p)
            }
        };

        Ok(self.target(format!("{dir}/{remote_rel}"), local_rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Runs `echo` in a real shell with `arg` already quoted, and returns what the shell
    /// actually produced. Comparing against a hand-written expected string would only
    /// test our own opinion of the escaping.
    fn round_trip(arg: &str) -> String {
        let script = format!("printf '%s' {}", Quoted::new(arg).as_str());
        let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert!(out.status.success(), "shell rejected: {script}");
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn quoting_survives_spaces_quotes_and_metacharacters() {
        for raw in [
            "plain",
            "--features a b",
            "it's",
            "$(id)",
            "a`b`",
            "x;rm -rf /",
            "",
        ] {
            assert_eq!(round_trip(raw), raw, "quoting changed {raw:?}");
        }
    }

    #[tokio::test]
    async fn an_upload_target_points_at_the_configured_host_by_convention() {
        let ssh = SshControlChannel::new("gpu-box");

        let t = ssh.request_upload("myproj").await.unwrap();

        assert_eq!(t.host, "gpu-box");
        assert_eq!(t.path, ".mamba/myproj");
        assert!(t.user.is_empty(), "the ssh config decides the user, not us");
    }

    #[tokio::test]
    async fn artifact_paths_follow_the_profile_of_the_last_build() {
        let ssh = SshControlChannel::new("gpu-box");
        ssh.remember("myproj", &["--release".to_string()], "/home/dev/proj");

        let t = ssh
            .request_artifact("myproj", Kind::ProcMacros)
            .await
            .unwrap();

        assert_eq!(t.relative_path, "target/release/deps");
        assert_eq!(t.path, ".mamba/myproj/target/release/deps");
    }

    #[tokio::test]
    async fn two_projects_on_one_channel_keep_their_own_build_memory() {
        // The daemon caches one channel per host and serves builds concurrently, so two
        // projects interleave on the same channel instance. Project B building must not
        // make project A's artifact request resolve with B's profile.
        let ssh = SshControlChannel::new("gpu-box");
        ssh.remember("proj-a", &["--release".to_string()], "/home/dev/a");
        ssh.remember("proj-b", &[], "/home/dev/b");

        let a = ssh.request_artifact("proj-a", Kind::ProcMacros).await.unwrap();
        let b = ssh.request_artifact("proj-b", Kind::ProcMacros).await.unwrap();

        assert_eq!(a.relative_path, "target/release/deps", "A built --release");
        assert_eq!(b.relative_path, "target/debug/deps", "B built plain debug");
    }

    #[test]
    fn the_build_command_sources_cargo_env_and_forces_colour() {
        let command = build_command(".mamba/proj", "/home/dev/proj", &["--release".to_string()]);

        assert!(
            command.contains(".cargo/env"),
            "a non-interactive shell has no cargo on PATH"
        );
        assert!(command.contains("CARGO_TERM_COLOR=always"));
        assert!(command.contains("--remap-path-prefix=$PWD="));
        assert!(
            command.contains("'--release'"),
            "flags must reach cargo quoted: {command}"
        );
    }
}
