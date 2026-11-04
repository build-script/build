// Java resources: the files next to the classes in a jar or an apk, put together like processResources in Gradle
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::common::system;
use crate::language::runtime::Arguments;

/// Files Gradle leaves out of resource directories by default
const DEFAULT_EXCLUDES: &[&str] = &[
    ".DS_Store",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".git",
    ".svn",
    ".hg",
];

/// The resources of a module as its build script declares them
#[derive(Debug, Clone, Default)]
pub struct JavaResources {
    /// directories whose content lands at the root, like sourceSets.main.resources
    pub dirs: Vec<PathBuf>,
    /// path in the jar and the file or directory of the project it comes from
    pub files: Vec<(PathBuf, PathBuf)>,
    /// path in the jar and its content
    pub texts: Vec<(PathBuf, String)>,
    /// @name@ in text resources and the value it becomes, like filter(ReplaceTokens)
    pub tokens: Vec<(String, String)>,
}

impl JavaResources {
    /// resource_dirs, resource_files, resource_texts and resource_tokens, paths relative to `directory`
    pub fn read(args: &Arguments, directory: &Path) -> Result<Self, String> {
        let dirs = args.get::<Vec<String>>("resource_dirs")?
            .iter()
            .map(|dir| directory.join(dir))
            .collect();

        let files = args.assignments("resource_files")?
            .into_iter()
            .map(|(target, source)| Ok((resource_path(&target)?, directory.join(source.trim()))))
            .collect::<Result<Vec<_>, String>>()?;

        let texts = args.assignments("resource_texts")?
            .into_iter()
            .map(|(target, content)| Ok((resource_path(&target)?, content)))
            .collect::<Result<Vec<_>, String>>()?;

        let tokens = args.assignments("resource_tokens")?;

        if let Some((name, _)) = tokens.iter()
            .find(|(name, _)| name.is_empty() || name.contains('@'))
        {
            return Err(format!("resource_tokens: '{name}' is not a token name"));
        }

        return Ok(JavaResources {
            dirs,
            files,
            texts,
            tokens,
        });
    }

    /// For up-to-date checks
    pub fn inputs(&self) -> Vec<PathBuf> {
        let mut inputs = self.dirs.clone();
        inputs.extend(self.files.iter().map(|(_, source)| source.clone()));

        return inputs;
    }

    /// The resources in `output` like build/resources/main of Gradle, tokens replaced.
    /// Two resources for one path are an error, like in Gradle 7.
    pub fn process(&self, generated_dirs: &[PathBuf], output: &Path) -> Result<(), String> {
        let mut resources: BTreeMap<PathBuf, Resource> = BTreeMap::new();

        let mut add =
            |target: PathBuf, content: ResourceContent, origin: String| -> Result<(), String> {
                if let Some(existing) = resources.get(&target) {
                    return Err(format!(
                        "resource {} comes from {} and from {origin}",
                        target.display(),
                        existing.origin
                    ));
                }

                resources.insert(target, Resource { content, origin });

                return Ok(());
            };

        for dir in self.dirs.iter().chain(generated_dirs) {
            for (relative, file) in resource_files_below(dir)? {
                add(
                    relative,
                    ResourceContent::File(file),
                    dir.display().to_string(),
                )?;
            }
        }

        for (target, source) in &self.files {
            if source.is_dir() {
                for (relative, file) in resource_files_below(source)? {
                    add(
                        target.join(relative),
                        ResourceContent::File(file),
                        format!("resource_files {}", source.display()),
                    )?;
                }
            } else if source.is_file() {
                add(
                    target.clone(),
                    ResourceContent::File(source.clone()),
                    format!("resource_files {}", source.display()),
                )?;
            } else {
                return Err(format!(
                    "resource_files: {} does not exist",
                    source.display()
                ));
            }
        }

        for (target, text) in &self.texts {
            add(
                target.clone(),
                ResourceContent::Text(text.clone()),
                "resource_texts".to_string(),
            )?;
        }

        system::recreate_dir(output)?;
        for (target, resource) in resources {
            let file = output.join(&target);

            let content = match (resource.content, self.tokens.is_empty()) {
                (ResourceContent::File(source), true) => {
                    system::copy_file(&source, &file)?;
                    continue;
                }
                (ResourceContent::File(source), false) => system::read_bytes(&source)?,
                (ResourceContent::Text(text), _) => text.into_bytes(),
            };

            system::create_parent_dir(&file)?;
            fs::write(&file, self.replace_tokens(content))
                .map_err(|error| format!("could not write {}: {error}", file.display()))?;
        }

        return Ok(());
    }

    /// @name@ of the declared tokens replaced, in text only; other @...@ stay
    fn replace_tokens(&self, content: Vec<u8>) -> Vec<u8> {
        if self.tokens.is_empty() || !content.contains(&b'@') {
            return content;
        }

        let Ok(mut text) = String::from_utf8(content.clone()) else {
            return content;
        };

        for (name, value) in &self.tokens {
            text = text.replace(&format!("@{name}@"), value);
        }

        return text.into_bytes();
    }
}

/// One file of the resources: where it comes from, and for errors who declared it
struct Resource {
    content: ResourceContent,
    origin: String,
}

enum ResourceContent {
    File(PathBuf),
    Text(String),
}

/// With their paths relative to `dir`, without the default excludes
fn resource_files_below(dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut files = vec![];

    for file in system::files_below(&[dir.to_path_buf()]) {
        let relative = file.strip_prefix(dir)
            .map_err(|error| error.to_string())?
            .to_path_buf();

        let excluded = relative.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            DEFAULT_EXCLUDES.contains(&name.as_ref())
                || name.starts_with("._")
                || name.ends_with('~')
        });

        if !excluded {
            files.push((relative, file));
        }
    }

    return Ok(files);
}

/// Relative and without "..", so it stays inside the jar
fn resource_path(target: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(target.trim().trim_end_matches('/'));

    let inside = !path.as_os_str().is_empty()
        && path.components()
            .all(|component| matches!(component, Component::Normal(_)));

    if !inside {
        return Err(format!(
            "resource path '{target}' must be relative and without '..'"
        ));
    }

    return Ok(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("build-script-resources-{}", std::process::id()));

        let _ = fs::remove_dir_all(&directory);
        system::write_file(
            &directory.join("src/main/resources/version.properties"),
            "version=@version@\nkeep=@other@ and ${version}\n",
        )
        .unwrap();
        system::write_file(&directory.join("src/main/resources/.DS_Store"), "finder").unwrap();
        fs::write(
            directory.join("src/main/resources/logo.bin"),
            [
                0xff, b'@', b'v', b'e', b'r', b's', b'i', b'o', b'n', b'@', 0xfe,
            ],
        )
        .unwrap();
        system::write_file(&directory.join("docs/SKILL.md"), "# Skill @version@\n").unwrap();
        system::write_file(&directory.join("docs/scripts/install.sh"), "echo ${HOME}\n").unwrap();

        return directory;
    }

    #[test]
    fn resources_come_together_with_the_tokens_replaced() {
        let directory = project();

        let resources = JavaResources {
            dirs: vec![
                directory.join("src/main/resources"),
                directory.join("src/missing/resources"),
            ],
            files: vec![
                (PathBuf::from("skill.md"), directory.join("docs/SKILL.md")),
                (PathBuf::from("scripts"), directory.join("docs/scripts")),
            ],
            texts: vec![(PathBuf::from("meta/version.txt"), "1.5.0".to_string())],
            tokens: vec![("version".to_string(), "1.5.0".to_string())],
        };

        let output = directory.join("build/resources/main");
        resources.process(&[], &output).unwrap();

        let read = |path: &str| fs::read_to_string(output.join(path)).unwrap();
        // only declared tokens, ${...} is no token
        assert_eq!(
            read("version.properties"),
            "version=1.5.0\nkeep=@other@ and ${version}\n"
        );
        assert_eq!(read("skill.md"), "# Skill 1.5.0\n");
        assert_eq!(read("scripts/install.sh"), "echo ${HOME}\n");
        assert_eq!(read("meta/version.txt"), "1.5.0");
        // binary files stay as they are, Finder files stay out
        assert_eq!(
            fs::read(output.join("logo.bin")).unwrap()[1..10],
            *b"@version@"
        );
        assert!(!output.join(".DS_Store").exists());

        // one path from two places is an error
        let twice = JavaResources {
            texts: vec![(PathBuf::from("version.properties"), "x".to_string())],
            ..resources.clone()
        };

        let error = twice.process(&[], &output).unwrap_err();
        assert!(
            error.contains("resource version.properties comes from"),
            "{error}"
        );

        let missing = JavaResources {
            files: vec![(PathBuf::from("a"), directory.join("nope"))],
            ..JavaResources::default()
        };

        assert!(
            missing
                .process(&[], &output)
                .unwrap_err()
                .contains("does not exist")
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn resource_paths_stay_inside_the_jar() {
        assert_eq!(resource_path("scripts/").unwrap(), PathBuf::from("scripts"));
        assert!(resource_path("../outside").is_err());
        assert!(resource_path("/etc/passwd").is_err());
        assert!(resource_path(" ").is_err());
    }
}
