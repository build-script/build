// Dependency resolver: transitive dependencies via POMs and Gradle module metadata, the highest version wins
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use serde_json::Value as Json;

use crate::common::system;

use super::metadata::{self, MavenMetadata, SnapshotFile};
use super::pom::{
    EffectivePom, InheritedPom, LibraryLicense, LicenseReportEntry, PomExclusion, PomFile,
    PomProjectInfo, interpolate, merge_dependencies,
};
use super::{
    MAX_DEPTH, MavenCoordinate, MavenModuleKey, MavenRepository, PomKey, TargetPlatform,
    compare_versions, concrete_version,
};

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

    /// The POMs of the newest builds of SNAPSHOT versions, and when they were
    /// written. A module that depends on a SNAPSHOT is built again when they change.
    pub fn snapshot_poms(
        &mut self,
        coordinates: &[MavenCoordinate],
    ) -> Result<Vec<(PathBuf, Option<SystemTime>)>, String> {
        let mut poms = vec![];

        for coordinate in coordinates.iter().filter(|coordinate| coordinate.is_snapshot()) {
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

    /// First pass: the highest requested version of every module. Constraints of modules
    /// published by Gradle (kotlin-stdlib, androidx) raise versions of modules in the graph.
    fn select_versions(
        &mut self,
        roots: &[MavenCoordinate],
        initial_constraints: Vec<MavenCoordinate>,
    ) -> Result<HashMap<MavenModuleKey, String>, String> {
        let mut selected: HashMap<MavenModuleKey, String> = HashMap::new();
        let mut expanded: HashSet<MavenCoordinate> = HashSet::new();
        let mut constraints: Vec<MavenCoordinate> = initial_constraints;
        // module as requested -> module after the platform redirect
        let mut redirected: HashMap<MavenModuleKey, MavenModuleKey> = HashMap::new();

        let mut queue: VecDeque<(MavenCoordinate, Vec<PomExclusion>)> =
            roots.iter()
                .map(|root| (root.clone(), vec![]))
                .collect();

        // false if a higher version of the module is already selected
        let select = |selected: &mut HashMap<MavenModuleKey, String>,
                      coordinate: &MavenCoordinate| {
            match selected.get(&coordinate.module_key()) {
                Some(current)
                    if compare_versions(current, &coordinate.version) != Ordering::Less =>
                {
                    return *current == coordinate.version;
                }
                _ => {
                    selected.insert(coordinate.module_key(), coordinate.version.clone());
                    return true;
                }
            }
        };

        loop {
            while let Some((requested, exclusions)) = queue.pop_front() {
                // like Gradle: the version is chosen per requested module, the
                // platform redirect comes after (annotation 1.6 -> annotation-jvm)
                if !select(&mut selected, &requested) {
                    continue;
                }

                let coordinate = self.platform_module(requested.clone())?;

                if coordinate.module_key() != requested.module_key() {
                    redirected.insert(requested.module_key(), coordinate.module_key());
                    // the platform module may also be requested directly
                    if !select(&mut selected, &coordinate) {
                        continue;
                    }
                }

                if expanded.insert(coordinate.clone()) {
                    queue.extend(self.dependencies_of(&coordinate, &exclusions)?);
                    constraints.extend(self.dependency_constraints(&coordinate)?);
                }
            }

            for constraint in &constraints {
                let requested = constraint.module_key();

                let below = |module: &MavenModuleKey| {
                    selected.get(module).is_some_and(|current| {
                        compare_versions(current, &constraint.version) == Ordering::Less
                    })
                };

                if below(&requested) || redirected.get(&requested).is_some_and(below) {
                    queue.push_back((constraint.clone(), vec![]));
                }
            }

            if queue.is_empty() {
                return Ok(selected);
            }
        }
    }

    /// Gradle publishes them as <dependencyManagement> of the POM
    fn dependency_constraints(
        &mut self,
        coordinate: &MavenCoordinate,
    ) -> Result<Vec<MavenCoordinate>, String> {
        if let Some(variant) = self.runtime_variant(coordinate)?
            .filter(|variant| variant.available_at.is_none())
        {
            return Ok(variant.constraints.clone());
        }

        let key = coordinate.pom_key();

        let published_by_gradle = self.inherited_pom(&key, 0)?
            .is_some_and(|model| model.gradle_metadata);

        match self.effective_pom(&key, 0)?.filter(|_| published_by_gradle) {
            Some(pom) => return Ok(managed_versions(&pom)),
            None => return Ok(vec![]),
        }
    }

    /// Second pass: through the selected versions, in breadth-first order
    fn collect_reachable(
        &mut self,
        roots: &[MavenCoordinate],
        selected: &HashMap<MavenModuleKey, String>,
    ) -> Result<Vec<MavenCoordinate>, String> {
        let mut visited: HashSet<MavenModuleKey> = HashSet::new();
        let mut ordered = vec![];

        let mut queue: VecDeque<(MavenCoordinate, Vec<PomExclusion>)> =
            roots.iter()
                .map(|root| (root.clone(), vec![]))
                .collect();

        while let Some((mut requested, exclusions)) = queue.pop_front() {
            if let Some(version) = selected.get(&requested.module_key()) {
                requested.version = version.clone();
            }

            let mut coordinate = self.platform_module(requested)?;
            let module = coordinate.module_key();

            if let Some(version) = selected.get(&module) {
                coordinate.version = version.clone();
            }

            if !visited.insert(module) {
                continue;
            }

            queue.extend(self.dependencies_of(&coordinate, &exclusions)?);
            ordered.push(coordinate);
        }

        return Ok(ordered);
    }

    /// Kotlin Multiplatform libraries keep their JVM and Android classes in
    /// separate modules (clikt -> clikt-jvm), Gradle module metadata says which one
    fn platform_module(&mut self, coordinate: MavenCoordinate) -> Result<MavenCoordinate, String> {
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
    fn module_metadata(
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
    fn root_component(
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
    fn runtime_variant(
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

    fn select_runtime_variant(
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

    /// Those that belong on the classpath
    fn dependencies_of(
        &mut self,
        coordinate: &MavenCoordinate,
        exclusions: &[PomExclusion],
    ) -> Result<Vec<(MavenCoordinate, Vec<PomExclusion>)>, String> {
        // Gradle module metadata knows the dependencies of the selected variant,
        // the POM only those of all platforms together
        if let Some(variant) = self.runtime_variant(coordinate)?
            .filter(|variant| variant.available_at.is_none())
        {
            let mut result = vec![];

            for (dependency, own_exclusions) in &variant.dependencies {
                if exclusions.iter()
                    .any(|exclusion| exclusion.matches(&dependency.group, &dependency.artifact))
                {
                    continue;
                }

                let mut nested = exclusions.to_vec();
                nested.extend(own_exclusions.iter().cloned());
                result.push((dependency.clone(), nested));
            }

            return Ok(result);
        }

        let Some(pom) = self.effective_pom(&coordinate.pom_key(), 0)? else {
            return Ok(vec![]);
        };

        let mut result = vec![];

        for dependency in &pom.dependencies {
            let scope = dependency.scope.as_deref().unwrap_or("compile");
            if dependency.optional || !matches!(scope, "compile" | "runtime") {
                continue;
            }

            if exclusions.iter()
                .any(|exclusion| exclusion.matches(&dependency.group, &dependency.artifact))
            {
                continue;
            }

            let name = format!("{}:{}", dependency.group, dependency.artifact);
            let Some(version) = &dependency.version else {
                return Err(format!("{coordinate}: no version given for {name}"));
            };

            let version = concrete_version(version)
                .map_err(|error| format!("{coordinate}: {name}: {error}"))?;

            let classifier = dependency.classifier.clone()
                .or_else(|| (dependency.artifact_type == "test-jar").then(|| "tests".to_string()));

            let mut nested = exclusions.to_vec();
            nested.extend(dependency.exclusions.iter().cloned());

            result.push((
                MavenCoordinate {
                    group: dependency.group.clone(),
                    artifact: dependency.artifact.clone(),
                    version,
                    classifier,
                },
                nested,
            ));
        }

        return Ok(result);
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

    /// Interpolated POM, missing versions are filled in from <dependencyManagement>
    fn effective_pom(
        &mut self,
        key: &PomKey,
        depth: usize,
    ) -> Result<Option<Rc<EffectivePom>>, String> {
        if let Some(pom) = self.effective_poms.get(key) {
            return Ok(pom.clone());
        }

        if depth > MAX_DEPTH {
            return Err(format!(
                "BOM imports of {}:{}:{} are nested too deeply",
                key.0, key.1, key.2
            ));
        }

        let Some(model) = self.inherited_pom(key, 0)? else {
            self.effective_poms.insert(key.clone(), None);
            return Ok(None);
        };

        let mut managed = vec![];
        let mut imported = vec![];

        for entry in &model.managed {
            let entry = entry.interpolated(&model.properties);

            if entry.scope.as_deref() != Some("import") {
                managed.push(entry);
                continue;
            }

            let Some(version) = entry.version else {
                continue;
            };

            if let Some(bom) =
                self.effective_pom(&(entry.group, entry.artifact, version), depth + 1)?
            {
                imported.extend(bom.managed.iter().cloned());
            }
        }

        // own entries take precedence over imported BOMs
        managed.extend(imported);

        let dependencies = model.dependencies.iter()
            .map(|dependency| {
                let mut dependency = dependency.interpolated(&model.properties);

                if let Some(entry) = managed.iter().find(|entry| entry.same_module(&dependency)) {
                    if dependency.version.is_none() {
                        dependency.version = entry.version.clone();
                    }

                    if dependency.scope.is_none() {
                        dependency.scope = entry.scope.clone();
                    }

                    if dependency.exclusions.is_empty() {
                        dependency.exclusions = entry.exclusions.clone();
                    }
                }
                dependency
            })
            .collect();

        let pom = Rc::new(EffectivePom {
            packaging: model.packaging.clone(),
            dependencies,
            managed,
        });

        self.effective_poms.insert(key.clone(), Some(pom.clone()));

        return Ok(Some(pom));
    }

    /// With its parent chain. None if no repository has it.
    fn inherited_pom(
        &mut self,
        key: &PomKey,
        depth: usize,
    ) -> Result<Option<Rc<InheritedPom>>, String> {
        if let Some(model) = self.inherited_poms.get(key) {
            return Ok(model.clone());
        }

        if depth > MAX_DEPTH {
            return Err(format!(
                "parent chain of {}:{}:{} is too long",
                key.0, key.1, key.2
            ));
        }

        let coordinate = MavenCoordinate::new(&key.0, &key.1, &key.2);

        let model = match self.find_in_repositories(&coordinate.pom_path())? {
            Some(file) => Some(Rc::new(self.read_pom_with_parents(
                &coordinate,
                &file,
                depth,
            )?)),
            None => None,
        };

        self.inherited_poms.insert(key.clone(), model.clone());

        return Ok(model);
    }

    fn read_pom_with_parents(
        &mut self,
        coordinate: &MavenCoordinate,
        file: &Path,
        depth: usize,
    ) -> Result<InheritedPom, String> {
        let content = system::read_file(file)?;

        let project = PomFile::parse(&content)
            .map_err(|error| format!("{} is not a valid POM: {error}", file.display()))?;

        let mut model = InheritedPom::default();

        if let Some(parent) = &project.parent {
            let (Some(group), Some(artifact), Some(version)) = (
                parent.group_id.clone(),
                parent.artifact_id.clone(),
                parent.version.clone(),
            ) else {
                return Err(format!("{}: incomplete <parent>", file.display()));
            };

            if let Some(inherited) = self.inherited_pom(
                &(group.clone(), artifact.clone(), version.clone()),
                depth + 1,
            )? {
                model.properties = inherited.properties.clone();
                model.dependencies = inherited.dependencies.clone();
                model.managed = inherited.managed.clone();
                model.licenses = inherited.licenses.clone();
            }

            for (name, value) in [
                ("groupId", group),
                ("artifactId", artifact),
                ("version", version),
            ] {
                model
                    .properties
                    .insert(format!("project.parent.{name}"), value.clone());
                model.properties.insert(format!("parent.{name}"), value);
            }
        }

        model.properties.extend(
            project.properties.iter()
                .map(|(name, value)| (name.clone(), value.trim().to_string())),
        );

        let own = [
            ("groupId", &coordinate.group),
            ("artifactId", &coordinate.artifact),
            ("version", &coordinate.version),
        ];

        for (name, value) in own {
            for prefix in ["project.", "pom.", ""] {
                model
                    .properties
                    .insert(format!("{prefix}{name}"), value.clone());
            }
        }

        model.packaging = project.packaging.clone()
            .unwrap_or_else(|| "jar".to_string());
        model.gradle_metadata = content.contains("published-with-gradle-metadata");
        merge_dependencies(&mut model.dependencies, project.dependencies());
        merge_dependencies(&mut model.managed, project.managed());

        model.project_info = PomProjectInfo {
            name: project.name.clone(),
            description: project.description.clone(),
            url: project.url.clone(),
            developers: project.developers(),
        };

        let licenses = project.licenses();
        if !licenses.is_empty() {
            model.licenses = licenses;
        }

        return Ok(model);
    }

    /// Remote files end up in the cache. A file of a SNAPSHOT version is the
    /// one of the newest build the repository has.
    fn find_in_repositories(&mut self, path: &str) -> Result<Option<PathBuf>, String> {
        let snapshot = SnapshotFile::parse(path);

        for index in 0..self.repositories.len() {
            let url = match &self.repositories[index] {
                // mvn install keeps SNAPSHOTs under their plain name
                MavenRepository::Local(directory) => {
                    let file = directory.join(path);
                    if file.is_file() {
                        return Ok(Some(file));
                    }
                    continue;
                }
                MavenRepository::Remote { url, .. } => url.clone(),
            };

            let build_path = match &snapshot {
                Some(snapshot) => self.snapshot_build_path(index, snapshot)?,
                None => None,
            };

            // timestamped builds never change, so they are cached like releases
            let path = build_path.as_deref().unwrap_or(path);
            let file = self.cache_dir.join(path);

            if file.is_file() {
                return Ok(Some(file));
            }

            let credentials = self.repositories[index].credentials()?;

            if system::download(&format!("{url}/{path}"), &file, false, credentials.as_ref())? {
                return Ok(Some(file));
            }
        }

        return Ok(None);
    }

    /// Where the newest build of a SNAPSHOT file is in a repository, None when
    /// it has no metadata or keeps the plain name
    fn snapshot_build_path(
        &mut self,
        index: usize,
        snapshot: &SnapshotFile,
    ) -> Result<Option<String>, String> {
        let key = (index, snapshot.directory.to_string());

        if !self.snapshot_metadata.contains_key(&key) {
            let metadata = metadata::fetch(&self.repositories[index], snapshot.directory)?;
            self.snapshot_metadata.insert(key.clone(), metadata.map(Rc::new));
        }

        let build = self.snapshot_metadata[&key].as_ref()
            .and_then(|metadata| metadata.snapshot_build(snapshot));

        return Ok(build.map(|build| snapshot.path_of_build(&build)));
    }
}

/// The variant of a module Gradle module metadata selects for the target platform
#[derive(Debug)]
struct GradleVariant {
    /// the variant lives in another module (clikt -> clikt-jvm)
    available_at: Option<MavenCoordinate>,
    dependencies: Vec<(MavenCoordinate, Vec<PomExclusion>)>,
    constraints: Vec<MavenCoordinate>,
    /// file urls relative to the module's directory
    files: Vec<String>,
}

/// The versions a BOM manages, like Gradle uses them for a platform: entries
/// with a concrete version, without scope and classifier
fn managed_versions(bom: &EffectivePom) -> Vec<MavenCoordinate> {
    return bom.managed.iter()
        .filter(|entry| entry.scope.is_none() && entry.classifier.is_none())
        .filter_map(|entry| {
            let version = concrete_version(entry.version.as_deref()?).ok()?;
            Some(MavenCoordinate::new(
                &entry.group,
                &entry.artifact,
                &version,
            ))
        })
        .collect();
}

/// The elements of a JSON array, none for anything else
fn json_array(value: Option<&Json>) -> &[Json] {
    return value.and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

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
        let repository: PathBuf =
            std::env::temp_dir().join(format!("build-script-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&repository);
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
            vec![maven::MavenRepository::Local(repository.clone())],
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

        fs::remove_dir_all(&repository).unwrap();
    }

    #[test]
    fn license_report_lists_what_the_poms_say() {
        let repository: PathBuf =
            std::env::temp_dir().join(format!("build-script-licenses-{}", std::process::id()));

        let _ = fs::remove_dir_all(&repository);
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
            vec![maven::MavenRepository::Local(repository.clone())],
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

        fs::remove_dir_all(&repository).unwrap();
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
        let repository: PathBuf =
            std::env::temp_dir().join(format!("build-script-kmp-{}", std::process::id()));

        let _ = fs::remove_dir_all(&repository);
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
            vec![maven::MavenRepository::Local(repository.clone())],
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
        fs::remove_dir_all(&repository).unwrap();
    }

    #[test]
    fn platforms_provide_versions_and_constraints() {
        let repository: PathBuf =
            std::env::temp_dir().join(format!("build-script-bom-{}", std::process::id()));

        let _ = fs::remove_dir_all(&repository);
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
            vec![maven::MavenRepository::Local(repository.clone())],
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
        fs::remove_dir_all(&repository).unwrap();
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
