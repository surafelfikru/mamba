//! The three questions `mamba-server` answers.
//!
//! Every path a client sees is produced here. A request names a project by an opaque id
//! and an artifact by kind; this module decides where those live and returns both an
//! absolute path for rsync to read and a project-relative one for the client to write to.

use mamba_core::layout;
use mamba_core::proto::artifact_request::Kind;
use mamba_core::proto::TransferTarget;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

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
    fn an_upload_target_creates_the_directory_so_a_first_build_needs_no_setup() {
        let root = tmpdir("svc-upload");
        let t = service(&root).upload_target("fresh").unwrap();

        assert!(root.join("fresh").is_dir());
        assert_eq!(t.path, root.join("fresh").display().to_string());
    }
}
