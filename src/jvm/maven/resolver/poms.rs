// POMs with their parents, properties and imported BOMs, read once per coordinate
use std::path::Path;
use std::rc::Rc;

use crate::common::system;
use crate::jvm::maven::pom::{
    EffectivePom, InheritedPom, PomFile,
    PomProjectInfo, merge_dependencies,
};
use crate::jvm::maven::{
    MAX_DEPTH, MavenCoordinate, PomKey, concrete_version,
};

use super::*;

impl DependencyResolver {
    /// Interpolated POM, missing versions are filled in from <dependencyManagement>
    pub(super) fn effective_pom(
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
    pub(super) fn inherited_pom(
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

    pub(super) fn read_pom_with_parents(
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
}

/// The versions a BOM manages, like Gradle uses them for a platform: entries
/// with a concrete version, without scope and classifier
pub(super) fn managed_versions(bom: &EffectivePom) -> Vec<MavenCoordinate> {
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
