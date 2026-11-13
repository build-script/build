// Plugins of repositories: `use go` of the official list, `use "owner/repository"` of GitHub, another host by
// its name. The newest release of a plugin, or the one after @, its commit kept in project.lock.
use std::path::{Path, PathBuf};

use crate::common::{git, lock, system};

/// The official list, BUILD_SCRIPT_PLUGINS names another, like a mirror in a company
pub const OFFICIAL_REPOSITORY: &str = "https://github.com/build-script/plugins";

/// the file of a plugin in its directory
const PLUGIN_FILE: &str = "plugin.build";

/// next to plugin.build: a directory per template of `build new` and `build add`
pub const TEMPLATES: &str = "templates";

/// the build call of a template
pub const MODULE_BUILD: &str = "module.build";

/// What `use` names: a repository, the directory of the plugin in it and the tags of its releases
#[derive(Debug, PartialEq)]
pub struct PluginSource {
    /// for the lock and the log: go, github.com/owner/repository/zig
    pub name: String,
    pub url: String,
    /// below the repository, "" for its root
    pub directory: String,
    /// the tags of its releases start with one of these, the first that has tags counts: go-v, v
    pub tag_prefixes: Vec<String>,
    /// after @: a version or its start like 1.4, otherwise a branch or a commit
    pub selector: Option<String>,
}

pub fn official_repository() -> String {
    return std::env::var("BUILD_SCRIPT_PLUGINS").unwrap_or_else(|_| OFFICIAL_REPOSITORY.to_string());
}

impl PluginSource {
    /// `use go` and `use go@1.4`: go/plugin.build of the official list, its releases tagged go-v1.4.2
    pub fn official(written: &str, repository: &str) -> Self {
        let (name, selector) = split_selector(written);

        return PluginSource {
            name: name.to_string(),
            url: repository.to_string(),
            directory: name.to_string(),
            tag_prefixes: vec![format!("{name}-v")],
            selector,
        };
    }

    /// `use "owner/repository"` of GitHub, `use "gitlab.com/owner/repository"` of another host, a plugin in a
    /// directory of the repository after them, a release after @
    pub fn of_repository(written: &str) -> Result<Self, String> {
        let (path, selector) = split_selector(written);
        let parts: Vec<&str> = path.split('/')
            .filter(|part| !part.is_empty())
            .collect();

        // a first part with a dot names the host
        let (host, rest) = match parts.first() {
            Some(first) if first.contains('.') => (*first, &parts[1..]),
            _ => ("github.com", &parts[..]),
        };

        if rest.len() < 2 || rest.contains(&"..") {
            return Err(format!("{written} names no repository, like owner/repository or gitlab.com/owner/repository"));
        }

        let repository = format!("{host}/{}/{}", rest[0], rest[1]);
        let directory = rest[2..].join("/");

        // a plugin in a directory may have releases of its own: zig-v1.0.0
        let mut tag_prefixes = vec!["v".to_string()];
        if let Some(last) = rest[2..].last() {
            tag_prefixes.insert(0, format!("{last}-v"));
        }

        return Ok(PluginSource {
            name: match directory.is_empty() {
                true => repository.clone(),
                false => format!("{repository}/{directory}"),
            },
            url: format!("https://{repository}"),
            directory,
            tag_prefixes,
            selector,
        });
    }

    fn lock_key(&self) -> String {
        return match &self.selector {
            Some(selector) => format!("plugin:{}@{selector}", self.name),
            None => format!("plugin:{}", self.name),
        };
    }
}

fn split_selector(written: &str) -> (&str, Option<String>) {
    return match written.split_once('@') {
        Some((name, selector)) => (name, Some(selector.to_string())),
        None => (written, None),
    };
}

/// The plugin.build of the plugin: at the commit project.lock keeps, otherwise of the newest release or the
/// one @ names. --refresh stays in the major version the lock had.
pub fn locate(source: &PluginSource, cache: &Path) -> Result<PathBuf, String> {
    let key = source.lock_key();
    let locked = lock::locked_value(&key)
        .as_deref()
        .and_then(parse_locked);

    let resolved = locked.is_none();
    let (reference, commit) = match locked {
        Some(locked) => locked,
        None => {
            let previous = lock::previous_value(&key)
                .as_deref()
                .and_then(parse_locked)
                .map(|(reference, _)| reference);
            choose_release(source, previous.as_deref())?
        }
    };

    let checkout = git::checkout_pinned(&source.url, &reference, &commit, cache)?;
    let file = checkout.join(&source.directory).join(PLUGIN_FILE);

    if !file.is_file() {
        let directory = if source.directory.is_empty() { String::new() } else { format!("{}/", source.directory) };
        return Err(format!("{} has no {directory}{PLUGIN_FILE} at {reference}", source.url));
    }

    // said once, the next builds take it from the lock
    if resolved {
        let host = source.url.split_once("://").map_or(source.url.as_str(), |(_, rest)| rest);
        system::log_tool("plugins", &format!("{} {} from {host}", source.name, version_of(source, &reference)));
    }

    lock::keep_value(&key, &format!("commit={commit} ref={reference}"));
    return Ok(file);
}

/// "commit=<hash> ref=<tag>" of the lock: the tag and the commit
fn parse_locked(value: &str) -> Option<(String, String)> {
    let (commit, reference) = value.split_once(' ')?;

    return Some((
        reference.strip_prefix("ref=")?.to_string(),
        commit.strip_prefix("commit=")?.to_string(),
    ));
}

/// 1.4.2 of go-v1.4.2, a branch as it is
fn version_of(source: &PluginSource, reference: &str) -> String {
    return source.tag_prefixes
        .iter()
        .find_map(|prefix| reference.strip_prefix(prefix.as_str()))
        .unwrap_or(reference)
        .to_string();
}

fn parse_version(text: &str) -> Option<Vec<u64>> {
    return text.split('.')
        .map(|part| part.parse().ok())
        .collect();
}

/// The tag and commit of the release to take: the newest, the newest of the version @ starts, the newest of
/// the major version of `previous`; a branch or commit after @; the default branch without releases
fn choose_release(source: &PluginSource, previous: Option<&str>) -> Result<(String, String), String> {
    let references = git::remote_references(&source.url, &[])?;

    // the releases of the first prefix that has any: version, tag, commit
    let mut releases: Vec<(Vec<u64>, String, String)> = vec![];

    for prefix in &source.tag_prefixes {
        releases = references.iter()
            .filter_map(|(name, commit)| {
                let tag = name.strip_prefix("refs/tags/")?;
                let version = parse_version(tag.strip_prefix(prefix.as_str())?)?;
                return Some((version, tag.to_string(), commit.clone()));
            })
            .collect();

        if !releases.is_empty() {
            break;
        }
    }

    let newest = |wanted: &[u64]| {
        return releases.iter()
            .filter(|(version, _, _)| version.starts_with(wanted))
            .max_by(|first, second| first.0.cmp(&second.0))
            .map(|(_, tag, commit)| (tag.clone(), commit.clone()));
    };

    if let Some(selector) = &source.selector {
        if let Some(release) = parse_version(selector.trim_start_matches('v')).and_then(|wanted| newest(&wanted)) {
            return Ok(release);
        }

        if selector.len() == 40 && selector.chars().all(|char| char.is_ascii_hexdigit()) {
            return Ok((selector.clone(), selector.clone()));
        }

        let branch = git::remote_references(&source.url, &[selector])?
            .into_iter()
            .find(|(name, _)| name == &format!("refs/heads/{selector}") || name == &format!("refs/tags/{selector}"));

        return branch.map(|(_, commit)| (selector.clone(), commit))
            .ok_or_else(|| format!("{} has no release, branch or commit {selector}", source.url));
    }

    let major = previous.map(|previous| version_of(source, previous))
        .and_then(|version| parse_version(&version))
        .and_then(|version| version.first().copied());

    if let Some(release) = major.and_then(|major| newest(&[major])).or_else(|| newest(&[])) {
        return Ok(release);
    }

    let head = git::remote_references(&source.url, &["HEAD"])?;

    return head.into_iter()
        .next()
        .map(|(_, commit)| ("HEAD".to_string(), commit))
        .ok_or_else(|| format!("{} has neither releases nor a default branch", source.url));
}

/// The templates of the plugin in `plugin_dir`: the directories below templates/ with a module.build
pub fn template_names(plugin_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(plugin_dir.join(TEMPLATES))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join(MODULE_BUILD).is_file())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();

    names.sort();
    return names;
}

/// The first comment of a file: what a plugin or a template is for
fn first_comment(file: &Path) -> String {
    return std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.trim().strip_prefix("//"))
        .unwrap_or_default()
        .trim()
        .to_string();
}

/// The official plugins of the default branch: their names, the first comment of their plugin.build and
/// their templates with the first comment of their module.build
pub fn official_plugins(repository: &str, cache: &Path) -> Result<Vec<(String, String, Vec<(String, String)>)>, String> {
    let head = git::remote_references(repository, &["HEAD"])?;
    let Some((_, commit)) = head.into_iter().next() else {
        return Err(format!("{repository} has no default branch"));
    };

    let checkout = git::checkout_pinned(repository, "HEAD", &commit, cache)?;
    let mut plugins = vec![];

    for entry in std::fs::read_dir(&checkout).into_iter().flatten().flatten() {
        let file = entry.path().join(PLUGIN_FILE);

        if !file.is_file() {
            continue;
        }

        let templates = template_names(&entry.path())
            .into_iter()
            .map(|name| {
                let about = first_comment(&entry.path().join(TEMPLATES).join(&name).join(MODULE_BUILD));
                return (name, about);
            })
            .collect();

        plugins.push((entry.file_name().to_string_lossy().to_string(), first_comment(&file), templates));
    }

    plugins.sort();
    return Ok(plugins);
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn uses_name_repositories_directories_and_releases() {
        let official = PluginSource::official("go@1.4", "https://github.com/build-script/plugins");
        assert_eq!((official.directory.as_str(), official.tag_prefixes.clone(), official.selector.clone()), ("go", vec!["go-v".to_string()], Some("1.4".to_string())));
        assert_eq!(official.lock_key(), "plugin:go@1.4");

        let github = PluginSource::of_repository("someone/build-zig").unwrap();
        assert_eq!((github.url.as_str(), github.directory.as_str(), github.name.as_str()), ("https://github.com/someone/build-zig", "", "github.com/someone/build-zig"));

        let directory = PluginSource::of_repository("someone/plugins/zig@v2").unwrap();
        assert_eq!((directory.directory.as_str(), directory.tag_prefixes.clone()), ("zig", vec!["zig-v".to_string(), "v".to_string()]));
        assert_eq!(directory.selector.as_deref(), Some("v2"));

        let gitlab = PluginSource::of_repository("gitlab.com/someone/build-zig").unwrap();
        assert_eq!(gitlab.url, "https://gitlab.com/someone/build-zig");

        assert!(PluginSource::of_repository("someone").is_err());
        assert!(PluginSource::of_repository("someone/../x").is_err());
    }

    #[test]
    fn the_newest_release_is_taken_and_kept_in_the_lock() {
        let _turn = lock::TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = ScratchDir::new("plugins-repository");
        let repository = directory.join("plugins");

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .output()
                .unwrap();
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        };
        let release = |content: &str, tags: &[&str]| {
            system::write_file(&repository.join("go/plugin.build"), &format!("// Go programs\nversion = \"{content}\"\n")).unwrap();
            git(&["add", "."]);
            git(&["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "--quiet", "-m", content]);
            for tag in tags {
                git(&["-c", "user.name=test", "-c", "user.email=test@example.com", "tag", "-a", "-m", tag, tag]);
            }
            return git(&["rev-parse", "HEAD"]);
        };

        system::create_dir(&repository).unwrap();
        git(&["init", "--quiet"]);
        let first = release("1.4.2", &["go-v1.4.2", "rust-v9.0.0"]);
        let second = release("1.10.0", &["go-v1.10.0"]);
        release("2.0.0", &["go-v2.0.0"]);

        let url = format!("file://{}", repository.display());
        let cache = directory.join("cache");
        let script = directory.join("project/project.build");
        let content = |file: &Path| system::read_file(file).unwrap();

        lock::open(&script, false).unwrap();

        // the newest, of go and not of rust
        let newest = locate(&PluginSource::official("go", &url), &cache).unwrap();
        assert!(content(&newest).contains("2.0.0"));

        // the newest 1.x, by numbers: 1.10.0 is newer than 1.4.2
        let one = locate(&PluginSource::official("go@1", &url), &cache).unwrap();
        assert!(content(&one).contains("1.10.0"));
        assert_eq!(lock::locked_value("plugin:go@1"), Some(format!("commit={second} ref=go-v1.10.0")));

        let exact = locate(&PluginSource::official("go@1.4", &url), &cache).unwrap();
        assert!(content(&exact).contains("1.4.2"));
        lock::save().unwrap();

        // a tag that moved since the lock was written is an error
        git(&["tag", "-f", "-a", "-m", "moved", "go-v1.4.2", &second]);
        lock::open(&script, false).unwrap();
        let error = locate(&PluginSource::official("go@1.4", &url), &directory.join("other-cache")).unwrap_err();
        assert!(error.contains(&format!("project.lock has {first}")), "{error}");

        // --refresh takes the moved tag, and without @ the newest release of the major version the lock had
        lock::keep_value("plugin:go", &format!("commit={second} ref=go-v1.10.0"));
        lock::save().unwrap();
        lock::open(&script, true).unwrap();

        let moved = locate(&PluginSource::official("go@1.4", &url), &cache).unwrap();
        assert!(content(&moved).contains("1.10.0"), "{}", content(&moved));

        let same_major = locate(&PluginSource::official("go", &url), &cache).unwrap();
        assert!(content(&same_major).contains("1.10.0"), "{}", content(&same_major));

        let missing = locate(&PluginSource::official("zig", &url), &cache).unwrap_err();
        assert!(missing.contains("has no zig/plugin.build"), "{missing}");

        let names = official_plugins(&url, &cache).unwrap();
        assert_eq!(names, [("go".to_string(), "Go programs".to_string(), vec![])]);
        lock::close();
    }
}
