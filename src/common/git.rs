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
    // https://github.com/llvm/llvm-project becomes github.com/llvm/llvm-project
    let url_path = url.trim_end_matches('/')
        .trim_end_matches(".git");

    let repository = url_path.split_once("://")
        .map_or(url_path, |(_, path)| path)
        .trim_start_matches('/');

    let version: String = reference.chars()
        .take(12)
        .map(|char| if char == '/' {
            '-'
        } else {
            char
        })
        .collect();

    let cache = system::cache_dir("git");

    let target = cache.join("checkouts")
        .join(repository)
        .join(&version);

    let checkout = checkout_to(url, reference, &target, &cache.join(format!("{repository}.git")))?;

    let outputs = system::build_dir(project_dir)
        .join("git")
        .join(repository)
        .join(&version);

    CHECKOUTS.lock()
        .unwrap()
        .push((checkout.clone(), outputs));

    return Ok(checkout);
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

/// Into `target`, with the objects of the repository in `objects`
fn checkout_to(url: &str, reference: &str, target: &Path, objects: &Path) -> Result<PathBuf, String> {
    let pinned = format!("{url} {reference}");

    if fs::read_to_string(target.join(MARKER)).is_ok_and(|found| found == pinned) {
        return Ok(target.to_path_buf());
    }

    return system::installing(|| {
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
        let cached = is_hash && in_objects(&["cat-file", "-e", &commit_object]).output().is_ok_and(|output| output.status.success());

        let commit = match cached {
            true => reference.to_string(),
            false => {
                // only this commit, not the history before it
                system::log("git", &format!("Fetching {reference} of {url}"));
                system::run_command_quietly(&mut in_objects(&["fetch", "--quiet", "--depth", "1", url, reference]), "git fetch")?;

                let output = in_objects(&["rev-parse", "FETCH_HEAD^{commit}"])
                    .output()
                    .map_err(|error| format!("could not start git: {error}"))?;
                String::from_utf8_lossy(&output.stdout).trim().to_string()
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
        system::run_command_quietly(Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(&unpacked), "tar")?;
        fs::remove_file(&tar).map_err(|error| format!("could not delete {}: {error}", tar.display()))?;

        system::write_file(&unpacked.join(MARKER), &pinned)?;
        system::remove_dir(target)?;
        fs::rename(&unpacked, target).map_err(|error| format!("could not move {}: {error}", unpacked.display()))?;

        return Ok(target.to_path_buf());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_are_checked_out_without_git_and_build_in_the_project() {
        let dir = std::env::temp_dir().join(format!("build-script-git-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
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
        fs::remove_dir_all(&dir).unwrap();
    }
}
