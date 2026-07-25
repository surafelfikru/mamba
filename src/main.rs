mod config;
mod remote;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn main() {
    println!("Hello, world!");
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
}
