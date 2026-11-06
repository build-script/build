// Dependency resolver: transitive dependencies via POMs and Gradle module metadata, the highest version wins
mod gradle_metadata;
mod graph;
mod poms;
mod repository;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use serde_json::Value as Json;

use crate::common::system;

use super::metadata::MavenMetadata;
use super::pom::{
    EffectivePom, InheritedPom, LibraryLicense, LicenseReportEntry, interpolate,
};
use super::{
    MavenCoordinate, MavenRepository, PomKey, TargetPlatform,
};

use gradle_metadata::GradleVariant;
use poms::managed_versions;

/// Resolves dependencies transitively via POM files. On conflicts the
/// highest version wins, like in Gradle.
pub struct DependencyResolver {
    repositories: Vec<MavenRepository>,
    cache_dir: PathBuf,
    platform: TargetPlatform,
    inherited_poms: HashMap<PomKey, Option<Rc<InheritedPom>>>,
    effective_poms: HashMap<PomKey, Option<Rc<EffectivePom>>>,
    /// parsed .module files, None when a module has none
    module_metadata: HashMap<PomKey, Option<Rc<Json>>>,
    /// selected variants, None when the POM has to do
    variants: HashMap<PomKey, Option<Rc<GradleVariant>>>,
    /// maven-metadata.xml of SNAPSHOT versions by repository and directory, None when it has none
    snapshot_metadata: HashMap<(usize, String), Option<Rc<MavenMetadata>>>,
}

impl DependencyResolver {
    pub fn new(repositories: Vec<MavenRepository>, platform: TargetPlatform) -> Self {
        return DependencyResolver {
            repositories,
            cache_dir: system::cache_dir("maven"),
            platform,
            inherited_poms: HashMap::new(),
            effective_poms: HashMap::new(),
            module_metadata: HashMap::new(),
            variants: HashMap::new(),
            snapshot_metadata: HashMap::new(),
        };
    }

    /// The POMs of the newest builds of the SNAPSHOT versions `snapshots`, and when they
    /// were written. A module that depends on a SNAPSHOT is built again when they change.
    pub fn snapshot_poms(
        &mut self,
        snapshots: &[MavenCoordinate],
    ) -> Result<Vec<(PathBuf, Option<SystemTime>)>, String> {
        let mut poms = vec![];

        for coordinate in snapshots {
            let Some(pom) = self.find_in_repositories(&coordinate.pom_path())? else {
                return Err(format!("{coordinate} was not found in any repository"));
            };

            // mvn install writes a local SNAPSHOT anew under the same name
            let modified = pom.metadata()
                .and_then(|metadata| metadata.modified())
                .ok();

            poms.push((pom, modified));
        }

        return Ok(poms);
    }

    /// Like Gradle platforms: BOM versions fill in roots without a version
    /// and act as constraints for everything else
    pub fn resolve_with_platforms(
        &mut self,
        roots: &[MavenCoordinate],
        platforms: &[MavenCoordinate],
    ) -> Result<Vec<PathBuf>, String> {
        let coordinates = self.resolve_graph(roots, platforms)?;
        return self.artifacts(&coordinates);
    }

    /// The files of resolved coordinates: jars, aars, nothing for BOMs
    pub fn artifacts(&mut self, coordinates: &[MavenCoordinate]) -> Result<Vec<PathBuf>, String> {
        let mut files = vec![];

        for coordinate in coordinates {
            files.extend(self.artifact_files(coordinate)?);
        }

        return Ok(files);
    }

    /// What the POMs of the resolved libraries say, without BOMs, sorted by name like
    /// the Gradle license plugin does it
    pub fn license_entries(
        &mut self,
        roots: &[MavenCoordinate],
        platforms: &[MavenCoordinate],
    ) -> Result<Vec<LicenseReportEntry>, String> {
        let mut libraries = vec![];

        for coordinate in self.resolve_graph(roots, platforms)? {
            if self.artifact_files(&coordinate)?.is_empty() {
                continue;
            }

            let Some(model) = self.inherited_pom(&coordinate.pom_key(), 0)? else {
                continue;
            };

            let text = |value: &Option<String>| {
                value.as_deref()
                    .map(|value| interpolate(value, &model.properties, 0))
            };

            libraries.push(LicenseReportEntry {
                name: text(&model.project_info.name).unwrap_or_else(|| coordinate.artifact.clone()),
                description: text(&model.project_info.description),
                url: text(&model.project_info.url),
                developers: model.project_info.developers.clone(),
                licenses: model.licenses.iter()
                    .map(|license| LibraryLicense {
                        name: interpolate(&license.name, &model.properties, 0),
                        url: interpolate(&license.url, &model.properties, 0),
                    })
                    .collect(),
                coordinate,
            });
        }

        libraries.sort_by_cached_key(|library| {
            (library.name.to_lowercase(), library.coordinate.to_string())
        });

        return Ok(libraries);
    }

    /// The coordinates of the roots and all transitive dependencies, with the
    /// selected versions and platform modules
    pub fn resolve_graph(
        &mut self,
        roots: &[MavenCoordinate],
        platforms: &[MavenCoordinate],
    ) -> Result<Vec<MavenCoordinate>, String> {
        let mut managed: Vec<MavenCoordinate> = vec![];

        for platform in platforms {
            let Some(bom) = self.effective_pom(&platform.pom_key(), 0)? else {
                return Err(format!("the platform {platform} was not found"));
            };

            managed.extend(managed_versions(&bom));
        }

        let roots = roots.iter()
            .map(|root| {
                if !root.version.is_empty() {
                    return Ok(root.clone());
                }
                managed.iter()
                    .find(|entry| entry.group == root.group && entry.artifact == root.artifact)
                    .map(|entry| MavenCoordinate {
                        version: entry.version.clone(),
                        ..root.clone()
                    })
                    .ok_or_else(|| {
                        format!("{root} has no version, and no platform(...) provides one")
                    })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let roots = roots.as_slice();

        let selected = self.select_versions(roots, managed)?;

        return self.collect_reachable(roots, &selected);
    }

    /// The jar, or the aar of an Android library; for variants the files they name
    fn artifact_files(&mut self, coordinate: &MavenCoordinate) -> Result<Vec<PathBuf>, String> {
        if let Some(variant) = self.runtime_variant(coordinate)?
            .filter(|variant| variant.available_at.is_none())
        {
            let mut files = vec![];

            for url in &variant.files {
                let path = coordinate.variant_file_path(url)
                    .ok_or_else(|| format!("{coordinate}: invalid file url {url}"))?;

                let file = self.find_in_repositories(&path)?.ok_or_else(|| {
                    format!("{coordinate}: {url} was not found in any repository")
                })?;

                files.push(file);
            }

            return Ok(files);
        }

        let mut extensions = vec!["jar"];

        if coordinate.classifier.is_none() {
            if let Some(pom) = self.effective_pom(&coordinate.pom_key(), 0)? {
                // pom-only artifacts (BOMs, aggregators) have no jar
                if pom.packaging == "pom" {
                    return Ok(vec![]);
                }

                if pom.packaging == "aar" {
                    extensions = vec!["aar"];
                }
            }
        }

        if self.platform == TargetPlatform::Android && !extensions.contains(&"aar") {
            extensions.push("aar");
        }

        for extension in extensions {
            if let Some(file) = self.find_in_repositories(&coordinate.artifact_path(extension))? {
                return Ok(vec![file]);
            }
        }

        let repositories: Vec<String> = self.repositories.iter()
            .map(|repository| repository.to_string())
            .collect();

        return Err(format!(
            "{coordinate} was not found in any repository ({})",
            repositories.join(", ")
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use crate::common::system;
    use crate::common::test_support::{ScratchDir, serve_files};
    use crate::jvm::maven;
    use crate::jvm::module::DEFAULT_KOTLIN_VERSION;

    fn write(repository: &Path, coordinate: &str, pom_body: &str) {
        let parts: Vec<&str> = coordinate.split(':').collect();
        let (group, artifact, version) = (parts[0], parts[1], parts[2]);
        let directory = repository.join(group.replace('.', "/"))
            .join(artifact)
            .join(version);

        fs::create_dir_all(&directory).unwrap();

        fs::write(
            directory.join(format!("{artifact}-{version}.pom")),
            format!("<project><groupId>{group}</groupId><artifactId>{artifact}</artifactId><version>{version}</version>{pom_body}</project>"),
        ).unwrap();
        fs::write(directory.join(format!("{artifact}-{version}.jar")), "").unwrap();
    }

    fn dependency(group: &str, artifact: &str, extra: &str) -> String {
        return format!(
            "<dependency><groupId>{group}</groupId><artifactId>{artifact}</artifactId>{extra}</dependency>"
        );
    }

    #[test]
    fn resolver_follows_poms_transitively() {
        let repository = ScratchDir::new("test");
        let e = "com.example";

        // parent with a property, dependencyManagement and an imported BOM
        write(
            &repository,
            "com.example:parent:1",
            &format!(
                "<packaging>pom</packaging>
             <properties><lib.version>2.0</lib.version></properties>
             <dependencyManagement><dependencies>{}{}</dependencies></dependencyManagement>",
                dependency(e, "lib-a", "<version>${lib.version}</version>"),
                dependency(
                    e,
                    "bom",
                    "<version>1</version><type>pom</type><scope>import</scope>"
                ),
            ),
        );
        write(
            &repository,
            "com.example:bom:1",
            &format!(
                "<packaging>pom</packaging><dependencyManagement><dependencies>{}</dependencies></dependencyManagement>",
                dependency(e, "managed", "<version>3.0</version>"),
            ),
        );
        write(&repository, "com.example:app:1.0", &format!(
            "<parent><groupId>{e}</groupId><artifactId>parent</artifactId><version>1</version></parent>
             <dependencies>{}{}{}{}{}</dependencies>",
            dependency(e, "lib-a", ""),
            dependency(e, "lib-b", "<version>1.0</version><exclusions><exclusion><groupId>com.example</groupId><artifactId>excluded</artifactId></exclusion></exclusions>"),
            dependency(e, "managed", ""),
            dependency("junit", "junit", "<version>4.13</version><scope>test</scope>"),
            dependency(e, "optional", "<version>1.0</version><optional>true</optional>"),
        ));
        write(
            &repository,
            "com.example:lib-a:2.0",
            &format!(
                "<dependencies>{}</dependencies>",
                dependency(e, "shared", "<version>1.0</version>"),
            ),
        );
        write(
            &repository,
            "com.example:lib-b:1.0",
            &format!(
                "<dependencies>{}{}</dependencies>",
                dependency(e, "shared", "<version>2.0</version><scope>runtime</scope>"),
                dependency(e, "excluded", "<version>1.0</version>"),
            ),
        );
        // only used by the losing version 1.0
        write(
            &repository,
            "com.example:shared:1.0",
            &format!(
                "<dependencies>{}</dependencies>",
                dependency(e, "old-only", "<version>1.0</version>"),
            ),
        );

        for coordinate in [
            "com.example:shared:2.0",
            "com.example:managed:3.0",
            "com.example:excluded:1.0",
            "com.example:old-only:1.0",
        ] {
            write(&repository, coordinate, "");
        }

        let mut resolver = maven::DependencyResolver::new(
            vec![maven::MavenRepository::Local(repository.to_path_buf())],
            maven::TargetPlatform::Jvm,
        );

        let files = resolver.resolve_with_platforms(
            &[maven::MavenCoordinate::parse("com.example:app:1.0").unwrap()],
            &[],
        )
            .unwrap();

        let mut names: Vec<String> = files.iter()
            .map(|file| file.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        names.sort();

        assert_eq!(
            names,
            [
                "app-1.0.jar",
                "lib-a-2.0.jar",
                "lib-b-1.0.jar",
                "managed-3.0.jar",
                "shared-2.0.jar"
            ]
        );

        let missing = resolver.resolve_with_platforms(
            &[maven::MavenCoordinate::parse("com.example:nope:1.0").unwrap()],
            &[],
        );

        assert!(missing.unwrap_err().contains("com.example:nope:1.0"));

    }

    #[test]
    fn license_report_lists_what_the_poms_say() {
        let repository = ScratchDir::new("licenses");
        let e = "com.example";

        // licenses come from the parent, name and url only from the POM itself
        write(&repository, "com.example:parent:1", "<packaging>pom</packaging><url>https://parent.example</url>
             <licenses><license><name>Apache-2.0</name><url>https://www.apache.org/licenses/LICENSE-2.0.txt</url></license></licenses>");
        write(&repository, "com.example:zeta:1.0", &format!(
            "<parent><groupId>{e}</groupId><artifactId>parent</artifactId><version>1</version></parent>
             <name>Zeta ${{project.version}}</name><description>Says \"hi\"</description>
             <developers><developer><id>jd</id><name>Jane Doe</name></developer><developer><id>only-id</id></developer></developers>
             <dependencies>{}{}</dependencies>",
            dependency(e, "alpha", "<version>2.0</version>"),
            dependency(e, "bom", "<version>1</version><type>pom</type>"),
        ));
        write(
            &repository,
            "com.example:alpha:2.0",
            "<licenses><license><name>MIT</name><url>https://opensource.org/licenses/MIT</url></license></licenses>",
        );
        write(
            &repository,
            "com.example:bom:1",
            "<packaging>pom</packaging>",
        );

        let mut resolver = maven::DependencyResolver::new(
            vec![maven::MavenRepository::Local(repository.to_path_buf())],
            maven::TargetPlatform::Jvm,
        );

        let libraries = resolver.license_entries(
            &[maven::MavenCoordinate::parse("com.example:zeta:1.0").unwrap()],
            &[],
        )
            .unwrap();

        assert_eq!(
            maven::license_report(&libraries).to_string(),
            concat!(
                r#"[{"project":"alpha","description":null,"version":"2.0","developers":[],"url":null,"year":null,"#,
                r#""licenses":[{"license":"MIT","license_url":"https://opensource.org/licenses/MIT"}],"dependency":"com.example:alpha:2.0"},"#,
                r#"{"project":"Zeta 1.0","description":"Says \"hi\"","version":"1.0","developers":["Jane Doe","only-id"],"url":null,"year":null,"#,
                r#""licenses":[{"license":"Apache-2.0","license_url":"https://www.apache.org/licenses/LICENSE-2.0.txt"}],"dependency":"com.example:zeta:1.0"}]"#,
            ),
        );

    }

    #[test]
    fn variant_files_stay_in_the_directory_of_the_artifact() {
        let guava = maven::MavenCoordinate::parse("com.google.guava:guava:33.6.0-jre").unwrap();
        assert_eq!(
            guava.variant_file_path("guava-33.6.0-jre.jar").as_deref(),
            Some("com/google/guava/guava/33.6.0-jre/guava-33.6.0-jre.jar")
        );
        assert_eq!(
            guava
                .variant_file_path("../33.6.0-android/guava-33.6.0-android.jar")
                .as_deref(),
            Some("com/google/guava/guava/33.6.0-android/guava-33.6.0-android.jar"),
        );
        assert_eq!(
            guava.variant_file_path("../../failureaccess/1.0/failureaccess-1.0.jar"),
            None
        );
        assert_eq!(guava.variant_file_path("/etc/passwd"), None);
        assert_eq!(guava.variant_file_path("nested/guava.jar"), None);
    }

    #[test]
    fn resolver_follows_gradle_module_metadata_to_jvm_variants() {
        let repository = ScratchDir::new("kmp");
        let e = "com.example";
        let marker = "<!-- do_not_remove: published-with-gradle-metadata -->";

        // the root module of a multiplatform library only has metadata
        write(
            &repository,
            "com.example:kmp:1.0",
            &format!(
                "{marker}<dependencies>{}</dependencies>",
                dependency(e, "common-only", "<version>1.0</version>")
            ),
        );
        fs::write(repository.join("com/example/kmp/1.0/kmp-1.0.module"), r#"{"variants": [
            {"name": "metadataApiElements", "attributes": {"org.gradle.usage": "kotlin-metadata", "org.jetbrains.kotlin.platform.type": "common"}},
            {"name": "jsRuntimeElements", "attributes": {"org.gradle.usage": "kotlin-runtime", "org.jetbrains.kotlin.platform.type": "js"},
             "available-at": {"group": "com.example", "module": "kmp-js", "version": "1.0"}},
            {"name": "jvmRuntimeElements", "attributes": {"org.gradle.category": "library", "org.gradle.usage": "java-runtime", "org.jetbrains.kotlin.platform.type": "jvm"},
             "available-at": {"group": "com.example", "module": "kmp-jvm", "version": "1.0"}}
        ]}"#).unwrap();
        write(
            &repository,
            "com.example:kmp-jvm:1.0",
            &format!(
                "{marker}<dependencies>{}</dependencies>",
                dependency(e, "shared", "<version>2.0</version>")
            ),
        );
        // a platform module names its root component, requesting it directly ends at the same variant
        fs::write(repository.join("com/example/kmp-jvm/1.0/kmp-jvm-1.0.module"),
            r#"{"component": {"url": "../../kmp/1.0/kmp-1.0.module", "group": "com.example", "module": "kmp", "version": "1.0"}, "variants": []}"#).unwrap();
        write(
            &repository,
            "com.example:direct-user:1.0",
            &format!(
                "<dependencies>{}</dependencies>",
                dependency(e, "kmp-jvm", "<version>1.0</version>")
            ),
        );

        // an older, plain version of the same library loses against the multiplatform one
        write(&repository, "com.example:kmp:0.9", "");
        write(
            &repository,
            "com.example:old-user:1.0",
            &format!(
                "<dependencies>{}</dependencies>",
                dependency(e, "kmp", "<version>0.9</version>")
            ),
        );

        // JitPack names an aggregate as component, without url: not a root to follow
        write(&repository, "com.example:jitpack-core:1.0", marker);
        fs::write(repository.join("com/example/jitpack-core/1.0/jitpack-core-1.0.module"),
            r#"{"component": {"group": "com.example", "module": "aggregate", "version": "1.0"}, "variants": []}"#).unwrap();
        write(&repository, "com.example:aggregate:1.0", "");

        // a feature of the root with a capability of its own (kotlin-test-junit5 of
        // kotlin-test) keeps its own variant when requested directly
        write(&repository, "com.example:kt-test:1.0", marker);
        fs::write(repository.join("com/example/kt-test/1.0/kt-test-1.0.module"), r#"{"variants": [
            {"name": "jvmRuntimeElements", "attributes": {"org.gradle.category": "library", "org.gradle.usage": "java-runtime", "org.jetbrains.kotlin.platform.type": "jvm"},
             "files": [{"url": "kt-test-1.0.jar"}]},
            {"name": "jvmJUnit5RuntimeElements", "attributes": {"org.gradle.category": "library", "org.gradle.usage": "java-runtime", "org.jetbrains.kotlin.platform.type": "jvm"},
             "available-at": {"group": "com.example", "module": "kt-test-junit5", "version": "1.0"},
             "capabilities": [{"group": "com.example", "name": "kt-test-framework-junit5", "version": "1.0"}]}
        ]}"#).unwrap();
        write(&repository, "com.example:kt-test-junit5:1.0", marker);
        fs::write(repository.join("com/example/kt-test-junit5/1.0/kt-test-junit5-1.0.module"), r#"{
            "component": {"url": "../../kt-test/1.0/kt-test-1.0.module", "group": "com.example", "module": "kt-test", "version": "1.0"},
            "variants": [{"name": "jvmJUnit5RuntimeElements", "attributes": {"org.gradle.category": "library", "org.gradle.usage": "java-runtime", "org.jetbrains.kotlin.platform.type": "jvm"},
             "files": [{"url": "kt-test-junit5-1.0.jar"}]}]
        }"#).unwrap();

        // AndroidX tells its Android variant apart by the JVM environment, not the platform type
        write(&repository, "com.example:androidx:1.0", marker);
        fs::write(repository.join("com/example/androidx/1.0/androidx-1.0.module"), r#"{"variants": [
            {"name": "androidRuntimeElements-published", "attributes": {"org.gradle.category": "library", "org.gradle.jvm.environment": "android", "org.gradle.usage": "java-runtime", "org.jetbrains.kotlin.platform.type": "jvm"},
             "available-at": {"group": "com.example", "module": "androidx-android", "version": "1.0"}},
            {"name": "jvmRuntimeElements-published", "attributes": {"org.gradle.category": "library", "org.gradle.jvm.environment": "standard-jvm", "org.gradle.usage": "java-runtime", "org.jetbrains.kotlin.platform.type": "jvm"},
             "available-at": {"group": "com.example", "module": "androidx-jvm", "version": "1.0"}}
        ]}"#).unwrap();
        write(&repository, "com.example:androidx-jvm:1.0", "");

        // without the marker a .module file is ignored, like in Gradle
        write(&repository, "com.example:plain:1.0", "");
        fs::write(
            repository.join("com/example/plain/1.0/plain-1.0.module"),
            "not even json",
        )
        .unwrap();

        for coordinate in ["com.example:common-only:1.0", "com.example:shared:2.0"] {
            write(&repository, coordinate, "");
        }

        let mut resolver = maven::DependencyResolver::new(
            vec![maven::MavenRepository::Local(repository.to_path_buf())],
            maven::TargetPlatform::Jvm,
        );

        let roots = [
            maven::MavenCoordinate::parse("com.example:old-user:1.0").unwrap(),
            maven::MavenCoordinate::parse("com.example:kmp:1.0").unwrap(),
            maven::MavenCoordinate::parse("com.example:direct-user:1.0").unwrap(),
            maven::MavenCoordinate::parse("com.example:jitpack-core:1.0").unwrap(),
            maven::MavenCoordinate::parse("com.example:plain:1.0").unwrap(),
            maven::MavenCoordinate::parse("com.example:kt-test-junit5:1.0").unwrap(),
            maven::MavenCoordinate::parse("com.example:androidx:1.0").unwrap(),
        ];

        let mut names: Vec<String> = resolver.resolve_with_platforms(&roots, &[])
            .unwrap()
            .iter()
            .map(|file| file.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        names.sort();

        assert_eq!(
            names,
            [
                "androidx-jvm-1.0.jar",
                "direct-user-1.0.jar",
                "jitpack-core-1.0.jar",
                "kmp-jvm-1.0.jar",
                "kt-test-junit5-1.0.jar",
                "old-user-1.0.jar",
                "plain-1.0.jar",
                "shared-2.0.jar",
            ]
        );
    }

    #[test]
    fn platforms_provide_versions_and_constraints() {
        let repository = ScratchDir::new("bom");
        let e = "com.example";

        write(
            &repository,
            "com.example:bom:2.0",
            &format!(
                "<packaging>pom</packaging><dependencyManagement><dependencies>{}{}{}</dependencies></dependencyManagement>",
                dependency(e, "ui", "<version>1.5</version>"),
                dependency(e, "runtime", "<version>1.5</version>"),
                dependency(e, "unused", "<version>9.0</version>"),
            ),
        );
        // ui wants an older runtime, the platform raises it
        write(
            &repository,
            "com.example:ui:1.5",
            &format!(
                "<dependencies>{}</dependencies>",
                dependency(e, "runtime", "<version>1.0</version>")
            ),
        );

        for coordinate in [
            "com.example:runtime:1.0",
            "com.example:runtime:1.5",
            "com.example:unused:9.0",
        ] {
            write(&repository, coordinate, "");
        }

        let mut resolver = maven::DependencyResolver::new(
            vec![maven::MavenRepository::Local(repository.to_path_buf())],
            maven::TargetPlatform::Jvm,
        );

        let ui = maven::MavenCoordinate::parse("com.example:ui").unwrap();
        assert_eq!(ui.to_string(), "com.example:ui");

        let platform =
            match maven::DependencyNotation::parse("platform(com.example:bom:2.0)").unwrap() {
                maven::DependencyNotation::Platform(platform) => platform,
                other => panic!("{other:?}"),
            };

        let mut names: Vec<String> = resolver.resolve_with_platforms(&[ui.clone()], &[platform])
            .unwrap()
            .iter()
            .map(|file| file.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        names.sort();
        assert_eq!(names, ["runtime-1.5.jar", "ui-1.5.jar"]);

        let error = resolver.resolve_with_platforms(&[ui], &[]).unwrap_err();
        assert!(error.contains("com.example:ui has no version"), "{error}");
    }

    #[test]
    fn downloads_that_do_not_match_their_checksum_stay_out_of_the_cache() {
        let directory = ScratchDir::new("checksums");

        fs::create_dir_all(&directory).unwrap();

        let sha1 = |content: &str| {
            let file = directory.join("content");
            fs::write(&file, content).unwrap();
            return system::sha(&file, 1).unwrap();
        };

        let lib = "/com/example/lib/1";
        let url = serve_files(vec![
            (format!("{lib}/good.jar"), "good".to_string()),
            (format!("{lib}/good.jar.sha1"), format!("{}  good.jar\n", sha1("good"))),
            (format!("{lib}/upper.jar"), "upper".to_string()),
            (format!("{lib}/upper.jar.sha1"), format!("SHA1(upper.jar)= {}", sha1("upper").to_uppercase())),
            (format!("{lib}/plain.jar"), "plain".to_string()),
            // a proxy that answers with a page of its own
            (format!("{lib}/bad.jar"), "<html>blocked</html>".to_string()),
            (format!("{lib}/bad.jar.sha1"), sha1("bad")),
        ]);

        let repositories = vec![maven::MavenRepository::Remote { url, credentials: None }];
        let mut resolver = maven::DependencyResolver::new(repositories, maven::TargetPlatform::Jvm);
        resolver.cache_dir = directory.join("cache");

        for (name, content) in [("good.jar", "good"), ("upper.jar", "upper"), ("plain.jar", "plain")] {
            let file = resolver.find_in_repositories(&format!("{}/{name}", &lib[1..]))
                .unwrap()
                .unwrap();

            assert_eq!(fs::read_to_string(file).unwrap(), content);
        }

        let error = resolver.find_in_repositories(&format!("{}/bad.jar", &lib[1..])).unwrap_err();
        assert!(error.contains("bad.jar does not match its checksum"), "{error}");

        // neither the file nor a partial one
        let cached = fs::read_dir(directory.join("cache").join(&lib[1..]))
            .unwrap()
            .count();
        assert_eq!(cached, 3);

    }

    /// Needs network access: cargo test -- --ignored
    #[test]
    #[ignore]
    fn resolves_kotlin_compiler_from_maven_central() {
        let repositories = vec![maven::MavenRepository::Remote {
            url: maven::MAVEN_CENTRAL.to_string(),
            credentials: None,
        }];

        let mut resolver = maven::DependencyResolver::new(repositories, maven::TargetPlatform::Jvm);

        let compiler = maven::MavenCoordinate::new(
            "org.jetbrains.kotlin",
            "kotlin-compiler-embeddable",
            DEFAULT_KOTLIN_VERSION,
        );

        let files = resolver.resolve_with_platforms(&[compiler], &[]).unwrap();

        let names: Vec<String> = files.iter()
            .map(|file| file.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        for expected in [
            "kotlin-compiler-embeddable-",
            "kotlin-stdlib-",
            "kotlin-daemon-embeddable-",
            "kotlinx-coroutines-core-jvm-",
        ] {
            assert!(
                names.iter().any(|name| name.starts_with(expected)),
                "{expected} missing in {names:?}"
            );
        }

        // gson inherits versions from its parent, coroutines pulls in an older kotlin-stdlib
        let roots = [
            maven::MavenCoordinate::parse("com.google.code.gson:gson:2.11.0").unwrap(),
            maven::MavenCoordinate::parse(
                "org.jetbrains.kotlinx:kotlinx-coroutines-core-jvm:1.10.2",
            )
            .unwrap(),
            maven::MavenCoordinate::new(
                "org.jetbrains.kotlin",
                "kotlin-stdlib",
                DEFAULT_KOTLIN_VERSION,
            ),
        ];

        let files = resolver.resolve_with_platforms(&roots, &[]).unwrap();
        let stdlibs: Vec<_> = files.iter()
            .filter(|file| {
                file.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("kotlin-stdlib-")
            })
            .collect();

        assert_eq!(stdlibs.len(), 1, "{files:?}");
        assert!(stdlibs[0].ends_with(format!("kotlin-stdlib-{}.jar", DEFAULT_KOTLIN_VERSION)));
        println!("{files:#?}");
    }
}
