// Gradle module metadata: the variant of a module for the JVM or Android, its dependencies, constraints
// and files
use std::rc::Rc;

use serde_json::Value as Json;

use crate::common::system;
use crate::jvm::maven::pom::PomExclusion;
use crate::jvm::maven::{
    MavenCoordinate, concrete_version,
};

use super::*;

impl DependencyResolver {
    /// Kotlin Multiplatform libraries keep their JVM and Android classes in
    /// separate modules (clikt -> clikt-jvm), Gradle module metadata says which one
    pub(super) fn platform_module(&mut self, coordinate: MavenCoordinate) -> Result<MavenCoordinate, String> {
        let mut current = coordinate;

        // a platform module (datastore-core-jvm) belongs to a root component
        // (datastore-core), and the root decides which variant the target gets
        if let Some(root) = self.root_component(&current)? {
            current = root;
        }

        // a redirect may point to another redirect, but not endlessly
        for _ in 0..4 {
            match self.runtime_variant(&current)?
                .and_then(|variant| variant.available_at.clone())
            {
                Some(target) if target != current => current = target,
                _ => break,
            }
        }

        return Ok(current);
    }

    /// The .module file, like Gradle only read when the POM announces one
    pub(super) fn module_metadata(
        &mut self,
        coordinate: &MavenCoordinate,
    ) -> Result<Option<Rc<Json>>, String> {
        if coordinate.classifier.is_some() {
            return Ok(None);
        }

        let key = coordinate.pom_key();

        if let Some(metadata) = self.module_metadata.get(&key) {
            return Ok(metadata.clone());
        }

        // asked for only when announced: every other one is a request per repository that ends in 404
        let announced = self.inherited_pom(&key, 0)?
            .is_some_and(|model| model.gradle_metadata);

        let file = if announced {
            self.find_in_repositories(&coordinate.module_path())?
        } else {
            None
        };

        let metadata = match file {
            Some(file) => {
                let content = system::read_file(&file)?;
                let metadata: Json = serde_json::from_str(&content).map_err(|error| {
                    format!(
                        "{} is not valid Gradle module metadata: {error}",
                        file.display()
                    )
                })?;
                Some(Rc::new(metadata))
            }
            None => None,
        };

        self.module_metadata.insert(key, metadata.clone());

        return Ok(metadata);
    }

    /// The root component a platform module belongs to, only a real one: with a url and
    /// variants pointing back here (JitPack names an aggregate instead)
    pub(super) fn root_component(
        &mut self,
        coordinate: &MavenCoordinate,
    ) -> Result<Option<MavenCoordinate>, String> {
        let Some(metadata) = self.module_metadata(coordinate)? else {
            return Ok(None);
        };

        let Some(component) = metadata.get("component") else {
            return Ok(None);
        };

        let field = |name: &str| {
            component.get(name)
                .and_then(Json::as_str)
                .map(str::to_string)
        };

        let (Some(_), Some(group), Some(artifact), Some(version)) = (
            field("url"),
            field("group"),
            field("module"),
            field("version"),
        ) else {
            return Ok(None);
        };

        if group == coordinate.group && artifact == coordinate.artifact {
            return Ok(None);
        }

        let root = MavenCoordinate {
            group,
            artifact,
            version,
            classifier: None,
        };

        let Some(root_metadata) = self.module_metadata(&root)? else {
            return Ok(None);
        };

        let points_back = json_array(root_metadata.get("variants")).iter().any(|variant| {
            let at_this_module = variant.get("available-at")
                .and_then(|available| available.get("module"))
                .and_then(Json::as_str)
                == Some(coordinate.artifact.as_str());

            // a variant with capabilities of its own is an optional feature of the
            // root (kotlin-test-junit5 of kotlin-test), not its platform part
            let feature = json_array(variant.get("capabilities")).iter().any(|capability| {
                capability.get("name").and_then(Json::as_str) != Some(root.artifact.as_str())
            });
            at_this_module && !feature
        });

        return Ok(points_back.then_some(root));
    }

    /// For the target platform, like Gradle selects it: the best platform match,
    /// release before debug. None without module metadata, then the POM is used.
    pub(super) fn runtime_variant(
        &mut self,
        coordinate: &MavenCoordinate,
    ) -> Result<Option<Rc<GradleVariant>>, String> {
        let key = coordinate.pom_key();

        if let Some(variant) = self.variants.get(&key) {
            return Ok(variant.clone());
        }

        let variant = match self.module_metadata(coordinate)? {
            Some(metadata) => self.select_runtime_variant(coordinate, &metadata)?
                .map(Rc::new),
            None => None,
        };

        self.variants.insert(key, variant.clone());

        return Ok(variant);
    }

    pub(super) fn select_runtime_variant(
        &mut self,
        coordinate: &MavenCoordinate,
        metadata: &Json,
    ) -> Result<Option<GradleVariant>, String> {
        let attribute = |variant: &Json, name: &str| {
            variant.get("attributes")
                .and_then(|attributes| attributes.get(name))
                .and_then(Json::as_str)
                .map(str::to_string)
        };

        let platforms = self.platform.kotlin_platforms();
        let rank = |variant: &Json| -> Option<(usize, usize, bool)> {
            let runtime = attribute(variant, "org.gradle.usage").as_deref() == Some("java-runtime")
                && attribute(variant, "org.gradle.category")
                    .is_none_or(|category| category == "library");

            if !runtime {
                return None;
            }

            let platform = match attribute(variant, "org.jetbrains.kotlin.platform.type") {
                Some(platform) => platforms.iter().position(|known| *known == platform)?,
                None => platforms.len(),
            };

            let environment = self.platform.jvm_environment_rank(
                attribute(variant, "org.gradle.jvm.environment").as_deref(),
            )?;

            let name = variant.get("name")
                .and_then(Json::as_str)
                .unwrap_or("");
            let debug = attribute(variant, "com.android.build.api.attributes.BuildTypeAttr")
                .as_deref()
                == Some("debug")
                || name.starts_with("debug");

            return Some((platform, environment, debug));
        };

        let variants = json_array(metadata.get("variants"));
        let Some(best) = variants.iter()
            .filter_map(|variant| rank(variant).map(|rank| (rank, variant)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, variant)| variant)
        else {
            return Ok(None);
        };

        if let Some(available) = best.get("available-at") {
            let field = |name: &str| {
                available.get(name)
                    .and_then(Json::as_str)
                    .map(str::to_string)
            };

            let (Some(group), Some(artifact), Some(version)) =
                (field("group"), field("module"), field("version"))
            else {
                return Err(format!(
                    "{coordinate}: incomplete available-at in its .module file"
                ));
            };

            let target = MavenCoordinate {
                group,
                artifact,
                version,
                classifier: None,
            };

            return Ok(Some(GradleVariant {
                available_at: Some(target),
                dependencies: vec![],
                constraints: vec![],
                files: vec![],
            }));
        }

        // requires, else strictly, else prefers
        let version_of = |entry: &Json| -> Option<String> {
            let version = entry.get("version")?;
            ["requires", "strictly", "prefers"].iter()
                .find_map(|constraint_type| {
                    version.get(constraint_type)
                        .and_then(Json::as_str)
                        .map(str::to_string)
                })
        };

        let name_of = |entry: &Json| -> Option<(String, String)> {
            Some((
                entry.get("group")?.as_str()?.to_string(),
                entry.get("module")?.as_str()?.to_string(),
            ))
        };

        let mut dependencies = vec![];
        let mut constraints = vec![];

        for dependency in json_array(best.get("dependencies")) {
            let Some((group, artifact)) = name_of(dependency) else {
                continue;
            };

            // without a version the module gets its version from other edges
            // (kotlin-stdlib in Compose Multiplatform), like in Gradle
            let Some(version) = version_of(dependency) else {
                continue;
            };

            let version = concrete_version(&version)
                .map_err(|error| format!("{coordinate}: {group}:{artifact}: {error}"))?;

            // platform(...) dependencies bring their BOM versions as constraints
            let category = attribute(dependency, "org.gradle.category");
            if matches!(category.as_deref(), Some("platform" | "enforced-platform")) {
                if let Some(bom) =
                    self.effective_pom(&(group.clone(), artifact.clone(), version.clone()), 0)?
                {
                    constraints.extend(managed_versions(&bom));
                }

                continue;
            }

            let exclusions = json_array(dependency.get("excludes"))
                .iter()
                .map(|exclude| PomExclusion {
                    group: exclude.get("group")
                        .and_then(Json::as_str)
                        .unwrap_or("*")
                        .to_string(),
                    artifact: exclude.get("module")
                        .and_then(Json::as_str)
                        .unwrap_or("*")
                        .to_string(),
                })
                .collect();

            dependencies.push((
                MavenCoordinate {
                    group,
                    artifact,
                    version,
                    classifier: None,
                },
                exclusions,
            ));
        }

        for constraint in json_array(best.get("dependencyConstraints")) {
            let (Some((group, artifact)), Some(version)) =
                (name_of(constraint), version_of(constraint))
            else {
                continue;
            };

            if let Ok(version) = concrete_version(&version) {
                constraints.push(MavenCoordinate {
                    group,
                    artifact,
                    version,
                    classifier: None,
                });
            }
        }

        let files = json_array(best.get("files"))
            .iter()
            .filter_map(|file| file.get("url").and_then(Json::as_str).map(str::to_string))
            .collect();

        return Ok(Some(GradleVariant {
            available_at: None,
            dependencies,
            constraints,
            files,
        }));
    }
}

/// The variant of a module Gradle module metadata selects for the target platform
#[derive(Debug)]
pub(super) struct GradleVariant {
    /// the variant lives in another module (clikt -> clikt-jvm)
    pub(super) available_at: Option<MavenCoordinate>,
    pub(super) dependencies: Vec<(MavenCoordinate, Vec<PomExclusion>)>,
    pub(super) constraints: Vec<MavenCoordinate>,
    /// file urls relative to the module's directory
    pub(super) files: Vec<String>,
}

/// The elements of a JSON array, none for anything else
pub(super) fn json_array(value: Option<&Json>) -> &[Json] {
    return value.and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
}
