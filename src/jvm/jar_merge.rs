// Jars merged into one: the classes and resources of a fat jar, the Java resources of an apk
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::system;

/// What of the jars is merged
#[derive(Clone, Copy)]
pub enum JarContents {
    /// what an apk packages besides the dex files
    JavaResources,
    /// what a fat jar needs, the classes of newer Java versions included
    ClassesAndResources,
}

/// The files of the jars in `output`: the first jar wins, META-INF/services files are
/// concatenated. Returns how many classes were left out with another content than the winner.
pub fn merge_jars(
    jars: &[PathBuf],
    contents: JarContents,
    output: &Path,
    scratch: &Path,
) -> Result<usize, String> {
    system::recreate_dir(output)?;

    // jars in the cache never change, like those from Maven: they are extracted once
    let cache = system::cache_dir("");
    let cache_name = match contents {
        JarContents::JavaResources => "java-resources",
        JarContents::ClassesAndResources => "jar-contents",
    };

    let mut merged = HashSet::new();
    let mut differing_classes = 0;

    for jar in jars {
        if !merged.insert(jar) {
            continue;
        }

        let extracted = if jar.starts_with(&cache) {
            let extract = |target: &Path| extract_contents(jar, contents, target);
            system::kept_in_cache(cache_name, jar, extract)?
        } else {
            extract_contents(jar, contents, scratch)?;
            scratch.to_path_buf()
        };

        for file in system::files_below(&[extracted.clone()]) {
            let relative = file.strip_prefix(&extracted)
                .map_err(|error| error.to_string())?;

            let target = output.join(relative);

            if !target.exists() {
                system::copy_file(&file, &target)?;
                continue;
            }

            let is_service = relative.starts_with("META-INF/services");
            let is_class = relative.extension().is_some_and(|extension| extension == "class");

            if !is_service && !is_class {
                continue;
            }

            let content = system::read_bytes(&file)?;
            let mut existing = system::read_bytes(&target)?;

            if is_class {
                if existing != content {
                    differing_classes += 1;
                }
                continue;
            }

            if !existing.ends_with(b"\n") {
                existing.push(b'\n');
            }

            existing.extend(content);
            fs::write(&target, existing)
                .map_err(|error| format!("could not write {}: {error}", target.display()))?;
        }
    }

    system::remove_dir(scratch)?;

    return Ok(differing_classes);
}

/// Without the manifest and the signatures, they belong to the jar they come from
fn extract_contents(jar: &Path, contents: JarContents, target: &Path) -> Result<(), String> {
    system::recreate_dir(target)?;

    let signature = ["META-INF/MANIFEST.MF", "META-INF/*.SF", "META-INF/*.RSA", "META-INF/*.DSA", "META-INF/*.EC"];
    let excluded: &[&str] = match contents {
        JarContents::JavaResources => &["*.class", "META-INF/versions/*"],
        // a fat jar is no Java module
        JarContents::ClassesAndResources => {
            &["module-info.class", "META-INF/versions/*/module-info.class"]
        }
    };

    return system::extract_zip(jar, target, |name| {
        return !signature.iter()
            .chain(excluded)
            .any(|pattern| matches_pattern(pattern, name));
    });
}

/// Like the patterns of unzip: * stands for any characters, / too
fn matches_pattern(pattern: &str, name: &str) -> bool {
    let Some((before, after)) = pattern.split_once('*') else {
        return pattern == name;
    };

    let Some(rest) = name.strip_prefix(before) else {
        return false;
    };

    return (0..=rest.len()).any(|start| rest.is_char_boundary(start) && matches_pattern(after, &rest[start..]));
}

#[cfg(test)]
mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    /// A jar of `files` (path and content)
    fn jar(directory: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let content_dir = directory.join(format!("{name}-content"));

        for (path, content) in files {
            system::write_file(&content_dir.join(path), content).unwrap();
        }

        let jar = directory.join(format!("{name}.jar"));
        system::add_to_zip(&jar, &system::relative_files(&content_dir), false).unwrap();

        return jar;
    }

    #[test]
    fn the_first_jar_wins_services_are_concatenated_signatures_left_out() {
        let directory = ScratchDir::new("merge");


        let first = jar(
            &directory,
            "first",
            &[
                ("com/example/Shared.class", "first"),
                ("META-INF/services/com.example.Service", "com.example.First"),
                ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n"),
                ("META-INF/FIRST.SF", "signature"),
                ("module-info.class", "module"),
            ],
        );

        let second = jar(
            &directory,
            "second",
            &[
                ("com/example/Shared.class", "second"),
                ("META-INF/services/com.example.Service", "com.example.Second\n"),
                ("META-INF/versions/17/com/example/Newer.class", "newer"),
                ("META-INF/versions/17/module-info.class", "module"),
            ],
        );

        let output = directory.join("merged");
        let differing = merge_jars(
            &[first.clone(), second.clone()],
            JarContents::ClassesAndResources,
            &output,
            &directory.join("scratch"),
        )
        .unwrap();

        let read = |path: &str| system::read_file(&output.join(path)).unwrap();

        assert_eq!(read("com/example/Shared.class"), "first");
        assert_eq!(differing, 1);
        assert_eq!(
            read("META-INF/services/com.example.Service"),
            "com.example.First\ncom.example.Second\n"
        );
        assert_eq!(read("META-INF/versions/17/com/example/Newer.class"), "newer");

        for left_out in [
            "META-INF/MANIFEST.MF",
            "META-INF/FIRST.SF",
            "module-info.class",
            "META-INF/versions/17/module-info.class",
        ] {
            assert!(!output.join(left_out).exists(), "{left_out}");
        }

        // an apk gets the resources only
        merge_jars(
            &[first, second],
            JarContents::JavaResources,
            &output,
            &directory.join("scratch"),
        )
        .unwrap();

        assert!(!output.join("com/example/Shared.class").exists());
        assert!(!output.join("META-INF/versions").exists());
        assert!(output.join("META-INF/services/com.example.Service").is_file());

    }
}
