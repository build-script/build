// Templates of JVM modules for `build new` and `build add`: Kotlin and Java apps and libraries,
// Android apps and libraries with Jetpack Compose
use crate::project::{ModuleTemplate, NewModule};

pub const MODULE_KINDS: &[&str] = &[
    "kotlin-app",
    "kotlin-library",
    "java-app",
    "java-library",
    "android-app",
    "android-library",
];

/// What each kind of project and module is, for the choice of new and add
pub const ABOUT: &[(&str, &str)] = &[
    ("java", "a Java program"),
    ("kotlin", "a Kotlin program"),
    ("android", "an Android app with Compose"),
    ("kotlin-app", "a Kotlin program"),
    ("kotlin-library", "a Kotlin library"),
    ("java-app", "a Java program"),
    ("java-library", "a Java library"),
    ("android-app", "an Android app with Compose"),
    ("android-library", "an Android library with Compose"),
];

/// The modules each kind of project starts with, dependencies first: the
/// kind of module and its directory, None for a module at the project root
pub const PROJECT_KINDS: &[(&str, &[(&str, Option<&str>)])] = &[
    ("java", &[("java-app", None)]),
    ("kotlin", &[("kotlin-app", None)]),
    ("android", &[("android-app", None)]),
];

/// Java and Kotlin keywords, which cannot be part of a package name
const KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "fun",
    "goto",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "int",
    "interface",
    "is",
    "long",
    "native",
    "new",
    "null",
    "object",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "typealias",
    "typeof",
    "val",
    "var",
    "void",
    "volatile",
    "when",
    "while",
];

const MAVEN_CENTRAL: &[&str] = &["maven_central()"];
const GOOGLE_AND_MAVEN_CENTRAL: &[&str] = &["google()", "maven_central()"];

/// The versions of the Compose libraries that go together
const COMPOSE_BOM: &str = r#"platform("androidx.compose:compose-bom:2026.09.00")"#;
/// setContent for the activity
const ACTIVITY_COMPOSE: &str = r#""androidx.activity:activity-compose:1.13.0""#;
const COMPOSE_LIBRARIES: &[&str] = &[
    r#""androidx.compose.material3:material3""#,
    r#""androidx.compose.ui:ui""#,
];

/// Like the Java naming conventions: reversed, other characters than letters and digits
/// become underscores, keywords and labels starting with a digit get one: mvmo.dev -> dev.mvmo
pub fn package_of_domain(domain: &str) -> String {
    let segments: Vec<String> = domain.split('.')
        .rev()
        .map(|label| {
            let segment = package_segment(label);
            if segment.starts_with(|char: char| char.is_ascii_digit()) {
                format!("_{segment}")
            } else {
                segment
            }
        })
        .collect();
    return segments.join(".");
}

/// my-App -> my_app, a keyword gets an underscore
pub fn package_segment(name: &str) -> String {
    let segment: String = name.to_ascii_lowercase()
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char
            } else {
                '_'
            }
        })
        .collect();

    if KEYWORDS.contains(&segment.as_str()) {
        return format!("{segment}_");
    }

    return segment;
}

/// Java identifiers separated by dots, none of them a keyword
pub fn checked_package(package: &str) -> Result<String, String> {
    let valid = package.split('.').all(|segment| {
        segment.chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && segment.chars()
                .all(|char| char.is_ascii_alphanumeric() || char == '_')
            && !KEYWORDS.contains(&segment)
    });

    if !valid {
        return Err(format!(
            "'{package}' is not a package name: identifiers separated by dots, like dev.mvmo"
        ));
    }

    return Ok(package.to_string());
}

pub fn module_template(kind: &str, module: &NewModule) -> Option<ModuleTemplate> {
    let package_dir = module.package.replace('.', "/");
    let package = &module.package;
    let name = &module.name;

    let dependencies = dependency_list(module, &[]);

    let template = match kind {
        "kotlin-app" => {
            let main = match module.dependencies.first() {
                Some((_, library_package)) => format!(
                    "package {package}\n\nimport {library_package}.greeting\n\nfun main() {{\n    println(greeting(\"{name}\"))\n}}\n"
                ),
                None => format!(
                    "package {package}\n\nfun main() {{\n    println(\"Hello from {name}\")\n}}\n"
                ),
            };
            ModuleTemplate {
                use_argument: "kotlin".to_string(),
                repositories: MAVEN_CENTRAL,
                is_app: true,
                files: vec![(format!("src/main/kotlin/{package_dir}/Main.kt"), main)],
                function: "build_kotlin".to_string(),
                positional: vec![quoted(name), dependencies],
                named: vec![("main_class".to_string(), quoted(&format!("{package}.MainKt")))],
            }
        }
        "kotlin-library" => ModuleTemplate {
            use_argument: "kotlin".to_string(),
            repositories: MAVEN_CENTRAL,
            is_app: false,
            files: vec![(
                format!("src/main/kotlin/{package_dir}/Greeting.kt"),
                format!(
                    "package {package}\n\nfun greeting(name: String): String = \"Hello, $name, from {name}\"\n"
                ),
            )],
            function: "build_kotlin".to_string(),
            positional: vec![quoted(name), dependencies],
            named: vec![],
        },
        "java-app" => ModuleTemplate {
            use_argument: "java".to_string(),
            repositories: MAVEN_CENTRAL,
            is_app: true,
            files: vec![(
                format!("src/main/java/{package_dir}/Main.java"),
                format!(
                    "package {package};\n\npublic class Main {{\n    public static void main(String[] args) {{\n        System.out.println(\"Hello from {name}\");\n    }}\n}}\n"
                ),
            )],
            function: "build_java".to_string(),
            positional: vec![quoted(name), dependencies],
            named: vec![("main_class".to_string(), quoted(&format!("{package}.Main")))],
        },
        "java-library" => ModuleTemplate {
            use_argument: "java".to_string(),
            repositories: MAVEN_CENTRAL,
            is_app: false,
            files: vec![(
                format!("src/main/java/{package_dir}/Greeting.java"),
                format!(
                    "package {package};\n\npublic final class Greeting {{\n    private Greeting() {{\n    }}\n\n    public static String greeting(String name) {{\n        return \"Hello, \" + name + \", from {name}\";\n    }}\n}}\n",
                ),
            )],
            function: "build_java".to_string(),
            positional: vec![quoted(name), dependencies],
            named: vec![],
        },
        "android-app" => ModuleTemplate {
            use_argument: "android".to_string(),
            repositories: GOOGLE_AND_MAVEN_CENTRAL,
            is_app: true,
            files: vec![
                (
                    "src/main/AndroidManifest.xml".to_string(),
                    APP_MANIFEST.replace("{name}", name),
                ),
                (
                    format!("src/main/kotlin/{package_dir}/MainActivity.kt"),
                    MAIN_ACTIVITY
                        .replace("{package}", package)
                        .replace("{name}", name),
                ),
                (
                    format!("src/main/kotlin/{package_dir}/ui/theme/Theme.kt"),
                    THEME.replace("{package}", package),
                ),
            ],
            function: "build_android".to_string(),
            positional: vec![
                quoted(name),
                quoted(package),
                dependency_list(
                    module,
                    &[&[COMPOSE_BOM, ACTIVITY_COMPOSE], COMPOSE_LIBRARIES].concat(),
                ),
            ],
            named: vec![("plugins".to_string(), r#"list("compose")"#.to_string())],
        },
        "android-library" => ModuleTemplate {
            use_argument: "android".to_string(),
            repositories: GOOGLE_AND_MAVEN_CENTRAL,
            is_app: false,
            files: vec![(
                format!("src/main/kotlin/{package_dir}/Greeting.kt"),
                LIBRARY_GREETING
                    .replace("{package}", package)
                    .replace("{name}", name),
            )],
            function: "build_android_library".to_string(),
            positional: vec![
                quoted(name),
                quoted(package),
                dependency_list(module, &[&[COMPOSE_BOM], COMPOSE_LIBRARIES].concat()),
            ],
            named: vec![("plugins".to_string(), r#"list("compose")"#.to_string())],
        },
        _ => return None,
    };

    return Some(template);
}

/// list(...) of the modules it depends on and of `libraries`, which are written
/// as in the script, one a line when there are libraries
fn dependency_list(module: &NewModule, libraries: &[&str]) -> String {
    let entries: Vec<&str> = module.dependencies.iter()
        .map(|(variable, _)| variable.as_str())
        .chain(libraries.iter().copied())
        .collect();

    if libraries.is_empty() {
        return format!("list({})", entries.join(", "));
    }

    let lines: String = entries.iter()
        .map(|entry| format!("        {entry},\n"))
        .collect();

    return format!("list(\n{lines}    )");
}

/// Compose draws everything, the theme only keeps the window free of an action bar
const APP_MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application android:label="{name}" android:theme="@android:style/Theme.Material.Light.NoActionBar">
        <activity android:name=".MainActivity" android:exported="true">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
"#;

const MAIN_ACTIVITY: &str = r#"package {package}

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import {package}.ui.theme.AppTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            AppTheme {
                Scaffold(modifier = Modifier.fillMaxSize()) { innerPadding ->
                    Greeting("{name}", Modifier.padding(innerPadding))
                }
            }
        }
    }
}

@Composable
fun Greeting(name: String, modifier: Modifier = Modifier) {
    Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        Text("Hello from $name", style = MaterialTheme.typography.headlineMedium)
    }
}
"#;

const THEME: &str = r#"package {package}.ui.theme

import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalContext

/** The colors of the wallpaper from Android 12 on, Material's own before, dark when the system is */
@Composable
fun AppTheme(darkTheme: Boolean = isSystemInDarkTheme(), content: @Composable () -> Unit) {
    val colorScheme = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> {
            val context = LocalContext.current
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        }
        darkTheme -> darkColorScheme()
        else -> lightColorScheme()
    }

    MaterialTheme(colorScheme = colorScheme, content = content)
}
"#;

const LIBRARY_GREETING: &str = r#"package {package}

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

@Composable
fun Greeting(name: String, modifier: Modifier = Modifier) {
    Text("Hello, $name, from {name}", modifier, style = MaterialTheme.typography.bodyLarge)
}
"#;

fn quoted(value: &str) -> String {
    return format!("\"{value}\"");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_become_packages_like_the_java_naming_conventions_say() {
        assert_eq!(package_of_domain("mvmo.dev"), "dev.mvmo");
        assert_eq!(package_of_domain("example.com"), "com.example");
        // hyphens, keywords and digits at the start do not make identifiers
        assert_eq!(
            package_of_domain("my-site.example.org"),
            "org.example.my_site"
        );
        assert_eq!(
            package_of_domain("int.1password.com"),
            "com._1password.int_"
        );

        assert_eq!(package_segment("My-App"), "my_app");
        assert_eq!(package_segment("native"), "native_");
        assert_eq!(checked_package("dev.mvmo"), Ok("dev.mvmo".to_string()));
        assert!(checked_package("dev..mvmo").is_err());
        assert!(checked_package("dev.mvmo.new").is_err());
        assert!(checked_package("1dev").is_err());
    }
}
