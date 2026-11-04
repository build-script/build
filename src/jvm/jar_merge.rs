// Jars merged into one: the classes and resources of a fat jar, the Java resources of an apk,
// and relocation, which moves packages of libraries so copies in other jars do not clash
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::system;

use super::maven::{DependencyResolver, MavenCoordinate, TargetPlatform};
use super::module::{ModuleBuildInputs, compile_java};

const JAR_RELOCATOR_VERSION: &str = "1.7";
/// jar-relocator brings ASM 9.2, which cannot read class files of newer Java versions
const ASM_VERSION: &str = "9.11.1";

/// jar-relocator has no command line
const RELOCATOR_DRIVER: &str = r#"import java.io.File;
import java.util.HashMap;
import java.util.Map;

import me.lucko.jarrelocator.JarRelocator;

/** Arguments: the input jar, the output jar, then packages as from=to */
public final class Relocate {
    public static void main(String[] arguments) throws Exception {
        Map<String, String> relocations = new HashMap<>();

        for (int index = 2; index < arguments.length; index++) {
            String[] relocation = arguments[index].split("=", 2);
            relocations.put(relocation[0], relocation[1]);
        }

        new JarRelocator(new File(arguments[0]), new File(arguments[1]), relocations).run();
    }
}
"#;

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

/// Moves the packages of `relocations` (from, to) in the bytecode and the resource
/// paths of `input`, with jar-relocator from Maven
pub fn relocate(
    inputs: &ModuleBuildInputs,
    input: &Path,
    output: &Path,
    relocations: &[(String, String)],
) -> Result<(), String> {
    let tools = [
        MavenCoordinate::new("me.lucko", "jar-relocator", JAR_RELOCATOR_VERSION),
        MavenCoordinate::new("org.ow2.asm", "asm", ASM_VERSION),
        MavenCoordinate::new("org.ow2.asm", "asm-commons", ASM_VERSION),
    ];

    let mut resolver = DependencyResolver::new(inputs.repositories.clone(), TargetPlatform::Jvm);
    let modules = resolver.resolve_graph(&tools, &[])?;
    let mut classpath = resolver.artifacts(&modules)?;

    // compiled once for every bytecode level, modules building at the same time may need it
    let jvm_target = inputs.jdk.jvm_target();
    let driver = system::installing(&system::cache_dir("jar-relocator"), || {
        let source = system::cache_dir("jar-relocator").join("Relocate.java");

        if !source.is_file() {
            system::write_file(&source, RELOCATOR_DRIVER)?;
        }

        return system::kept_in_cache(&format!("jar-relocator-{jvm_target}"), &source, |target| {
            compile_java(&inputs.jdk, &[source.clone()], &classpath, target, &jvm_target, None)
        });
    })?;

    classpath.insert(0, driver);

    let mut command = inputs.jdk.java_command(&[], &classpath, "Relocate")?;
    command.arg(input).arg(output);

    for (from, to) in relocations {
        command.arg(format!("{from}={to}"));
    }

    return system::run_command(&mut command, "jar-relocator");
}

/// Service files name classes with dots in their file name and lines, jar-relocator
/// only moves bytecode and paths
pub fn relocate_service_files(
    contents: &Path,
    relocations: &[(String, String)],
) -> Result<(), String> {
    let services = contents.join("META-INF/services");

    for file in system::files_below(&[services.clone()]) {
        let name = file.file_name()
            .unwrap_or_default()
            .to_string_lossy();

        let lines: Vec<String> = system::read_file(&file)?
            .lines()
            .map(|line| relocated_class(line, relocations))
            .collect();

        fs::remove_file(&file)
            .map_err(|error| format!("could not delete {}: {error}", file.display()))?;

        let relocated_file = services.join(relocated_class(&name, relocations));
        system::write_file(&relocated_file, &(lines.join("\n") + "\n"))?;
    }

    return Ok(());
}

/// org.bstats.Metrics with org.bstats=dev.mvmo.libs.bstats: dev.mvmo.libs.bstats.Metrics
fn relocated_class(name: &str, relocations: &[(String, String)]) -> String {
    for (from, to) in relocations {
        if let Some(rest) = name.strip_prefix(from.as_str()) {
            if rest.is_empty() || rest.starts_with('.') {
                return format!("{to}{rest}");
            }
        }
    }

    return name.to_string();
}

#[cfg(test)]
mod tests {
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
        let directory =
            std::env::temp_dir().join(format!("build-script-merge-{}", std::process::id()));

        let _ = fs::remove_dir_all(&directory);

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

        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn service_files_are_relocated_by_name_and_content() {
        let contents =
            std::env::temp_dir().join(format!("build-script-services-{}", std::process::id()));

        let _ = fs::remove_dir_all(&contents);

        system::write_file(
            &contents.join("META-INF/services/org.bstats.Service"),
            "org.bstats.charts.Chart\norg.bstatsother.Kept\n",
        )
        .unwrap();

        let relocations = [("org.bstats".to_string(), "dev.mvmo.libs.bstats".to_string())];
        relocate_service_files(&contents, &relocations).unwrap();

        assert_eq!(
            system::read_file(&contents.join("META-INF/services/dev.mvmo.libs.bstats.Service"))
                .unwrap(),
            "dev.mvmo.libs.bstats.charts.Chart\norg.bstatsother.Kept\n"
        );
        assert!(!contents.join("META-INF/services/org.bstats.Service").exists());

        fs::remove_dir_all(&contents).unwrap();
    }
}
