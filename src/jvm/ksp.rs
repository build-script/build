// KSP: Kotlin symbol processors like Room, Moshi or Dagger, run with KSP2's command line before kotlinc
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value as Json;

use crate::common::{system, up_to_date};
use crate::language::runtime::Arguments;

use super::maven::{self, DependencyResolver, MavenCoordinate};
use super::module::{Dependencies, ModuleBuildInputs};

pub const DEFAULT_KSP_VERSION: &str = "2.3.12";
const KSP_MAIN_CLASS: &str = "com.google.devtools.ksp.cmdline.KSPJvmMain";
/// KSP splits its lists at File.pathSeparatorChar. Its JVM gets one that is in no
/// path and no option value, so values may contain ':' like URLs.
const KSP_LIST_SEPARATOR: char = '\u{1F}';
/// in the caches directory: what the last run saw
const SNAPSHOT_FILE_NAME: &str = "build-script-snapshot";
/// in the caches directory: KSP's map of sources to their outputs
const SOURCE_TO_OUTPUTS_FILE_NAME: &str = "sourceToOutputs";
/// The entry of sourceToOutputs with outputs of incremental runs that became
/// invalid, KSP leaves deleting them to its caller
const REMOVED_OUTPUTS_KEY: &str = "<This is a virtual key for removed outputs; DO NOT USE>";

/// The symbol processors of a module, like ksp(...) dependencies in Gradle
#[derive(Debug, Clone)]
pub struct SymbolProcessors {
    pub processors: Dependencies,
    /// "key=value", like ksp { arg("key", "value") } in Gradle
    pub options: Vec<String>,
    pub ksp_version: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSymbolProcessors {
    tool: Vec<PathBuf>,
    processors: Vec<PathBuf>,
    /// the Kotlin KSP analyzes sources with, it does not know newer language versions
    analyzer_kotlin_version: String,
}

/// One run of KSP over a set of sources: the main ones or the tests
pub struct SymbolProcessing<'a> {
    pub inputs: &'a ModuleBuildInputs,
    pub resolved: &'a ResolvedSymbolProcessors,
    pub source_dirs: &'a [PathBuf],
    pub module_name: &'a str,
    /// the compile classpath of the sources
    pub libraries: &'a [PathBuf],
    /// of the libraries, those whose internal declarations the sources see,
    /// like the module under test for its tests
    pub friends: &'a [PathBuf],
    pub jvm_target: &'a str,
    /// None for Android, where java.* comes from android.jar among the libraries
    pub jdk_home: Option<&'a Path>,
    /// build/generated/ksp/<main|test>
    pub output_dir: PathBuf,
    /// build/ksp-caches/<main|test>, KSP keeps what it needs for incremental runs there
    pub caches_dir: PathBuf,
}

pub struct SymbolProcessorOutput {
    pub kotlin: PathBuf,
    pub java: PathBuf,
    pub resources: PathBuf,
}

impl SymbolProcessors {
    /// Of modules without Kotlin
    pub fn none() -> Self {
        return SymbolProcessors {
            processors: Dependencies::default(),
            options: vec![],
            ksp_version: DEFAULT_KSP_VERSION.to_string(),
        };
    }

    /// The processors of the list argument `name`, with ksp_options and ksp_version
    pub fn read(args: &Arguments, name: &str, kotlin_version: &str) -> Result<Self, String> {
        return Ok(SymbolProcessors {
            processors: Dependencies::read(args, name, kotlin_version)?,
            options: args.get("ksp_options")?,
            ksp_version: args.get("ksp_version")?,
        });
    }

    /// KSP and the processors, None without processors. Processors that are
    /// modules run with kotlin-stdlib `kotlin_version`.
    pub fn resolve(
        &self,
        resolver: &mut DependencyResolver,
        kotlin_version: &str,
    ) -> Result<Option<ResolvedSymbolProcessors>, String> {
        if self.processors.is_empty() {
            return Ok(None);
        }

        let ksp = MavenCoordinate::new(
            "com.google.devtools.ksp",
            "symbol-processing-aa-embeddable",
            &self.ksp_version,
        );

        let tool_modules = resolver.resolve_graph(&[ksp], &[])?;

        let analyzer_kotlin_version = tool_modules.iter()
            .find(|module| {
                module.group == "org.jetbrains.kotlin" && module.artifact == "kotlin-stdlib"
            })
            .map(|module| module.version.clone())
            .ok_or_else(|| format!("KSP {} comes without kotlin-stdlib", self.ksp_version))?;

        return Ok(Some(ResolvedSymbolProcessors {
            tool: resolver.artifacts(&tool_modules)?,
            processors: self
                .processors
                .resolve_processor_path(resolver, kotlin_version)?,
            analyzer_kotlin_version,
        }));
    }
}

impl SymbolProcessing<'_> {
    /// Runs the processors, kotlinc and javac compile what they generate. Incremental
    /// like with Gradle: only changed sources, no run when none changed.
    pub fn run(&self) -> Result<SymbolProcessorOutput, String> {
        let output = SymbolProcessorOutput {
            kotlin: self.output_dir.join("kotlin"),
            java: self.output_dir.join("java"),
            resources: self.output_dir.join("resources"),
        };

        let language_version = language_version_capped_at(
            &self.inputs.kotlin_version,
            &self.resolved.analyzer_kotlin_version,
        );

        let snapshot = SymbolProcessingSnapshot {
            configuration: self.configuration_hash(&language_version)?,
            sources: source_hashes(self.source_dirs)?,
        };

        let snapshot_file = self.caches_dir.join(SNAPSHOT_FILE_NAME);

        let changes = SymbolProcessingSnapshot::read(&snapshot_file)
            .filter(|previous| {
                previous.configuration == snapshot.configuration && self.output_dir.is_dir()
            })
            .map(|previous| snapshot.changes_since(&previous));

        match changes {
            Some(changes) if changes.is_empty() => {
                system::log(&self.inputs.name, "Symbol processors up to date");
                self.delete_removed_outputs()?;

                return Ok(output);
            }
            Some(changes) => {
                system::log(
                    &self.inputs.name,
                    &format!(
                        "Running symbol processors on {} changed file(s) (KSP, Kotlin {language_version})",
                        changes.modified.len() + changes.removed.len(),
                    ),
                );
                if self.execute(&language_version, Some(&changes)).is_err() {
                    // the caches may not fit any more, a complete run sorts that out
                    system::log(
                        &self.inputs.name,
                        "Incremental run failed, running all sources again",
                    );
                    self.execute_from_scratch(&language_version)?;
                }
            }
            None => {
                system::log(
                    &self.inputs.name,
                    &format!("Running symbol processors (KSP, Kotlin {language_version})"),
                );
                self.execute_from_scratch(&language_version)?;
            }
        }

        self.delete_removed_outputs()?;
        snapshot.write(&snapshot_file)?;

        return Ok(output);
    }

    /// Of incremental runs, no longer generated. KSP names them relative to the module directory.
    fn delete_removed_outputs(&self) -> Result<(), String> {
        let Ok(content) = fs::read_to_string(self.caches_dir.join(SOURCE_TO_OUTPUTS_FILE_NAME))
        else {
            return Ok(());
        };

        let source_to_outputs: Json = serde_json::from_str(&content).map_err(|error| {
            format!("KSP's {SOURCE_TO_OUTPUTS_FILE_NAME} is not valid JSON: {error}")
        })?;

        let removed = source_to_outputs.get(REMOVED_OUTPUTS_KEY)
            .and_then(Json::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        for output in removed.iter().filter_map(Json::as_str) {
            let file = self.inputs.directory.join(output);
            if file.is_file() {
                fs::remove_file(&file)
                    .map_err(|error| format!("could not delete {}: {error}", file.display()))?;
            }
        }

        return Ok(());
    }

    /// Without caches and old output, nothing stale survives
    fn execute_from_scratch(&self, language_version: &str) -> Result<(), String> {
        system::remove_dir(&self.output_dir)?;
        system::remove_dir(&self.caches_dir)?;

        return self.execute(language_version, None);
    }

    fn execute(
        &self,
        language_version: &str,
        changes: Option<&SourceChanges>,
    ) -> Result<(), String> {
        let source_roots: Vec<PathBuf> = self.source_dirs.iter()
            .filter(|dir| dir.is_dir())
            .cloned()
            .collect();

        // KSP takes friends apart from the other libraries
        let libraries: Vec<PathBuf> = self.libraries.iter()
            .filter(|library| !self.friends.contains(library))
            .cloned()
            .collect();

        let mut command = Command::new(self.inputs.jdk.tool_path("java"));
        command
            // the JVM splits its own classpath at the separator as well
            .arg(format!("-Dpath.separator={KSP_LIST_SEPARATOR}"))
            .arg("-cp")
            .arg(join_ksp_list(&self.resolved.tool))
            .arg(KSP_MAIN_CLASS)
            .arg(format!("-jvm-target={}", self.jvm_target))
            .arg(format!("-module-name={}", self.module_name))
            .arg(list_option("-source-roots", &source_roots))
            .arg(list_option("-java-source-roots", &source_roots))
            .arg(list_option("-libraries", &libraries))
            .arg(path_option("-project-base-dir", &self.inputs.directory))
            .arg(path_option("-output-base-dir", &self.output_dir))
            .arg(path_option("-caches-dir", &self.caches_dir))
            .arg(path_option(
                "-class-output-dir",
                &self.output_dir.join("classes"),
            ))
            .arg(path_option(
                "-kotlin-output-dir",
                &self.output_dir.join("kotlin"),
            ))
            .arg(path_option(
                "-java-output-dir",
                &self.output_dir.join("java"),
            ))
            .arg(path_option(
                "-resource-output-dir",
                &self.output_dir.join("resources"),
            ))
            .arg(format!("-language-version={language_version}"))
            .arg(format!("-api-version={language_version}"))
            .arg("-incremental=true");

        if let Some(changes) = changes {
            if !changes.modified.is_empty() {
                command.arg(list_option("-modified-sources", &changes.modified));
            }

            if !changes.removed.is_empty() {
                command.arg(list_option("-removed-sources", &changes.removed));
            }
        }

        if !self.friends.is_empty() {
            command.arg(list_option("-friends", self.friends));
        }

        if let Some(jdk_home) = self.jdk_home {
            command.arg(path_option("-jdk-home", jdk_home));
        }

        if !self.inputs.symbol_processors.options.is_empty() {
            command.arg(format!(
                "-processor-options={}",
                self.inputs
                    .symbol_processors
                    .options
                    .join(&KSP_LIST_SEPARATOR.to_string())
            ));
        }

        // the processor classpath comes last
        command.arg(join_ksp_list(&self.resolved.processors));
        // relative paths in options, like room.schemaLocation, belong to the module
        command.current_dir(&self.inputs.directory);

        return system::run_command(&mut command, "KSP");
    }

    /// What makes earlier output invalid when it changes: processors, options,
    /// language version and the classpath, modules built before included
    fn configuration_hash(&self, language_version: &str) -> Result<u64, String> {
        let mut hasher = DefaultHasher::new();
        self.resolved.tool.hash(&mut hasher);
        self.inputs.symbol_processors.options.hash(&mut hasher);
        language_version.hash(&mut hasher);
        self.jvm_target.hash(&mut hasher);

        // processors that are modules of the script change in place like libraries
        for library in self.resolved.processors.iter().chain(self.libraries) {
            library.hash(&mut hasher);
            if library.is_dir() {
                // class directories like R and BuildConfig of Android apps are
                // compiled anew by every build, their content decides
                for file in system::files_below(&[library.clone()]) {
                    file.hash(&mut hasher);
                    up_to_date::content_hash(&file)?.hash(&mut hasher);
                }
            } else {
                // jars of modules built before change in place, Maven jars do not
                let metadata = fs::metadata(library).ok();
                metadata.as_ref()
                    .map(|metadata| metadata.len())
                    .hash(&mut hasher);
                metadata.and_then(|metadata| metadata.modified().ok())
                    .hash(&mut hasher);
            }
        }

        return Ok(hasher.finish());
    }
}

/// The sources one run of KSP saw
#[derive(Debug, PartialEq)]
struct SymbolProcessingSnapshot {
    configuration: u64,
    sources: BTreeMap<PathBuf, u64>,
}

#[derive(Debug, PartialEq)]
struct SourceChanges {
    modified: Vec<PathBuf>,
    removed: Vec<PathBuf>,
}

impl SourceChanges {
    fn is_empty(&self) -> bool {
        return self.modified.is_empty() && self.removed.is_empty();
    }
}

impl SymbolProcessingSnapshot {
    /// Sources that are new or changed, and sources that are gone
    fn changes_since(&self, previous: &SymbolProcessingSnapshot) -> SourceChanges {
        let modified = self.sources.iter()
            .filter(|(path, hash)| previous.sources.get(*path) != Some(*hash))
            .map(|(path, _)| path.clone())
            .collect();

        let removed = previous.sources.keys()
            .filter(|path| !self.sources.contains_key(*path))
            .cloned()
            .collect();

        return SourceChanges { modified, removed };
    }

    /// First line the configuration, then a hash and a path per line
    fn read(file: &Path) -> Option<SymbolProcessingSnapshot> {
        let content = fs::read_to_string(file).ok()?;
        let mut lines = content.lines();
        let configuration = lines.next()?
            .parse()
            .ok()?;
        let mut sources = BTreeMap::new();

        for line in lines {
            let (hash, path) = line.split_once('\t')?;
            sources.insert(PathBuf::from(path), hash.parse().ok()?);
        }

        return Some(SymbolProcessingSnapshot {
            configuration,
            sources,
        });
    }

    fn write(&self, file: &Path) -> Result<(), String> {
        let mut content = format!("{}\n", self.configuration);

        for (path, hash) in &self.sources {
            content.push_str(&format!("{hash}\t{}\n", path.display()));
        }

        return system::write_file(file, &content);
    }
}

/// Of the Kotlin and Java files below the directories
fn source_hashes(source_dirs: &[PathBuf]) -> Result<BTreeMap<PathBuf, u64>, String> {
    let mut sources = BTreeMap::new();

    for file in system::files_below(source_dirs) {
        if !file.extension()
            .is_some_and(|extension| extension == "kt" || extension == "java")
        {
            continue;
        }

        let hash = up_to_date::content_hash(&file)?;
        sources.insert(file, hash);
    }

    return Ok(sources);
}

/// The language version of the module, KSP's analyzer does not know newer ones
fn language_version_capped_at(
    module_kotlin_version: &str,
    analyzer_kotlin_version: &str,
) -> String {
    let major_minor = |version: &str| version.split('.').take(2).collect::<Vec<_>>().join(".");
    let (module, analyzer) = (
        major_minor(module_kotlin_version),
        major_minor(analyzer_kotlin_version),
    );

    if maven::compare_versions(&module, &analyzer).is_gt() {
        return analyzer;
    }

    return module;
}

fn path_option(name: &str, path: &Path) -> OsString {
    let mut option = OsString::from(format!("{name}="));
    option.push(path);

    return option;
}

fn list_option(name: &str, paths: &[PathBuf]) -> OsString {
    let mut option = OsString::from(format!("{name}="));
    option.push(join_ksp_list(paths));

    return option;
}

/// Paths joined with KSP's list separator
fn join_ksp_list(paths: &[PathBuf]) -> OsString {
    let mut joined = OsString::new();

    for (index, path) in paths.iter().enumerate() {
        if index > 0 {
            joined.push(KSP_LIST_SEPARATOR.to_string());
        }

        joined.push(path);
    }

    return joined;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_language_version_is_capped_at_the_analyzer() {
        assert_eq!(language_version_capped_at("2.4.21", "2.3.20"), "2.3");
        assert_eq!(language_version_capped_at("2.1.0", "2.3.20"), "2.1");
        assert_eq!(language_version_capped_at("1.9.25", "2.3.20"), "1.9");
    }

    #[test]
    fn snapshots_tell_which_sources_changed() {
        let snapshot = |sources: &[(&str, u64)]| SymbolProcessingSnapshot {
            configuration: 7,
            sources: sources.iter()
                .map(|(path, hash)| (PathBuf::from(path), *hash))
                .collect(),
        };

        let previous = snapshot(&[("a/Kept.kt", 1), ("a/Changed.kt", 2), ("a/Removed.kt", 3)]);
        let current = snapshot(&[("a/Kept.kt", 1), ("a/Changed.kt", 20), ("a/New.kt", 4)]);

        assert_eq!(
            current.changes_since(&previous),
            SourceChanges {
                modified: vec![PathBuf::from("a/Changed.kt"), PathBuf::from("a/New.kt")],
                removed: vec![PathBuf::from("a/Removed.kt")],
            }
        );
        assert!(current.changes_since(&current).is_empty());

        let file =
            std::env::temp_dir().join(format!("build-script-ksp-snapshot-{}", std::process::id()));

        current.write(&file).unwrap();
        assert_eq!(SymbolProcessingSnapshot::read(&file), Some(current));
        fs::remove_file(&file).unwrap();
    }

    #[test]
    fn lists_for_ksp_keep_colons_in_values() {
        let joined = join_ksp_list(&[PathBuf::from("/a"), PathBuf::from("C:/b")]);
        assert_eq!(
            joined,
            OsString::from(format!("/a{KSP_LIST_SEPARATOR}C:/b"))
        );
    }
}
