// Sources from git, pinned to a commit: `git(url, commit)` of the scripts. The objects and the checkouts of a
// repository are shared by all projects in the cache. What modules in a checkout build stays in the project
// that uses it, in build/git/<repository>/<commit>/.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use super::system;

/// File in a checkout with its url and commit
const MARKER: &str = ".build-script-git";

/// The checkouts the script uses, with where their modules build in the project
static CHECKOUTS: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

/// <cache>/git/checkouts/<repository>/<commit>. `reference` is a commit, better its whole hash, or a tag.
/// Checked out once on the machine, the next builds take it as it is.
pub fn checkout(url: &str, reference: &str, project_dir: &Path) -> Result<PathBuf, String> {
    let (repository, version) = checkout_names(url, reference);
    let cache = system::cache_dir("git");

    let target = cache.join("checkouts")
        .join(&repository)
        .join(&version);

    let checkout = checkout_to(url, reference, &target, &cache.join(format!("{repository}.git")))?;

    let outputs = system::build_dir(project_dir)
        .join("git")
        .join(&repository)
        .join(&version);

    CHECKOUTS.lock()
        .unwrap()
        .push((checkout.clone(), outputs));

    return Ok(checkout);
}

/// The repository, https://github.com/llvm/llvm-project becomes github.com/llvm/llvm-project, and the
/// directory of the reference: a commit by its first 12 characters, a tag or a branch whole,
/// llvmorg-19.1.6 and llvmorg-19.1.7 differ only at their end
fn checkout_names(url: &str, reference: &str) -> (String, String) {
    let url_path = url.trim_end_matches('/')
        .trim_end_matches(".git");

    let repository = url_path.split_once("://")
        .map_or(url_path, |(_, path)| path)
        .trim_start_matches('/');

    let is_hash = reference.len() == 40 && reference.chars().all(|char| char.is_ascii_hexdigit());
    let version = match is_hash {
        true => reference[..12].to_string(),
        false => reference.replace('/', "-"),
    };

    return (repository.to_string(), version);
}

/// Where a module in a checkout of the script builds: below build/git/ of the project
pub fn project_build_dir(module_dir: &Path) -> Option<PathBuf> {
    let checkouts = CHECKOUTS.lock().unwrap();
    let (checkout, outputs) = checkouts.iter()
        .find(|(checkout, _)| module_dir.starts_with(checkout))?;

    return Some(
        outputs.join(module_dir.strip_prefix(checkout).ok()?)
            .join("build")
    );
}

/// Whether git tracks files in `directory`, false outside of a repository
pub fn tracks_files(directory: &Path) -> bool {
    let mut command = Command::new("git");
    command.arg("-C")
        .arg(directory)
        .args(["ls-files", "."]);

    return system::command_output(&mut command, "git ls-files").is_ok_and(|output| !output.is_empty());
}

/// Removes what git does not track in `directory`, what it ignores too
pub fn remove_untracked(directory: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command.arg("-C")
        .arg(directory)
        .args(["clean", "-d", "-x", "--force", "--quiet", "."]);

    return system::run_command_quietly(&mut command, "git clean");
}

/// Into `target`, with the objects of the repository in `objects`
fn checkout_to(url: &str, reference: &str, target: &Path, objects: &Path) -> Result<PathBuf, String> {
    let pinned = format!("{url} {reference}");

    if fs::read_to_string(target.join(MARKER)).is_ok_and(|found| found == pinned) {
        return Ok(target.to_path_buf());
    }

    return system::installing(objects, || {
        let in_objects = |args: &[&str]| {
            let mut command = Command::new("git");
            command.arg("--git-dir")
                .arg(objects)
                .args(args);
            return command;
        };

        if !objects.join("HEAD").is_file() {
            system::create_dir(objects)?;
            system::run_command_quietly(Command::new("git").args(["init", "--bare", "--quiet"]).arg(objects), "git init")?;
        }

        // a commit the cache has needs no fetch, a tag may have moved
        let is_hash = reference.len() == 40 && reference.chars().all(|char| char.is_ascii_hexdigit());
        let commit_object = format!("{reference}^{{commit}}");
        let cached = is_hash && system::command_succeeds(&mut in_objects(&["cat-file", "-e", &commit_object]));

        let commit = match cached {
            true => reference.to_string(),
            false => {
                // only this commit, not the history before it
                system::log_tool("git", &format!("Fetching {reference} of {url}"));
                system::run_command_quietly(&mut in_objects(&["fetch", "--quiet", "--depth", "1", url, reference]), "git fetch")?;

                let output = system::command_output(&mut in_objects(&["rev-parse", "FETCH_HEAD^{commit}"]), "git rev-parse")?;
                output.trim().to_string()
            }
        };

        if is_hash && !commit.eq_ignore_ascii_case(reference) {
            return Err(format!("{url} gave {commit} for the commit {reference}"));
        }

        // a plain tree of the sources, without .git, unpacked next to it: a tag like v1.2.3 has dots
        let unpacked = PathBuf::from(format!("{}.unpacked", target.display()));
        let tar = PathBuf::from(format!("{}.tar", target.display()));
        system::remove_dir(&unpacked)?;
        system::create_dir(&unpacked)?;

        let tar_option = format!("--output={}", tar.display());
        system::run_command_quietly(&mut in_objects(&["archive", "--format=tar", &tar_option, &commit]), "git archive")?;
        system::extract(&tar, &unpacked)?;
        fs::remove_file(&tar).map_err(|error| format!("could not delete {}: {error}", tar.display()))?;

        system::write_file(&unpacked.join(MARKER), &pinned)?;
        system::remove_dir(target)?;
        fs::rename(&unpacked, target).map_err(|error| format!("could not move {}: {error}", unpacked.display()))?;

        return Ok(target.to_path_buf());
    });
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn tags_check_out_whole_and_commits_by_twelve_characters() {
        assert_eq!(checkout_names("https://github.com/llvm/llvm-project.git", "llvmorg-19.1.6"), ("github.com/llvm/llvm-project".to_string(), "llvmorg-19.1.6".to_string()));
        assert_ne!(checkout_names("https://github.com/llvm/llvm-project", "llvmorg-19.1.6"), checkout_names("https://github.com/llvm/llvm-project", "llvmorg-19.1.7"));
        assert_eq!(checkout_names("https://github.com/fmtlib/fmt/", "407c905e45ad75fc29bf0f9bb7c5c2fd3475976f").1, "407c905e45ad");
        assert_eq!(checkout_names("file:///srv/library", "release/2.0"), ("srv/library".to_string(), "release-2.0".to_string()));
    }

    #[test]
    fn commits_are_checked_out_without_git_and_build_in_the_project() {
        let dir = ScratchDir::new("git");
        let repository = dir.join("library");
        system::write_file(&repository.join("include/library.h"), "#pragma once\n").unwrap();

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .output()
                .unwrap();
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        };
        git(&["init", "--quiet"]);
        // like GitHub: a commit can be fetched by its hash
        git(&["config", "uploadpack.allowAnySHA1InWant", "true"]);
        git(&["add", "."]);
        git(&["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "--quiet", "-m", "first"]);
        let commit = git(&["rev-parse", "HEAD"]);

        let url = format!("file://{}", repository.display());
        let checkout = checkout_to(&url, &commit, &dir.join("checkouts/library"), &dir.join("library.git")).unwrap();

        assert!(checkout.join("include/library.h").is_file());
        assert!(!checkout.join(".git").exists());

        let missing = "0".repeat(40);
        assert!(checkout_to(&url, &missing, &dir.join("checkouts/missing"), &dir.join("library.git")).is_err());

        // a module in the checkout builds in the project
        CHECKOUTS.lock()
            .unwrap()
            .push((checkout.clone(), dir.join("project/build/git/library")));
        let build_dir = project_build_dir(&checkout.join("examples/hello"));
        assert_eq!(build_dir, Some(dir.join("project/build/git/library/examples/hello/build")));
    }
}
