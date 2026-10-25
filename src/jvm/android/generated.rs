// Java sources Android builds generate: R classes from symbol tables (R.txt), view bindings from layouts, BuildConfig fields
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::system;
use crate::common::xml::{self, XmlElement};

/// The resources an R class has, as aapt2 writes them with --output-text-symbols
#[derive(Debug, Default)]
pub struct SymbolTable {
    symbols: Vec<Symbol>,
    index: HashMap<(String, String), usize>,
}

impl SymbolTable {
    pub fn read(file: &Path) -> Result<Self, String> {
        let mut table = SymbolTable::default();

        for line in system::read_file(file)?.lines() {
            let mut parts = line.splitn(4, ' ');
            let (Some(java_type), Some(resource_type), Some(name), Some(value)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };

            let symbol = Symbol {
                java_type: java_type.to_string(),
                resource_type: resource_type.to_string(),
                name: name.to_string(),
                value: value.trim().to_string(),
            };

            table.index.insert(
                (symbol.resource_type.clone(), symbol.name.clone()),
                table.symbols.len(),
            );
            table.symbols.push(symbol);
        }

        return Ok(table);
    }

    /// The symbols of `all` (the library linked with its dependencies) that the library
    /// declares: those of `own`, with the styleable indices only `all` knows
    pub fn write_own(all: &SymbolTable, own: &SymbolTable, file: &Path) -> Result<(), String> {
        let styleables: Vec<String> = own.symbols.iter()
            .filter(|symbol| symbol.java_type == "int[]")
            .map(|symbol| format!("{}_", symbol.name))
            .collect();

        let declared = |symbol: &Symbol| {
            own.get(&symbol.resource_type, &symbol.name).is_some()
                || symbol.resource_type == "styleable"
                    && styleables.iter()
                        .any(|prefix| symbol.name.starts_with(prefix))
        };

        let lines: Vec<String> = all.symbols.iter()
            .filter(|symbol| declared(symbol))
            .map(|symbol| {
                format!(
                    "{} {} {} {}",
                    symbol.java_type, symbol.resource_type, symbol.name, symbol.value
                )
            })
            .collect();

        return system::write_file(file, &(lines.join("\n") + "\n"));
    }

    fn get(&self, resource_type: &str, name: &str) -> Option<&Symbol> {
        return self.index.get(&(resource_type.to_string(), name.to_string()))
            .map(|index| &self.symbols[*index]);
    }
}

/// `package`.R for `symbols`. With `ids` (the app's) the fields are final with the app's
/// ids and resources the app lacks are left out; without, they are for compiling against.
pub fn write_r_class(
    package: &str,
    symbols: &SymbolTable,
    ids: Option<&SymbolTable>,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    let modifiers = if ids.is_some() {
        "public static final"
    } else {
        "public static"
    };

    let mut classes: Vec<(&str, Vec<String>)> = vec![];

    for symbol in &symbols.symbols {
        let value = match ids {
            Some(ids) => match ids.get(&symbol.resource_type, &symbol.name) {
                Some(final_symbol) => &final_symbol.value,
                None => continue,
            },
            None => &symbol.value,
        };

        let field = match symbol.java_type.as_str() {
            "int[]" => format!("        {modifiers} int[] {} = {};", symbol.name, value),
            _ => format!("        {modifiers} int {} = {};", symbol.name, value),
        };

        match classes.iter_mut()
            .find(|(resource_type, _)| *resource_type == symbol.resource_type)
        {
            Some((_, fields)) => fields.push(field),
            None => classes.push((&symbol.resource_type, vec![field])),
        }
    }

    let mut source = format!("package {package};\n\npublic final class R {{\n");

    for (resource_type, fields) in classes {
        source.push_str(&format!(
            "    public static final class {resource_type} {{\n{}\n    }}\n",
            fields.join("\n")
        ));
    }

    source.push_str("}\n");

    let file = output_dir.join(package.replace('.', "/")).join("R.java");
    system::write_file(&file, &source)?;

    return Ok(file);
}

/// Each with its variants (layout, layout-land, …), the plain one first
pub fn layouts_by_name(res_dirs: &[PathBuf]) -> Result<BTreeMap<String, Vec<XmlElement>>, String> {
    let mut layouts: BTreeMap<String, Vec<XmlElement>> = BTreeMap::new();

    for res_dir in res_dirs {
        let Ok(entries) = fs::read_dir(res_dir) else {
            continue;
        };

        let mut directories: Vec<PathBuf> = entries.flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("layout"))
            })
            .collect();

        // "layout" sorts before "layout-land" and the others
        directories.sort();

        for directory in directories {
            for file in system::files_with_extension(&[directory], "xml") {
                let name = file.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                let content = system::read_file(&file)?;
                let layout =
                    xml::parse(&content).map_err(|error| format!("{}: {error}", file.display()))?;

                layouts.entry(name)
                    .or_default()
                    .push(layout);
            }
        }
    }

    return Ok(layouts);
}

/// activity_main -> ActivityMainBinding
pub fn binding_class(layout: &str) -> String {
    let mut name: String = layout.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect();

    name.push_str("Binding");

    return name;
}

/// Java source of a view binding, None for layouts without one. `libraries`: namespaces
/// and layouts of library modules with view binding, highest priority first.
pub fn view_binding_source(
    namespace: &str,
    layout: &str,
    variants: &[XmlElement],
    layouts: &BTreeMap<String, Vec<XmlElement>>,
    libraries: &[(String, BTreeMap<String, Vec<XmlElement>>)],
) -> Option<String> {
    let root = variants.first()?;

    // data binding layouts and opted out ones
    if root.name == "layout" || root.attribute("tools:viewBindingIgnore") == Some("true") {
        return None;
    }

    let is_merge = root.name == "merge";

    let root_type = if is_merge
        || variants.iter()
            .any(|variant| view_class(variant) != view_class(root))
    {
        "android.view.View".to_string()
    } else {
        view_class(root)
    };

    let mut views = vec![];

    for variant in variants {
        collect_bound_views(variant, &mut views);
    }

    // the root of an included layout and the namespace of its binding, None for own layouts
    let find_layout = |name: &str| -> Option<(Option<&str>, &XmlElement)> {
        if let Some(root) = layouts.get(name).and_then(|variants| variants.first()) {
            return Some((None, root));
        }
        return libraries.iter().find_map(|(namespace, layouts)| {
            layouts.get(name)
                .and_then(|variants| variants.first())
                .map(|root| (Some(namespace.as_str()), root))
        });
    };

    let is_merge_layout =
        |name: &str| find_layout(name).is_some_and(|(_, root)| root.name == "merge");

    for view in &mut views {
        let Some(include) = view.include.clone() else {
            continue;
        };

        match find_layout(&include) {
            Some((_, root))
                if root.name == "layout"
                    || root.attribute("tools:viewBindingIgnore") == Some("true") =>
            {
                view.include = None;
                view.java_type = "android.view.View".to_string();
            }
            Some((None, _)) => {}
            Some((Some(library), _)) => {
                view.java_type = format!("{library}.databinding.{}", binding_class(&include))
            }
            // layouts of aars come without binding classes, they stay plain views
            None => {
                view.include = None;
                view.java_type = "android.view.View".to_string();
            }
        }
    }

    let class = binding_class(layout);
    let r = format!("{namespace}.R");
    let mut out = format!(
        "// Generated by build-script like the view binding of the Android Gradle plugin\npackage {namespace}.databinding;\n\n"
    );

    out.push_str(&format!(
        "public final class {class} implements androidx.viewbinding.ViewBinding {{\n"
    ));
    out.push_str(&format!("  private final {root_type} bindingRoot__;\n"));

    for view in &views {
        out.push_str(&format!(
            "  public final {} {};\n",
            view.java_type,
            field_name(&view.id)
        ));
    }

    let parameters: String = views.iter()
        .map(|view| format!(", {} {}", view.java_type, field_name(&view.id)))
        .collect();

    out.push_str(&format!("\n  private {class}({root_type} bindingRoot__{parameters}) {{\n    this.bindingRoot__ = bindingRoot__;\n"));

    for view in &views {
        let name = field_name(&view.id);
        out.push_str(&format!("    this.{name} = {name};\n"));
    }

    out.push_str("  }\n\n");
    out.push_str(&format!(
        "  @Override\n  public {root_type} getRoot() {{\n    return bindingRoot__;\n  }}\n\n"
    ));

    if is_merge {
        out.push_str(&format!(
            "  public static {class} inflate(android.view.LayoutInflater inflater, android.view.ViewGroup parent) {{\n    if (parent == null) {{\n      throw new NullPointerException(\"parent\");\n    }}\n    inflater.inflate({r}.layout.{layout}, parent);\n    return bind(parent);\n  }}\n\n",
        ));
    } else {
        out.push_str(&format!(
            "  public static {class} inflate(android.view.LayoutInflater inflater) {{\n    return inflate(inflater, null, false);\n  }}\n\n  public static {class} inflate(android.view.LayoutInflater inflater, android.view.ViewGroup parent, boolean attachToParent) {{\n    android.view.View root__ = inflater.inflate({r}.layout.{layout}, parent, false);\n    if (attachToParent) {{\n      parent.addView(root__);\n    }}\n    return bind(root__);\n  }}\n\n",
        ));
    }

    // an <include android:id> replaces the id of the included root, so the
    // root is never looked up by id
    let root_id = root.attribute("android:id")
        .and_then(|id| id.strip_prefix("@+id/").or_else(|| id.strip_prefix("@id/")));

    out.push_str(&format!("  public static {class} bind(android.view.View root__) {{\n    android.view.View found__;\n"));

    for view in &views {
        let name = field_name(&view.id);
        if !is_merge && view.include.is_none() && Some(view.id.as_str()) == root_id {
            out.push_str(&format!(
                "    {} {name} = ({}) root__;\n",
                view.java_type, view.java_type
            ));
            continue;
        }

        match &view.include {
            Some(include) if is_merge_layout(include) => {
                out.push_str(&format!(
                    "    {} {name} = {}.bind(root__);\n",
                    view.java_type, view.java_type
                ));
            }
            Some(_) => {
                out.push_str(&format!(
                    "    found__ = root__.findViewById({r}.id.{});\n    {} {name} = found__ == null ? null : {}.bind(found__);\n",
                    view.id, view.java_type, view.java_type,
                ));
            }
            None => {
                out.push_str(&format!(
                    "    {} {name} = ({}) root__.findViewById({r}.id.{});\n",
                    view.java_type, view.java_type, view.id
                ));
            }
        }
    }

    let arguments: String = views.iter()
        .map(|view| format!(", {}", field_name(&view.id)))
        .collect();

    out.push_str(&format!(
        "    return new {class}(({root_type}) root__{arguments});\n  }}\n}}\n"
    ));

    return Some(out);
}

/// Gradle's buildConfigField("String", "API_URL", "\"https://…\"") is written
/// "String API_URL https://…": String values get their quotes added
pub fn build_config_java_fields(fields: &[String]) -> Result<String, String> {
    let mut out = String::new();

    for field in fields {
        let invalid =
            || format!("build_config_fields entry '{field}' must look like 'Type NAME value'");

        let (java_type, rest) = field.trim()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;

        let (name, value) = rest.trim_start()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;

        let value = value.trim();
        let value = if java_type == "String" && !value.starts_with('"') {
            format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            value.to_string()
        };

        out.push_str(&format!(
            "  public static final {java_type} {name} = {value};\n"
        ));
    }

    return Ok(out);
}

/// One line of an R.txt: `int anim fade_in 0x7f010000`, `int[] styleable Foo { 0x1, 0x2 }`
/// or `int styleable Foo_bar 1`
#[derive(Debug, Clone, PartialEq)]
struct Symbol {
    /// int or int[]
    java_type: String,
    resource_type: String,
    name: String,
    value: String,
}

/// One view with an id, in the binding: a field
struct ViewBindingField {
    id: String,
    java_type: String,
    /// layout of an <include>, bound with its own binding class
    include: Option<String>,
}

/// message_text -> messageText
fn field_name(id: &str) -> String {
    let class = binding_class(id);
    let class = class.trim_end_matches("Binding");
    let mut chars = class.chars();

    return match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    };
}

/// The class of a view tag, the way LayoutInflater resolves short names
fn view_class(element: &XmlElement) -> String {
    match element.name.as_str() {
        "view" => {
            return element.attribute("class")
                .unwrap_or("android.view.View")
                .to_string();
        }
        name if name.contains('.') => return name.to_string(),
        "View" | "ViewStub" | "SurfaceView" | "TextureView" | "ViewGroup" => {
            return format!("android.view.{}", element.name);
        }
        "WebView" => return "android.webkit.WebView".to_string(),
        name => return format!("android.widget.{name}"),
    }
}

fn collect_bound_views(element: &XmlElement, views: &mut Vec<ViewBindingField>) {
    let id = element.attribute("android:id")
        .and_then(|id| id.strip_prefix("@+id/").or_else(|| id.strip_prefix("@id/")));

    let skipped = matches!(
        element.name.as_str(),
        "fragment" | "requestFocus" | "tag" | "merge"
    );

    if let (Some(id), false) = (id, skipped) {
        let include = (element.name == "include").then(|| {
            element.attribute("layout")
                .and_then(|layout| layout.strip_prefix("@layout/"))
                .map(str::to_string)
        })
            .flatten();

        let java_type = match &include {
            Some(layout) => binding_class(layout),
            None => view_class(element),
        };

        match views.iter_mut().find(|view| view.id == id) {
            // the same id with different views in layout variants
            Some(view) if view.java_type != java_type => {
                view.java_type = "android.view.View".to_string();
                view.include = None;
            }
            Some(_) => {}
            None => views.push(ViewBindingField {
                id: id.to_string(),
                java_type,
                include,
            }),
        }
    }

    for child in &element.children {
        collect_bound_views(child, views);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_classes_get_the_ids_of_the_app() {
        let directory =
            std::env::temp_dir().join(format!("build-script-r-classes-{}", std::process::id()));

        let _ = fs::remove_dir_all(&directory);
        system::write_file(&directory.join("library.txt"), "int color accent 0x0\nint[] styleable Badge { 0x0, 0x0 }\nint styleable Badge_android_text 0\nint string gone 0x0\n").unwrap();
        system::write_file(&directory.join("app.txt"), "int color accent 0x7f050001\nint color other 0x7f050002\nint[] styleable Badge { 0x101014f, 0x7f040003 }\nint styleable Badge_android_text 0\n").unwrap();
        let library = SymbolTable::read(&directory.join("library.txt")).unwrap();
        let app = SymbolTable::read(&directory.join("app.txt")).unwrap();

        let file = write_r_class(
            "com.example.lib",
            &library,
            Some(&app),
            &directory.join("final"),
        )
        .unwrap();

        let source = fs::read_to_string(file).unwrap();
        assert!(
            source.contains("public static final int accent = 0x7f050001;"),
            "{source}"
        );
        assert!(
            source.contains("public static final int[] Badge = { 0x101014f, 0x7f040003 };"),
            "{source}"
        );
        // only what the library declares, and only what the app has
        assert!(
            !source.contains("other") && !source.contains("gone"),
            "{source}"
        );

        let file = write_r_class(
            "com.example.lib",
            &library,
            None,
            &directory.join("compile"),
        )
        .unwrap();

        assert!(
            fs::read_to_string(file)
                .unwrap()
                .contains("public static int gone = 0x0;")
        );

        // a library without its dependencies loses the indices of attributes from them
        system::write_file(
            &directory.join("own.txt"),
            "int[] styleable Badge { 0x0, 0x0 }\nint styleable Badge_android_text 1\n",
        )
        .unwrap();
        system::write_file(&directory.join("all.txt"), "int attr srcCompat 0x0\nint[] styleable Badge { 0x0, 0x0 }\nint styleable Badge_android_text 1\nint styleable Badge_srcCompat 0\n").unwrap();
        let own = SymbolTable::read(&directory.join("own.txt")).unwrap();
        let all = SymbolTable::read(&directory.join("all.txt")).unwrap();
        SymbolTable::write_own(&all, &own, &directory.join("R.txt")).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("R.txt")).unwrap(),
            "int[] styleable Badge { 0x0, 0x0 }\nint styleable Badge_android_text 1\nint styleable Badge_srcCompat 0\n"
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn view_bindings_are_generated_from_layouts() {
        let res = std::env::temp_dir().join(format!("build-script-res-{}", std::process::id()));
        let _ = fs::remove_dir_all(&res);

        let layout = |dir: &str, name: &str, content: &str| {
            fs::create_dir_all(res.join(dir)).unwrap();
            fs::write(res.join(dir).join(format!("{name}.xml")), content).unwrap();
        };

        layout(
            "layout",
            "activity_main",
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:id="@+id/main_root">
            <TextView android:id="@+id/message_text"/>
            <com.example.Custom android:id="@+id/custom"/>
            <include layout="@layout/toolbar" android:id="@+id/toolbar"/>
            <include layout="@layout/footer"/>
            <include layout="@layout/merged" android:id="@+id/merged"/>
            <include layout="@layout/lib_toolbar" android:id="@+id/lib_toolbar"/>
            <include layout="@layout/from_aar" android:id="@+id/from_aar"/>
            <fragment android:id="@+id/map" android:name="x.Map"/>
        </LinearLayout>"#,
        );
        // a variant with another view for the same id
        layout(
            "layout-land",
            "activity_main",
            r#"<LinearLayout android:id="@+id/main_root"><Button android:id="@+id/message_text"/></LinearLayout>"#,
        );
        layout(
            "layout",
            "toolbar",
            r#"<FrameLayout><ImageView android:id="@+id/logo"/></FrameLayout>"#,
        );
        layout(
            "layout",
            "merged",
            r#"<merge><View android:id="@+id/divider"/></merge>"#,
        );
        layout(
            "layout",
            "ignored",
            r#"<FrameLayout xmlns:tools="http://schemas.android.com/tools" tools:viewBindingIgnore="true"/>"#,
        );

        let layouts = layouts_by_name(&[res.clone()]).unwrap();
        assert_eq!(
            layouts.keys().collect::<Vec<_>>(),
            ["activity_main", "ignored", "merged", "toolbar"]
        );
        assert_eq!(layouts["activity_main"].len(), 2);

        // a library module with view binding, layouts of aars are unknown
        let library_layouts = [(
            "lib_toolbar".to_string(),
            vec![xml::parse("<androidx.appcompat.widget.Toolbar/>").unwrap()],
        )];

        let libraries = vec![("com.lib".to_string(), library_layouts.into_iter().collect())];

        let source = view_binding_source(
            "com.app",
            "activity_main",
            &layouts["activity_main"],
            &layouts,
            &libraries,
        )
        .unwrap();

        for expected in [
            "package com.app.databinding;",
            "public final class ActivityMainBinding implements androidx.viewbinding.ViewBinding {",
            "public final android.widget.LinearLayout mainRoot;",
            "public final android.view.View messageText;",
            "public final com.example.Custom custom;",
            "public final ToolbarBinding toolbar;",
            "public final MergedBinding merged;",
            "public final com.lib.databinding.LibToolbarBinding libToolbar;",
            "libToolbar = found__ == null ? null : com.lib.databinding.LibToolbarBinding.bind(found__);",
            "public final android.view.View fromAar;",
            "inflater.inflate(com.app.R.layout.activity_main, parent, false);",
            "toolbar = found__ == null ? null : ToolbarBinding.bind(found__);",
            "MergedBinding merged = MergedBinding.bind(root__);",
            "android.widget.LinearLayout mainRoot = (android.widget.LinearLayout) root__;",
            "android.view.View messageText = (android.view.View) root__.findViewById(com.app.R.id.message_text);",
            "return new ActivityMainBinding((android.widget.LinearLayout) root__, mainRoot, messageText, custom, toolbar, merged, libToolbar, fromAar);",
        ] {
            assert!(
                source.contains(expected),
                "missing {expected:?} in\n{source}"
            );
        }

        assert!(!source.contains("map"), "fragments get no field:\n{source}");

        let merged =
            view_binding_source("com.app", "merged", &layouts["merged"], &layouts, &[]).unwrap();

        assert!(
            merged.contains(
                "inflate(android.view.LayoutInflater inflater, android.view.ViewGroup parent)"
            ),
            "{merged}"
        );
        assert!(
            view_binding_source("com.app", "ignored", &layouts["ignored"], &layouts, &[]).is_none()
        );

        fs::remove_dir_all(&res).unwrap();
    }

    #[test]
    fn build_config_fields_become_java_declarations() {
        let fields = build_config_java_fields(&[
            "String API_URL https://example.com/a b".to_string(),
            "String QUOTED \"x\"".to_string(),
            "int  RETRIES  3".to_string(),
        ])
        .unwrap();

        assert_eq!(
            fields,
            "  public static final String API_URL = \"https://example.com/a b\";\n  public static final String QUOTED = \"x\";\n  public static final int RETRIES = 3;\n"
        );
        assert!(build_config_java_fields(&["boolean".to_string()]).is_err());
    }
}
