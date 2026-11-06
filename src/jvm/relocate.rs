// Relocation moves packages of the libraries inside a fat jar, so copies in other jars do not clash
use std::fs;
use std::path::Path;

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
    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn service_files_are_relocated_by_name_and_content() {
        let contents = ScratchDir::new("services");


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

    }
}
