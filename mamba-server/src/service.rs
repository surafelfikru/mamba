//! The three questions `mamba-server` answers.
//!
//! Every path a client sees is produced here. A request names a project by an opaque id
//! and an artifact by kind; this module decides where those live and returns both an
//! absolute path for rsync to read and a project-relative one for the client to write to.

use mamba_core::layout;
use mamba_core::proto::artifact_request::Kind;
use mamba_core::proto::control_server::Control;
use mamba_core::proto::{
    build_event, ArtifactRequest, BuildEvent, BuildRequest, TransferTarget, UploadRequest,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

/// Serves one directory of projects, advertising itself under a fixed host and user.
///
/// The advertised pair goes into every [`TransferTarget`], which is what lets a future
/// server point a client at a different machine without the client changing.
pub struct ControlService {
    root: PathBuf,
    advertise_host: String,
    advertise_user: String,
    /// Each project's most recent build arguments, so a later artifact request resolves
    /// against the same profile the build actually used.
    last_build_args: Mutex<HashMap<String, Vec<String>>>,
}

impl ControlService {
    pub fn new(root: PathBuf, advertise_host: String, advertise_user: String) -> ControlService {
        ControlService {
            root,
            advertise_host,
            advertise_user,
            last_build_args: Mutex::new(HashMap::new()),
        }
    }

    /// Maps a project id to its directory, refusing anything that would climb out of the
    /// server's root. The id arrives over the network, so this is a trust boundary.
    pub fn project_dir(&self, project_id: &str) -> Result<PathBuf, String> {
        let plain = !project_id.is_empty()
            && project_id != "."
            && project_id != ".."
            && project_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));

        if !plain {
            return Err(format!("project id {project_id:?} is not a plain name"));
        }
        Ok(self.root.join(project_id))
    }

    pub(crate) fn remember_args(&self, project_id: &str, args: &[String]) {
        if let Ok(mut map) = self.last_build_args.lock() {
            map.insert(project_id.to_string(), args.to_vec());
        }
    }

    pub(crate) fn last_args(&self, project_id: &str) -> Vec<String> {
        self.last_build_args
            .lock()
            .ok()
            .and_then(|m| m.get(project_id).cloned())
            .unwrap_or_default()
    }

    /// Where a client should push this project's source. Created on demand so a project's
    /// first build needs no setup.
    pub fn upload_target(&self, project_id: &str) -> Result<TransferTarget, String> {
        let dir = self.project_dir(project_id)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

        Ok(TransferTarget {
            host: self.advertise_host.clone(),
            port: 22,
            user: self.advertise_user.clone(),
            path: dir.display().to_string(),
            relative_path: String::new(),
        })
    }

    /// Builds a transfer target for one artifact kind.
    ///
    /// The remote and local halves differ for the binary: it travels as the stripped
    /// `.slim` file but lands under the plain name a local build would have produced.
    pub fn resolve_artifact(
        &self,
        project_id: &str,
        kind: Kind,
        args: &[String],
    ) -> Result<TransferTarget, String> {
        let dir = self.project_dir(project_id)?;
        let subdir = layout::target_subdir(args);

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
                let name = layout::binary_name(&dir)?;
                run_split(&dir, &subdir, &name)?;
                (
                    format!("target/{subdir}/{name}.slim"),
                    format!("target/{subdir}/{name}"),
                )
            }
            Kind::Symbols => {
                let name = layout::binary_name(&dir)?;
                let p = format!("target/{subdir}/{name}.debug");
                (p.clone(), p)
            }
        };

        Ok(TransferTarget {
            host: self.advertise_host.clone(),
            port: 22,
            user: self.advertise_user.clone(),
            path: dir.join(&remote_rel).display().to_string(),
            relative_path: local_rel,
        })
    }
}

#[tonic::async_trait]
impl Control for ControlService {
    async fn request_upload(
        &self,
        request: Request<UploadRequest>,
    ) -> Result<Response<TransferTarget>, Status> {
        self.upload_target(&request.into_inner().project_id)
            .map(Response::new)
            .map_err(Status::invalid_argument)
    }

    async fn request_artifact(
        &self,
        request: Request<ArtifactRequest>,
    ) -> Result<Response<TransferTarget>, Status> {
        let req = request.into_inner();
        let kind = Kind::try_from(req.kind)
            .map_err(|_| Status::invalid_argument(format!("unknown artifact kind {}", req.kind)))?;

        // The profile is remembered from the build that produced the artifact rather than
        // resent, so a client cannot ask for one profile's binary after building another.
        let args = self.last_args(&req.project_id);

        self.resolve_artifact(&req.project_id, kind, &args)
            .map(Response::new)
            .map_err(Status::failed_precondition)
    }

    type StartBuildStream = ReceiverStream<Result<BuildEvent, Status>>;

    /// Runs cargo and pushes its output back a line at a time, ending with the exit code.
    ///
    /// `--remap-path-prefix` rewrites the compiler's record of where it ran to the
    /// client's own project path, so a debugger on the far end finds source files instead
    /// of paths that exist only here.
    async fn start_build(
        &self,
        request: Request<BuildRequest>,
    ) -> Result<Response<Self::StartBuildStream>, Status> {
        let req = request.into_inner();
        let dir = self
            .project_dir(&req.project_id)
            .map_err(Status::invalid_argument)?;

        self.remember_args(&req.project_id, &req.args);

        let mut child = tokio::process::Command::new("cargo")
            .arg("build")
            .args(&req.args)
            .current_dir(&dir)
            .env("PATH", cargo_path())
            .env("CARGO_TERM_COLOR", "always")
            .env(
                "RUSTFLAGS",
                format!("--remap-path-prefix={}={}", dir.display(), req.local_root),
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Status::internal(format!("could not start cargo: {e}")))?;

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

            let code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(130),
                Err(_) => 1,
            };
            let _ = tx
                .send(Ok(BuildEvent {
                    payload: Some(build_event::Payload::ExitCode(code)),
                }))
                .await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// `PATH` with rustup's directory on the front, so cargo is findable however the server
/// was started.
///
/// A server launched from a login shell inherits a usable `PATH`, but one started
/// detached over ssh or by a service manager does not — rustup wires `~/.cargo/bin` in
/// from interactive rc files only. Without this the build fails with a bare "No such file
/// or directory" that says nothing about which file. The ssh transport solves the same
/// problem by sourcing `~/.cargo/env`; this is that fix on the other side.
fn cargo_path() -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    match std::env::var_os("HOME") {
        Some(home) => {
            let bin = PathBuf::from(home).join(".cargo/bin");
            format!("{}:{inherited}", bin.display())
        }
        None => inherited,
    }
}

/// Runs the debug-info split in a project directory.
fn run_split(dir: &Path, subdir: &str, name: &str) -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(layout::split_script(subdir, name))
        .current_dir(dir)
        .status()
        .map_err(|e| format!("could not run the split: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("splitting debug info failed with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mamba-{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(root: &Path) -> ControlService {
        ControlService::new(
            root.to_path_buf(),
            "gpu-box".to_string(),
            "ubuntu".to_string(),
        )
    }

    #[test]
    fn a_project_id_maps_under_the_server_root() {
        let root = tmpdir("svc-root");
        assert_eq!(service(&root).project_dir("proj").unwrap(), root.join("proj"));
    }

    #[test]
    fn a_project_id_cannot_escape_the_server_root() {
        let root = tmpdir("svc-escape");
        let svc = service(&root);

        for bad in ["../etc", "a/../../b", "/etc", "", ".", ".."] {
            assert!(svc.project_dir(bad).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn proc_macros_resolve_to_the_deps_directory() {
        let root = tmpdir("svc-macros");
        let t = service(&root)
            .resolve_artifact("proj", Kind::ProcMacros, &["--release".to_string()])
            .unwrap();

        assert_eq!(t.relative_path, "target/release/deps");
        assert_eq!(
            t.path,
            root.join("proj/target/release/deps").display().to_string()
        );
        assert_eq!(t.host, "gpu-box");
        assert_eq!(t.user, "ubuntu");
    }

    #[test]
    fn generated_source_resolves_to_the_build_directory() {
        let root = tmpdir("svc-gen");
        let t = service(&root)
            .resolve_artifact("proj", Kind::GeneratedSource, &[])
            .unwrap();

        assert_eq!(t.relative_path, "target/debug/build");
    }

    #[test]
    fn the_binary_travels_stripped_but_lands_under_its_plain_name() {
        let root = tmpdir("svc-bin");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("target/debug")).unwrap();
        fs::write(proj.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        fs::write(proj.join("target/debug/app"), "ELF").unwrap();

        let t = service(&root)
            .resolve_artifact("proj", Kind::Binary, &[])
            .unwrap();

        assert!(t.path.ends_with("target/debug/app.slim"), "got {}", t.path);
        assert_eq!(t.relative_path, "target/debug/app");
    }

    #[test]
    fn symbols_resolve_to_the_debug_file_beside_the_binary() {
        let root = tmpdir("svc-sym");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("target/debug")).unwrap();
        fs::write(proj.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();

        let t = service(&root)
            .resolve_artifact("proj", Kind::Symbols, &[])
            .unwrap();

        assert!(t.path.ends_with("target/debug/app.debug"), "got {}", t.path);
        assert_eq!(t.relative_path, "target/debug/app.debug");
    }

    #[test]
    fn the_build_path_puts_rustups_directory_ahead_of_whatever_was_inherited() {
        let path = cargo_path();

        assert!(
            path.starts_with(&format!(
                "{}/.cargo/bin:",
                std::env::var("HOME").unwrap()
            )),
            "a server started detached has no cargo on PATH: {path}"
        );
    }

    #[test]
    fn an_upload_target_creates_the_directory_so_a_first_build_needs_no_setup() {
        let root = tmpdir("svc-upload");
        let t = service(&root).upload_target("fresh").unwrap();

        assert!(root.join("fresh").is_dir());
        assert_eq!(t.path, root.join("fresh").display().to_string());
    }

    #[tokio::test]
    async fn a_failing_build_streams_diagnostics_and_reports_cargos_own_exit_code() {
        use mamba_core::proto::build_event::Payload;
        use mamba_core::proto::control_server::Control;
        use tokio_stream::StreamExt;

        let root = tmpdir("svc-build");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("src")).unwrap();
        fs::write(
            proj.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(proj.join("src/main.rs"), "fn main() { this is not rust }").unwrap();

        let svc = service(&root);
        let stream = svc
            .start_build(tonic::Request::new(mamba_core::proto::BuildRequest {
                project_id: "proj".to_string(),
                args: vec![],
                local_root: proj.display().to_string(),
            }))
            .await
            .unwrap();

        let mut stream = stream.into_inner();
        let mut saw_output = false;
        let mut exit = None;
        while let Some(event) = stream.next().await {
            match event.unwrap().payload {
                Some(Payload::Stderr(b)) if !b.is_empty() => saw_output = true,
                Some(Payload::ExitCode(c)) => exit = Some(c),
                _ => {}
            }
        }

        assert!(saw_output, "a failing build must report diagnostics");
        assert_eq!(exit, Some(101), "cargo reports a compile error as 101");
    }
}
