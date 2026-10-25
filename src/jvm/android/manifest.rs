// Manifests: read with placeholders, merged with those of libraries like the manifest merger
use std::path::Path;

use crate::common::system;
use crate::common::xml::{self, XmlElement};

/// manifest elements whose android:name is a class, relative to the namespace
const COMPONENTS: &[&str] = &[
    "application",
    "activity",
    "activity-alias",
    "service",
    "receiver",
    "provider",
];

/// The activity with a MAIN/LAUNCHER intent filter, as full class name
pub fn launcher_activity(manifest: &XmlElement, namespace: &str) -> Option<String> {
    let application = manifest.child("application")?;

    let has = |filter: &XmlElement, tag: &str, name: &str| {
        filter.children(tag)
            .any(|entry| entry.attribute("android:name") == Some(name))
    };

    let activity = application.children.iter()
        .filter(|child| child.name == "activity" || child.name == "activity-alias")
        .find(|activity| {
            activity.children("intent-filter").any(|filter| {
                has(filter, "action", "android.intent.action.MAIN")
                    && has(filter, "category", "android.intent.category.LAUNCHER")
            })
        })?;

    let name = activity.attribute("android:name")?;

    if name.starts_with('.') {
        return Some(format!("{namespace}{name}"));
    }

    if !name.contains('.') {
        return Some(format!("{namespace}.{name}"));
    }

    return Some(name.to_string());
}

/// With ${applicationId} and the manifest placeholders replaced
pub fn read_manifest_with_placeholders(
    path: &Path,
    application_id: &str,
    placeholders: &[(String, String)],
) -> Result<XmlElement, String> {
    let content = system::read_file(path)?;
    let mut content = content.replace("${applicationId}", application_id);

    for (name, value) in placeholders {
        content = content.replace(&format!("${{{name}}}"), value);
    }

    return xml::parse(&content).map_err(|error| format!("{}: {error}", path.display()));
}

/// `.MainActivity` -> `com.example.MainActivity`
pub fn expand_class_names(element: &mut XmlElement, namespace: &str) {
    let is_component = COMPONENTS.contains(&element.name.as_str());

    for (key, value) in &mut element.attributes {
        let class_attribute = (key == "android:name" && is_component)
            || key == "android:targetActivity"
            || key == "android:parentActivityName";

        if class_attribute && value.starts_with('.') {
            *value = format!("{namespace}{value}");
        }
    }

    for child in &mut element.children {
        expand_class_names(child, namespace);
    }
}

/// Simplified manifest merger of the Android Gradle plugin: library entries are
/// added, entries with the same name are combined, the app always wins
pub fn merge_manifest(app: &mut XmlElement, library: &XmlElement) {
    for child in &library.children {
        match child.name.as_str() {
            "uses-sdk" => {}
            "application" => {
                if app.child("application").is_none() {
                    app.children.push(XmlElement {
                        name: "application".to_string(),
                        ..XmlElement::default()
                    });
                }

                let application = app.children.iter_mut()
                    .find(|child| child.name == "application")
                    .unwrap();

                merge_element(application, child);
            }
            _ => merge_child(app, child),
        }
    }
}

/// Android only accepts an <activity-alias> after its target activity, the
/// Gradle plugin's manifest merger orders them like that too
pub fn move_aliases_last(manifest: &mut XmlElement) {
    let Some(application) = manifest.children.iter_mut()
        .find(|child| child.name == "application")
    else {
        return;
    };

    let (aliases, others): (Vec<XmlElement>, Vec<XmlElement>) =
        std::mem::take(&mut application.children)
            .into_iter()
            .partition(|child| child.name == "activity-alias");

    application.children = others;
    application.children.extend(aliases);
}

/// tools:node="remove", they do not end up in the apk
pub fn remove_entries_marked_for_removal(element: &mut XmlElement) {
    element
        .children
        .retain(|child| child.attribute("tools:node") != Some("remove"));
    for child in &mut element.children {
        remove_entries_marked_for_removal(child);
    }
}

fn merge_child(parent: &mut XmlElement, child: &XmlElement) {
    // tag and android:name identify an entry
    let same_entry = |existing: &&mut XmlElement| {
        existing.name == child.name
            && existing.attribute("android:name") == child.attribute("android:name")
    };

    match parent.children.iter_mut().find(same_entry) {
        // the app removes this entry on purpose
        Some(existing) if existing.attribute("tools:node") == Some("remove") => {}
        // unnamed entries like <intent-filter> are only added when new
        Some(existing)
            if child.attribute("android:name").is_none()
                && existing.name != "queries"
                && existing.name != "application" =>
        {
            if !same_content(existing, child) {
                parent.children.push(child.clone());
            }
        }
        Some(existing) => merge_element(existing, child),
        None => parent.children.push(child.clone()),
    }
}

fn merge_element(target: &mut XmlElement, source: &XmlElement) {
    for (key, value) in &source.attributes {
        if target.attribute(key).is_none() && !key.starts_with("tools:") {
            target.attributes.push((key.clone(), value.clone()));
        }
    }

    for child in &source.children {
        merge_child(target, child);
    }
}

/// Like they would be written: names, attributes, trimmed text and children
fn same_content(left: &XmlElement, right: &XmlElement) -> bool {
    return left.name == right.name
        && left.attributes == right.attributes
        && left.text.trim() == right.text.trim()
        && left.children.len() == right.children.len()
        && left.children.iter()
            .zip(&right.children)
            .all(|(left, right)| same_content(left, right));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_activity_is_found_in_the_manifest() {
        let manifest = |name: &str| {
            xml::parse(&format!(
                r#"<manifest><application>
            <activity android:name=".Settings"/>
            <activity android:name="{name}">
                <intent-filter>
                    <action android:name="android.intent.action.MAIN"/>
                    <category android:name="android.intent.category.LAUNCHER"/>
                </intent-filter>
            </activity>
        </application></manifest>"#
            ))
            .unwrap()
        };

        assert_eq!(
            launcher_activity(&manifest(".MainActivity"), "com.app").as_deref(),
            Some("com.app.MainActivity")
        );
        assert_eq!(
            launcher_activity(&manifest("MainActivity"), "com.app").as_deref(),
            Some("com.app.MainActivity")
        );
        assert_eq!(
            launcher_activity(&manifest("other.Start"), "com.app").as_deref(),
            Some("other.Start")
        );

        let without = xml::parse(
            r#"<manifest><application><activity android:name=".A"/></application></manifest>"#,
        )
        .unwrap();

        assert_eq!(launcher_activity(&without, "com.app"), None);
    }

    #[test]
    fn library_manifests_are_merged_into_the_app() {
        let mut app = xml::parse(r#"<manifest package="com.app">
            <application android:label="App">
                <activity android:name=".Main"/>
                <provider android:name="androidx.startup.InitializationProvider" android:authorities="x">
                    <meta-data android:name="app.Init"/>
                </provider>
                <receiver android:name="lib.Unwanted" tools:node="remove"/>
            </application>
        </manifest>"#).unwrap();

        let mut library = xml::parse(r#"<manifest package="com.lib">
            <uses-sdk android:minSdkVersion="21"/>
            <uses-permission android:name="android.permission.INTERNET"/>
            <application android:appComponentFactory="lib.Factory" android:label="Lib">
                <activity android:name=".LibActivity"/>
                <provider android:name="androidx.startup.InitializationProvider" android:authorities="x">
                    <meta-data android:name="lib.Init"/>
                </provider>
                <receiver android:name="lib.Unwanted"/>
            </application>
        </manifest>"#).unwrap();

        expand_class_names(&mut library, "com.lib");
        merge_manifest(&mut app, &library);
        merge_manifest(&mut app, &library);
        remove_entries_marked_for_removal(&mut app);

        let mut aliased = xml::parse(
            r#"<manifest><application>
            <activity-alias android:name=".Launcher" android:targetActivity=".Main"/>
            <activity android:name=".Main"/>
        </application></manifest>"#,
        )
        .unwrap();

        move_aliases_last(&mut aliased);
        let order: Vec<&str> = aliased.child("application")
            .unwrap().children.iter()
            .map(|child| child.name.as_str())
            .collect();

        assert_eq!(order, ["activity", "activity-alias"]);

        let application = app.child("application").unwrap();

        let names: Vec<&str> = application.children.iter()
            .filter_map(|child| child.attribute("android:name"))
            .collect();

        assert_eq!(
            names,
            [
                ".Main",
                "androidx.startup.InitializationProvider",
                "com.lib.LibActivity"
            ]
        );
        assert_eq!(application.attribute("android:label"), Some("App"));
        assert_eq!(
            application.attribute("android:appComponentFactory"),
            Some("lib.Factory")
        );

        let provider = application.children("provider")
            .next()
            .unwrap();
        let initializers: Vec<&str> = provider.children.iter()
            .filter_map(|child| child.attribute("android:name"))
            .collect();

        assert_eq!(initializers, ["app.Init", "lib.Init"]);

        assert_eq!(app.children("uses-permission").count(), 1);
        assert!(app.child("uses-sdk").is_none());
    }
}
