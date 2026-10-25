// POM files: the model after inheriting from the parent, with dependencies, properties and what license reports say
use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::{Value as Json, json};

use super::{MAX_DEPTH, MavenCoordinate};

/// The parts of a POM file the resolver reads, before inheriting from the parent
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PomFile {
    pub parent: Option<PomParent>,
    #[serde(deserialize_with = "trimmed_text")]
    pub packaging: Option<String>,
    pub properties: HashMap<String, String>,
    dependencies: PomDependencyList,
    dependency_management: PomDependencyManagement,
    #[serde(deserialize_with = "trimmed_text")]
    pub name: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    pub description: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    pub url: Option<String>,
    developers: PomDevelopers,
    licenses: PomLicenses,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PomParent {
    #[serde(deserialize_with = "trimmed_text")]
    pub group_id: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    pub artifact_id: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PomDependency {
    pub group: String,
    pub artifact: String,
    pub version: Option<String>,
    pub classifier: Option<String>,
    /// <type>
    pub artifact_type: String,
    pub scope: Option<String>,
    pub optional: bool,
    pub exclusions: Vec<PomExclusion>,
}

#[derive(Debug, Clone)]
pub struct PomExclusion {
    pub group: String,
    pub artifact: String,
}

/// After inheriting from the parent, but before interpolation
#[derive(Debug, Default)]
pub struct InheritedPom {
    pub packaging: String,
    /// the POM has the marker Gradle writes when it also publishes a .module file
    pub gradle_metadata: bool,
    pub properties: HashMap<String, String>,
    pub dependencies: Vec<PomDependency>,
    /// <dependencyManagement>
    pub managed: Vec<PomDependency>,
    /// <name>, <description>, <url> and <developers> of this POM only, like
    /// the Gradle license plugin reads them
    pub project_info: PomProjectInfo,
    /// inherited from the parent when the POM names none
    pub licenses: Vec<LibraryLicense>,
}

#[derive(Debug, Default, Clone)]
pub struct PomProjectInfo {
    pub name: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub developers: Vec<String>,
}

/// Interpolated, with the versions of <dependencyManagement> filled in
#[derive(Debug)]
pub struct EffectivePom {
    pub packaging: String,
    pub dependencies: Vec<PomDependency>,
    pub managed: Vec<PomDependency>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LibraryLicense {
    pub name: String,
    pub url: String,
}

/// What a license report says about a library
#[derive(Debug)]
pub struct LicenseReportEntry {
    pub coordinate: MavenCoordinate,
    /// <name> of the POM, else the artifact id
    pub name: String,
    pub description: Option<String>,
    pub url: Option<String>,
    pub developers: Vec<String>,
    pub licenses: Vec<LibraryLicense>,
}

/// The JSON report of the Gradle license plugin (com.jaredsburrows.license),
/// which apps read from their assets to show third party licenses
pub fn license_report(libraries: &[LicenseReportEntry]) -> Json {
    let entries = libraries.iter().map(|library| {
        json!({
            "project": library.name,
            "description": library.description,
            "version": library.coordinate.version,
            "developers": library.developers,
            "url": library.url,
            "year": null,
            "licenses": library.licenses.iter()
                .map(|license| json!({ "license": license.name, "license_url": license.url }))
                .collect::<Vec<_>>(),
            "dependency": library.coordinate.to_string(),
        })
    });
    return Json::Array(entries.collect());
}

/// Entries of the child replace matching entries of the parent
pub fn merge_dependencies(target: &mut Vec<PomDependency>, entries: Vec<PomDependency>) {
    for entry in entries {
        match target.iter_mut()
            .find(|existing| existing.same_module(&entry))
        {
            Some(existing) => *existing = entry,
            None => target.push(entry),
        }
    }
}

/// Replaces ${property}, unknown properties are kept as they are
pub fn interpolate(value: &str, properties: &HashMap<String, String>, depth: usize) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];

        let Some(end) = after.find('}') else {
            rest = &rest[start..];
            break;
        };

        let key = &after[..end];

        match properties.get(key) {
            Some(replacement) if depth < MAX_DEPTH => {
                out.push_str(&interpolate(replacement, properties, depth + 1))
            }
            _ => out.push_str(&rest[start..start + 2 + end + 1]),
        }

        rest = &after[end + 1..];
    }

    out.push_str(rest);

    return out;
}

impl PomFile {
    pub fn parse(content: &str) -> Result<Self, String> {
        return quick_xml::de::from_str(content).map_err(|error| error.to_string());
    }

    /// `<dependencies>` of the project
    pub fn dependencies(&self) -> Vec<PomDependency> {
        return self.dependencies.dependency.iter()
            .filter_map(PomDependency::from_entry)
            .collect();
    }

    /// `<dependencies>` of `<dependencyManagement>`
    pub fn managed(&self) -> Vec<PomDependency> {
        return self.dependency_management.dependencies.dependency.iter()
            .filter_map(PomDependency::from_entry)
            .collect();
    }

    /// Their names, their ids where they have none
    pub fn developers(&self) -> Vec<String> {
        return self.developers.developer.iter()
            .filter_map(|developer| developer.name.clone().or_else(|| developer.id.clone()))
            .collect();
    }

    pub fn licenses(&self) -> Vec<LibraryLicense> {
        return self.licenses.license.iter()
            .map(|license| LibraryLicense {
                name: license.name.clone().unwrap_or_default(),
                url: license.url.clone().unwrap_or_default(),
            })
            .collect();
    }
}

impl PomDependency {
    pub fn same_module(&self, other: &PomDependency) -> bool {
        return self.group == other.group
            && self.artifact == other.artifact
            && self.classifier == other.classifier
            && self.artifact_type == other.artifact_type;
    }

    pub fn interpolated(&self, properties: &HashMap<String, String>) -> Self {
        let resolve = |value: &str| interpolate(value, properties, 0);
        return PomDependency {
            group: resolve(&self.group),
            artifact: resolve(&self.artifact),
            version: self.version.as_deref().map(resolve),
            classifier: self.classifier.as_deref().map(resolve),
            artifact_type: resolve(&self.artifact_type),
            scope: self.scope.as_deref().map(resolve),
            optional: self.optional,
            exclusions: self.exclusions.clone(),
        };
    }

    /// Entries without group or artifact are left out
    fn from_entry(entry: &PomDependencyEntry) -> Option<Self> {
        let exclusions = entry.exclusions.exclusion.iter()
            .map(|exclusion| PomExclusion {
                group: exclusion.group_id.clone()
                    .unwrap_or_else(|| "*".to_string()),
                artifact: exclusion.artifact_id.clone()
                    .unwrap_or_else(|| "*".to_string()),
            })
            .collect();

        return Some(PomDependency {
            group: entry.group_id.clone()?,
            artifact: entry.artifact_id.clone()?,
            version: entry.version.clone(),
            classifier: entry.classifier.clone(),
            artifact_type: entry.artifact_type.clone()
                .unwrap_or_else(|| "jar".to_string()),
            scope: entry.scope.clone(),
            optional: entry.optional.as_deref() == Some("true"),
            exclusions,
        });
    }
}

impl PomExclusion {
    pub fn matches(&self, group: &str, artifact: &str) -> bool {
        return (self.group == "*" || self.group == group)
            && (self.artifact == "*" || self.artifact == artifact);
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomDependencyManagement {
    dependencies: PomDependencyList,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomDependencyList {
    dependency: Vec<PomDependencyEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PomDependencyEntry {
    #[serde(deserialize_with = "trimmed_text")]
    group_id: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    artifact_id: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    version: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    classifier: Option<String>,
    #[serde(rename = "type", deserialize_with = "trimmed_text")]
    artifact_type: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    scope: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    optional: Option<String>,
    exclusions: PomExclusionList,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomExclusionList {
    exclusion: Vec<PomExclusionEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PomExclusionEntry {
    #[serde(deserialize_with = "trimmed_text")]
    group_id: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    artifact_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomDevelopers {
    developer: Vec<PomDeveloper>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomDeveloper {
    #[serde(deserialize_with = "trimmed_text")]
    id: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomLicenses {
    license: Vec<PomLicense>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PomLicense {
    #[serde(deserialize_with = "trimmed_text")]
    name: Option<String>,
    #[serde(deserialize_with = "trimmed_text")]
    url: Option<String>,
}

/// Trimmed, None for an empty element
fn trimmed_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let text = Option::<String>::deserialize(deserializer)?;
    return Ok(text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty()));
}
