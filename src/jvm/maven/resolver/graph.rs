// The dependency graph: the highest requested version wins, constraints raise versions, exclusions
// and redirected modules count, like Gradle
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::jvm::maven::pom::PomExclusion;
use crate::jvm::maven::{
    MavenCoordinate, MavenModuleKey,
    compare_versions, concrete_version,
};

use super::*;

impl DependencyResolver {
    /// First pass: the highest requested version of every module. Constraints of modules
    /// published by Gradle (kotlin-stdlib, androidx) raise versions of modules in the graph.
    pub(super) fn select_versions(
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
    pub(super) fn dependency_constraints(
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
    pub(super) fn collect_reachable(
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

    /// Those that belong on the classpath
    pub(super) fn dependencies_of(
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
}
