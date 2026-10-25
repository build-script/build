// install for JVM modules: which version of a library, and whether it fits the module before it goes into the script
use std::path::Path;

use crate::common::system;

use super::maven::{self, DependencyResolver, MavenCoordinate, TargetPlatform};

/// The module a library is installed to, as its script says
pub struct InstallTarget<'a> {
    pub module: &'a str,
    /// what the build call gets: maven_central() and the like
    pub repositories: &'a [String],
    pub project_dir: &'a Path,
    pub android: bool,
    /// what its jdk argument says, like "21"
    pub jdk: &'a str,
}

/// The library with its version, the newest unless the notation has one, once
/// it and what it depends on resolved for the module and fit its JDK
pub fn checked_library(notation: &str, target: &InstallTarget) -> Result<MavenCoordinate, String> {
    let mut library = MavenCoordinate::parse(notation)?;
    let repositories = maven::repositories(target.repositories, target.project_dir)?;

    if library.version.is_empty() {
        library.version = maven::newest_version(&repositories, &library)?;

        if library.is_snapshot() {
            println!(
                "{}:{} has no release, {} is its newest SNAPSHOT",
                library.group, library.artifact, library.version
            );
        }
    }

    let platform = match target.android {
        true => TargetPlatform::Android,
        false => TargetPlatform::Jvm,
    };

    let mut resolver = DependencyResolver::new(repositories, platform);
    let modules = resolver.resolve_graph(&[library.clone()], &[])?;
    let files = resolver.artifacts(&modules)?;

    let name = |file: &Path| {
        file.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    };

    if !target.android {
        if let Some(aar) = files.iter()
            .find(|file| file.extension().is_some_and(|extension| extension == "aar"))
        {
            return Err(format!(
                "{library} needs Android, {} is an Android library and {} no Android module",
                name(aar),
                target.module
            ));
        }
    }

    let Some(jdk) = java_version(target.jdk) else {
        return Ok(library);
    };

    for jar in files.iter()
        .filter(|file| file.extension().is_some_and(|extension| extension == "jar"))
    {
        if let Some(needed) = class_java_version(jar)?.filter(|needed| *needed > jdk) {
            return Err(format!(
                "{} needs Java {needed}, {} builds with JDK {}: give it jdk = \"{needed}\"",
                name(jar),
                target.module,
                target.jdk
            ));
        }
    }

    return Ok(library);
}

/// The feature version of a jdk argument: 21 of "21" and "21.0.2", 8 of "1.8"
fn java_version(jdk: &str) -> Option<u32> {
    let version = jdk.strip_prefix("1.").unwrap_or(jdk);
    let digits: String = version.chars()
        .take_while(char::is_ascii_digit)
        .collect();
    return digits.parse().ok();
}

/// The Java version the classes of a jar were compiled for, from a few classes
/// outside META-INF, where multi release jars keep those for newer versions
fn class_java_version(jar: &Path) -> Result<Option<u32>, String> {
    // what unzip cannot read, the build will complain about
    let Ok(entries) = system::zip_entries(jar) else {
        return Ok(None);
    };

    let classes = entries.iter()
        .filter(|entry| entry.ends_with(".class") && !entry.starts_with("META-INF/"))
        .filter(|entry| !entry.ends_with("module-info.class"))
        // unzip reads them as patterns
        .filter(|entry| !entry.contains(['$', '*', '?', '[', '\\']))
        .take(3);

    let mut version = None;

    for class in classes {
        let bytes = system::read_zip_entry(jar, class)?;

        // the magic number, the minor and the major version
        if bytes.len() >= 8 && bytes[..4] == [0xCA, 0xFE, 0xBA, 0xBE] {
            let major = u16::from_be_bytes([bytes[6], bytes[7]]) as u32;
            version = version.max(Some(major.saturating_sub(44)));
        }
    }

    return Ok(version);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdk_arguments_name_a_java_version() {
        assert_eq!(java_version("21"), Some(21));
        assert_eq!(java_version("21.0.2"), Some(21));
        assert_eq!(java_version("1.8"), Some(8));
        assert_eq!(java_version("temurin"), None);
    }
}
