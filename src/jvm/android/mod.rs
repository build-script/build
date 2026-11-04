// Android: apps and libraries built without the Android Gradle plugin
mod build;
mod device;
mod generated;
mod manifest;
mod release;
mod sdk;

use std::path::{Path, PathBuf};

use crate::common::system;
use crate::common::xml::{self, XmlElement};
use crate::jvm::JvmLaunch;
use crate::jvm::idea;
use crate::jvm::jar_merge::{JarContents, merge_jars};
use crate::jvm::maven::{MavenCoordinate, TargetPlatform};
use crate::jvm::module::{
    Dependencies, ModuleBuildInputs, Sources, dependencies_type, kotlin_and_java_sources,
    module_parameters, source_set_dirs,
};
use crate::jvm::testing::{UnitTests, unit_test_parameters};
use crate::language::runtime::{
    Arguments, BuildScriptContext, FunctionParameters, FunctionSignature, ModuleTests, TestOutcome,
    TestSuite, Type, Value, builtin,
};

use build::{
    AndroidBuild, BuildType, LinkedResources, Signature, android_input_paths,
    existing_dirs, native_library_dirs, read_android_inputs,
};
use device::InstrumentedTests;
use generated::SymbolTable;
use manifest::{launcher_activity, read_manifest_with_placeholders};
use release::ReleaseKey;
use sdk::AndroidSdk;

pub use build::DEFAULT_NDK_VERSION;
pub use device::install_and_start;
pub use sdk::ndk;

pub const DEFAULT_COMPILE_SDK: i64 = 36;
pub const DEFAULT_MIN_SDK: i64 = 24;
pub const DEFAULT_BUILD_TOOLS: &str = "36.1.0";
/// what new Android Studio projects run their instrumented tests with
const DEFAULT_TEST_RUNNER: &str = "androidx.test.runner.AndroidJUnitRunner";
/// added to the instrumented tests when they use the default runner without naming it
const DEFAULT_TEST_RUNNER_LIBRARY: &str = "androidx.test:runner:1.6.2";
/// R.txt of a library module, what apps write its R class from
const LIBRARY_SYMBOL_FILE: &str = "intermediates/R.txt";

/// What consumers of an Android library need besides its classes
#[derive(Debug, Hash)]
pub struct AndroidResources {
    pub namespace: String,
    /// res directories with files, lowest priority first
    pub res_dirs: Vec<PathBuf>,
    /// merged into the manifest of apps using the library
    pub manifest: Option<PathBuf>,
    /// R.txt: the resources of the library's R class
    pub symbols: Option<PathBuf>,
    /// assets directories with files, lowest priority first
    pub asset_dirs: Vec<PathBuf>,
    /// jniLibs directories with <abi>/*.so, lowest priority first
    pub native_library_dirs: Vec<PathBuf>,
    /// the library has <Layout>Binding classes in <namespace>.databinding
    pub view_binding: bool,
    /// ProGuard rules for release builds of the apps using the library
    pub consumer_proguard_files: Vec<PathBuf>,
}

pub fn builtins() -> Vec<(&'static str, Value)> {
    let app = android_parameters(
        FunctionParameters::new()
            .required("name", Type::String)
            .required("namespace", Type::String)
            .required("dependencies", dependencies_type())
            .required("repositories", Type::list(Type::String))
            .optional("application_id", Type::String)
            .with_default("min_sdk", Type::Int, DEFAULT_MIN_SDK)
            .optional("target_sdk", Type::Int)
            .with_default("compile_sdk", Type::Int, DEFAULT_COMPILE_SDK)
            .with_default("version_code", Type::Int, 1i64)
            .with_default("version_name", Type::String, "1.0")
            .optional("core_library_desugaring", Type::String)
            // asset name of a JSON report of the libraries' licenses
            .optional("license_report", Type::String)
            // only for build --release: R8 with these rules besides the libraries'
            .with_default("minify", Type::Bool, true)
            .with_default("proguard_files", Type::list(Type::String), Value::string_list(&[]))
            // release builds also build an app bundle for Google Play
            .with_default("app_bundle", Type::Bool, false)
            // the release key, its passwords from environment variables the script names
            .optional("keystore", Type::String)
            .optional("key_alias", Type::String)
            .optional("keystore_password_env", Type::String)
            .optional("key_password_env", Type::String),
    );

    let library = android_parameters(
        FunctionParameters::new()
            .required("name", Type::String)
            .required("namespace", Type::String)
            .required("dependencies", dependencies_type())
            .required("repositories", Type::list(Type::String))
            .with_default("min_sdk", Type::Int, DEFAULT_MIN_SDK)
            .with_default("compile_sdk", Type::Int, DEFAULT_COMPILE_SDK)
            // ProGuard rules for release builds of the apps using the library
            .with_default(
                "consumer_proguard_files",
                Type::list(Type::String),
                Value::string_list(&[]),
            ),
    );

    return vec![
        (
            "build_android",
            builtin("build_android", app, build_android),
        ),
        (
            "build_android_library",
            builtin("build_android_library", library, build_android_library),
        ),
    ];
}

/// Those of build_kotlin and those of Android modules. Debug builds: the debug
/// source set lies over main, like with the Gradle plugin.
fn android_parameters(parameters: FunctionParameters) -> FunctionSignature {
    let source_sets = &["main", "debug"];
    return unit_test_parameters(
        module_parameters(parameters, source_sets, Sources::KotlinAndJava),
        Sources::KotlinAndJava,
    )
    // "name=value" for ${name} in the manifests
    .with_default(
        "manifest_placeholders",
        Type::list(Type::String),
        Value::string_list(&[]),
    )
    .with_default("view_binding", Type::Bool, false)
    // like android.nonTransitiveRClass, the default since Android Gradle plugin 8
    .with_default("non_transitive_r_class", Type::Bool, true)
    // "Type NAME value", like buildConfigField in Gradle
    .with_default(
        "build_config_fields",
        Type::list(Type::String),
        Value::string_list(&[]),
    )
    .with_default(
        "res_dirs",
        Type::list(Type::String),
        source_set_dirs(source_sets, &["res"]),
    )
    .with_default(
        "asset_dirs",
        Type::list(Type::String),
        source_set_dirs(source_sets, &["assets"]),
    )
    .with_default(
        "aidl_dirs",
        Type::list(Type::String),
        source_set_dirs(source_sets, &["aidl"]),
    )
    // like externalNativeBuild { cmake { path } }: native libraries built with the NDK
    .optional("cmake", Type::String)
    .with_default("ndk_version", Type::String, DEFAULT_NDK_VERSION)
    .with_default("build_tools", Type::String, DEFAULT_BUILD_TOOLS)
    // for builds without a terminal to ask about the SDK license
    .with_default("accept_sdk_license", Type::Bool, false)
    // like androidTestImplementation: the instrumented tests, which run on a device
    .with_default(
        "android_test_dependencies",
        dependencies_type(),
        Value::string_list(&[]),
    )
    .with_default(
        "android_test_source_dirs",
        Type::list(Type::String),
        source_set_dirs(&["androidTest"], &["kotlin", "java"]),
    )
    .with_default(
        "test_instrumentation_runner",
        Type::String,
        DEFAULT_TEST_RUNNER,
    )
    .returns(Type::Module);
}

/// An apk in build/outputs/apk/<build type>/: aapt2, kotlinc against android.jar, d8 or
/// for release R8, aligned and signed with the debug key or the release key. Release builds
/// with app_bundle also get an app bundle in build/outputs/bundle/release/.
fn build_android(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let inputs = read_android_inputs(context, args)?;
    let build_type = BuildType::of(context);
    let settings = ApkSettings::of_app(args, &inputs.directory)?;
    let namespace: String = args.get("namespace")?;
    let build_dir = system::build_dir(&inputs.directory);
    let manifest = build_dir.join("intermediates/manifest/AndroidManifest.xml");

    // debug builds work without the passwords of the release key
    let release_key = match build_type {
        BuildType::Debug => None,
        BuildType::Release => ReleaseKey::read(args, &context.project_dir)?,
    };

    let signature = match (build_type, &release_key) {
        (BuildType::Debug, _) => Signature::Debug,
        (BuildType::Release, Some(key)) => Signature::Release(key),
        (BuildType::Release, None) => Signature::Unsigned,
    };

    let output_name = match signature {
        Signature::Debug => format!("{}-debug", inputs.name),
        Signature::Release(_) => format!("{}-release", inputs.name),
        Signature::Unsigned => format!("{}-release-unsigned", inputs.name),
    };

    let apk = build_dir.join("outputs/apk")
        .join(build_type.name())
        .join(format!("{output_name}.apk"));

    let app_bundle: bool = args.get("app_bundle")?;
    let bundle = (build_type == BuildType::Release && app_bundle).then(|| {
        build_dir.join("outputs/bundle/release")
            .join(format!("{output_name}.aab"))
    });

    let mut input_paths = android_input_paths(&inputs, args, build_type)?;
    input_paths.extend(settings.proguard_files.iter().cloned());
    input_paths.extend(release_key.iter().map(|key| key.keystore.clone()));

    let mut fingerprint = inputs.input_fingerprint(&inputs.name, args, &input_paths)?;
    fingerprint.add(build_type.name());

    match signature {
        Signature::Debug => {}
        Signature::Release(key) => system::log(
            &inputs.name,
            &format!("Release build, signed with {} from {}", key.alias, key.keystore.display()),
        ),
        Signature::Unsigned => system::log(
            &inputs.name,
            "Release build, unsigned: build_android has no keystore",
        ),
    }

    // a missing IntelliJ module file builds the module again
    let idea_file = context.idea.then(|| idea::module_file(&context.project_dir, &inputs.name));
    let mut outputs: Vec<&Path> = vec![&apk, &manifest];
    outputs.extend(bundle.as_deref());
    outputs.extend(idea_file.as_deref());

    let (inputs, sdk, merged_manifest) =
        if fingerprint.reports_up_to_date(&inputs.name, &outputs)? {
            let merged_manifest = xml::parse(&system::read_file(&manifest)?)?;
            (inputs, android_sdk(args)?, merged_manifest)
        } else {
            let build = AndroidBuild::prepare(inputs, args, build_type)?;

            // before compiling: IntelliJ sees new libraries even while the code does not compile
            if context.idea {
                build.write_idea_module(&context.project_dir, args)?;
            }

            let source_manifest = build.main_dir.join("AndroidManifest.xml");

            if !source_manifest.is_file() {
                return Err(format!("{} is missing", source_manifest.display()));
            }

            let merged_manifest = Apk {
                settings: &settings,
                source_manifest: Some(&source_manifest),
                test_manifest: None,
                manifest: &manifest,
                signature: &signature,
                output: &apk,
                bundle: bundle.as_deref(),
            }
            .build(&build)?;

            fingerprint.store_done(&build.inputs.name, &apk)?;
            (build.inputs, build.sdk, merged_manifest)
        };

    // installed, then its launcher activity started
    let launch = JvmLaunch::Apk {
        sdk_root: sdk.root.clone(),
        application_id: settings.application_id.clone(),
        activity: launcher_activity(&merged_manifest, &namespace),
        min_sdk: args.get("min_sdk")?,
        signed: !matches!(signature, Signature::Unsigned),
    };

    // the app's classes, its R and BuildConfig, its Java resources
    let intermediates = build_dir.join("intermediates");
    let classes = [
        "classes/kotlin",
        "classes/generated",
        "java-resources-module",
    ]
    .map(|dir| intermediates.join(dir));

    let tests = android_tests(context, args, &inputs, &sdk, classes.to_vec(), &settings);

    return Ok(Value::from(inputs.built_module(
        None,
        None,
        apk,
        Some(launch),
        tests,
    )));
}

/// build/libs/<name>.jar. Apps using it compile its resources and generate
/// its R class, like for an aar.
fn build_android_library(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
    let inputs = read_android_inputs(context, args)?;
    let build_type = BuildType::of(context);
    let namespace: String = args.get("namespace")?;

    let consumer_proguard_files =
        required_module_files(args, "consumer_proguard_files", &inputs.directory)?;

    let source_manifest = Some(inputs.directory.join("src/main/AndroidManifest.xml"))
        .filter(|manifest| manifest.is_file());

    let jar = system::build_dir(&inputs.directory)
        .join("libs")
        .join(format!("{}.jar", inputs.name));

    let android = AndroidResources {
        namespace: namespace.clone(),
        res_dirs: existing_dirs(&inputs, args, "res_dirs", build_type)?,
        manifest: source_manifest.clone(),
        symbols: Some(system::build_dir(&inputs.directory).join(LIBRARY_SYMBOL_FILE)),
        asset_dirs: existing_dirs(&inputs, args, "asset_dirs", build_type)?,
        native_library_dirs: native_library_dirs(&inputs, args, build_type)?,
        view_binding: args.get("view_binding")?,
        consumer_proguard_files,
    };

    let mut fingerprint = inputs.input_fingerprint(
        &inputs.name,
        args,
        &android_input_paths(&inputs, args, build_type)?,
    )?;
    fingerprint.add(build_type.name());

    let idea_file = context.idea.then(|| idea::module_file(&context.project_dir, &inputs.name));
    let mut outputs = vec![jar.as_path()];
    outputs.extend(idea_file.as_deref());

    let (inputs, sdk) = if fingerprint.reports_up_to_date(&inputs.name, &outputs)? {
        // only the tests need the SDK then
        let sdk = if context.tests.includes(&inputs.name) {
            Some(android_sdk(args)?)
        } else {
            None
        };
        (inputs, sdk)
    } else {
        let build = AndroidBuild::prepare(inputs, args, build_type)?;

        if context.idea {
            build.write_idea_module(&context.project_dir, args)?;
        }

        let intermediates = build.build_dir.join("intermediates");
        let manifest = intermediates.join("manifest/AndroidManifest.xml");
        build.write_manifest(
            source_manifest.as_deref(),
            None,
            &build.namespace,
            false,
            &manifest,
        )?;

        // R with non-final ids only to compile against, apps generate the real one
        system::log(&build.inputs.name, "Linking resources");
        let own_resources = build.compile_own_resources()?;
        let symbols = system::build_dir(&build.inputs.directory).join(LIBRARY_SYMBOL_FILE);
        build.link_library(&manifest, &own_resources, &intermediates, &symbols)?;
        let r_classes = intermediates.join("classes/r");
        build.compile_generated_java(
            &build.write_r_classes(&symbols, None, &intermediates.join("generated/r"))?,
            &[],
            &r_classes,
        )?;

        let build_config = build
            .write_build_config(&[format!("String LIBRARY_PACKAGE_NAME {}", build.namespace)])?;

        let build_config_classes = intermediates.join("classes/buildconfig");
        build.compile_generated_java(&[build_config], &[], &build_config_classes)?;

        let classes = intermediates.join("classes/kotlin");

        let generated_resources =
            build.compile_sources(&[r_classes, build_config_classes.clone()], &classes)?;

        build.build_native_libraries()?;

        // BuildConfig, view bindings and Java resources belong to the library jar, R does not
        let module_resources = intermediates.join("java-resources-module");
        build
            .inputs
            .resources
            .process(&generated_resources, &module_resources)?;
        system::copy_dir_contents(&build_config_classes, &classes)?;
        system::copy_dir_contents(&module_resources, &classes)?;
        build.jar(&classes, &jar)?;

        fingerprint.store_done(&build.inputs.name, &jar)?;
        (build.inputs, Some(build.sdk))
    };

    let tests = match &sdk {
        Some(sdk) => {
            let classes = vec![
                jar.clone(),
                system::build_dir(&inputs.directory).join("intermediates/classes/r"),
            ];
            android_tests(
                context,
                args,
                &inputs,
                sdk,
                classes,
                &ApkSettings::of_library_tests(&namespace),
            )
        }
        None => vec![],
    };

    return Ok(Value::from(inputs.built_module(
        Some(jar.clone()),
        Some(android),
        jar,
        None,
        tests,
    )));
}

/// What an apk is built as besides the module: the app, or a library's instrumented tests
struct ApkSettings {
    application_id: String,
    version_code: i64,
    version_name: String,
    target_sdk: Option<i64>,
    core_library_desugaring: Option<String>,
    /// asset name of a JSON report of the libraries' licenses
    license_report: Option<String>,
    /// "Type NAME value" of its BuildConfig besides DEBUG and BUILD_TYPE
    build_config: Vec<String>,
    /// release builds: R8 or only d8, and the app's own rules for R8
    minify: bool,
    proguard_files: Vec<PathBuf>,
}

/// One apk of a module: what it is built as, from which manifests, to where
struct Apk<'a> {
    settings: &'a ApkSettings,
    /// src/main/AndroidManifest.xml, apps have one
    source_manifest: Option<&'a Path>,
    /// the <instrumentation> and src/androidTest/AndroidManifest.xml, over the module's manifest
    test_manifest: Option<&'a XmlElement>,
    /// the merged manifest
    manifest: &'a Path,
    signature: &'a Signature<'a>,
    output: &'a Path,
    /// release builds with app_bundle
    bundle: Option<&'a Path>,
}

impl ApkSettings {
    fn of_app(args: &Arguments, module_dir: &Path) -> Result<Self, String> {
        let namespace: String = args.get("namespace")?;

        let application_id = args.get::<Option<String>>("application_id")?
            .unwrap_or(namespace);

        let version_code: i64 = args.get("version_code")?;
        let version_name: String = args.get("version_name")?;

        return Ok(ApkSettings {
            build_config: vec![
                format!("String APPLICATION_ID {application_id}"),
                format!("int VERSION_CODE {version_code}"),
                format!("String VERSION_NAME {version_name}"),
            ],
            application_id,
            version_code,
            version_name,
            target_sdk: args.get("target_sdk")?,
            core_library_desugaring: args.get("core_library_desugaring")?,
            license_report: args.get("license_report")?,
            minify: args.get("minify")?,
            proguard_files: required_module_files(args, "proguard_files", module_dir)?,
        });
    }

    /// Like the Gradle plugin, the instrumented tests of a library run in an app of their own
    fn of_library_tests(namespace: &str) -> Self {
        return ApkSettings {
            application_id: format!("{namespace}.test"),
            version_code: 1,
            version_name: "1.0".to_string(),
            target_sdk: None,
            core_library_desugaring: None,
            license_report: None,
            build_config: vec![format!("String LIBRARY_PACKAGE_NAME {namespace}")],
            minify: false,
            proguard_files: vec![],
        };
    }
}

impl Apk<'_> {
    /// Resources linked with aapt2, R and BuildConfig, Kotlin and Java, dex with d8 or R8,
    /// packaged with the Java resources and native libraries, the app bundle too with `bundle`.
    /// Returns the merged manifest.
    fn build(&self, build: &AndroidBuild) -> Result<XmlElement, String> {
        let settings = self.settings;
        let target_sdk = settings.target_sdk.unwrap_or(build.compile_sdk);
        let intermediates = build.build_dir.join("intermediates");

        let merged_manifest = build.write_manifest(
            self.source_manifest,
            self.test_manifest,
            &settings.application_id,
            true,
            self.manifest,
        )?;

        system::log(&build.inputs.name, "Linking resources");
        let own_resources = build.compile_own_resources()?;
        let symbols = intermediates.join("R.txt");
        let linked = intermediates.join("apk/resources.ap_");

        let mut extra = vec![
            "--target-sdk-version".to_string(),
            target_sdk.to_string(),
            "--version-code".to_string(),
            settings.version_code.to_string(),
            "--version-name".to_string(),
            settings.version_name.clone(),
        ];

        // debug builds are debuggable, like with the Gradle plugin
        if build.build_type == BuildType::Debug {
            extra.push("--debug-mode".to_string());
        }

        let shrinks = build.build_type == BuildType::Release && settings.minify;
        let proguard_dir = intermediates.join("proguard");
        let aapt2_rules = proguard_dir.join("aapt2.pro");
        let main_dex_rules = proguard_dir.join("main-dex.pro");
        // below API 21 what starts the app has to be in the first dex file
        let needs_main_dex = build.min_sdk < 21;

        // what the manifest and the layouts name by class name has to stay
        if shrinks {
            system::create_dir(&proguard_dir)?;
            extra.extend(["--proguard".to_string(), aapt2_rules.display().to_string()]);

            if needs_main_dex {
                extra.extend([
                    "--proguard-main-dex".to_string(),
                    main_dex_rules.display().to_string(),
                ]);
            }
        }

        if settings.application_id != build.namespace {
            extra.extend([
                "--rename-manifest-package".to_string(),
                settings.application_id.clone(),
            ]);
        }

        let mut asset_dirs = build.merged_asset_dirs();

        if let Some(name) = &settings.license_report {
            // like the Gradle license plugin with copyJsonReportToAssets
            let generated = intermediates.join("generated/assets");
            system::write_file(
                &generated.join(name),
                &build.inputs.license_report(TargetPlatform::Android)?,
            )?;
            asset_dirs.push(generated);
        }

        for assets in asset_dirs {
            extra.extend(["-A".to_string(), assets.display().to_string()]);
        }

        build.link(
            self.manifest,
            &own_resources,
            LinkedResources::App,
            &linked,
            &symbols,
            &extra,
        )?;

        // an app bundle has its resources in protobuf format
        let proto_resources = intermediates.join("bundle/resources.zip");

        if self.bundle.is_some() {
            let mut proto_extra = extra.clone();
            proto_extra.push("--proto-format".to_string());

            build.link(
                self.manifest,
                &own_resources,
                LinkedResources::App,
                &proto_resources,
                &intermediates.join("bundle/R.txt"),
                &proto_extra,
            )?;
        }

        // the app generates the final R classes of all libraries
        let mut generated_sources = build.write_r_classes(
            &symbols,
            Some(&SymbolTable::read(&symbols)?),
            &intermediates.join("generated/r"),
        )?;

        generated_sources.push(build.write_build_config(&settings.build_config)?);
        let generated = intermediates.join("classes/generated");
        build.compile_generated_java(
            &generated_sources,
            &build.android_classpath(&[]),
            &generated,
        )?;

        let classes = intermediates.join("classes/kotlin");
        let generated_resources = build.compile_sources(&[generated.clone()], &classes)?;
        build.build_native_libraries()?;

        let own_jars = vec![
            build.jar(&classes, &intermediates.join("jars/classes.jar"))?,
            build.jar(&generated, &intermediates.join("jars/generated.jar"))?,
        ];

        // the app's own Java resources come first, the first jar wins
        let module_resources = intermediates.join("java-resources-module");
        build
            .inputs
            .resources
            .process(&generated_resources, &module_resources)?;

        let mut resource_jars =
            vec![build.jar(&module_resources, &intermediates.join("jars/resources.jar"))?];

        resource_jars.extend(own_jars.iter().chain(&build.jars).cloned());
        let java_resources = intermediates.join("java-resources");
        merge_jars(
            &resource_jars,
            JarContents::JavaResources,
            &java_resources,
            &intermediates.join("java-resources.tmp"),
        )?;

        let dex_dir = intermediates.join("dex");

        let desugared = match &settings.core_library_desugaring {
            Some(version) => {
                Some(build.desugared_library(version, &intermediates.join("desugar"))?)
            }
            None => None,
        };

        let mut library_keep_rules = None;

        if shrinks {
            system::log(&build.inputs.name, "Shrinking and dexing with R8");
            let rules = release::proguard_rule_files(
                build,
                &settings.proguard_files,
                &aapt2_rules,
                &java_resources,
                &proguard_dir,
            )?;

            library_keep_rules = release::shrink_with_r8(
                build,
                &own_jars,
                &rules,
                Some(main_dex_rules.as_path()).filter(|_| needs_main_dex),
                desugared.as_ref(),
                &dex_dir,
            )?;
        } else {
            system::log(&build.inputs.name, "Dexing");
            build.dex(&own_jars, &dex_dir, desugared.as_ref())?;
        }

        if let Some(desugared) = &desugared {
            build.dex_desugared_library(
                desugared,
                library_keep_rules.as_deref(),
                &dex_dir,
                &intermediates.join("desugar/dex"),
            )?;
        }

        system::log(&build.inputs.name, "Packaging");
        let native_libraries = intermediates.join("native-libraries");
        build.merge_native_libraries(&native_libraries)?;

        build.package(
            &linked,
            &dex_dir,
            &java_resources,
            &native_libraries,
            &intermediates.join("apk"),
            self.signature,
            self.output,
        )?;

        if let Some(bundle) = self.bundle {
            system::log(&build.inputs.name, "Building the app bundle");
            let mapping = build.build_dir.join("outputs/mapping/release/mapping.txt");

            release::build_app_bundle(
                build,
                &proto_resources,
                &dex_dir,
                &java_resources,
                &native_libraries,
                Some(mapping.as_path()).filter(|_| shrinks),
                self.signature,
                bundle,
            )?;
        }

        return Ok(merged_manifest);
    }
}

/// The files the parameter `name` lists, relative to the module. A missing one is an error.
fn required_module_files(
    args: &Arguments,
    name: &str,
    module_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut files = vec![];

    for file in args.get::<Vec<String>>(name)? {
        let file = module_dir.join(file);

        if !file.is_file() {
            return Err(format!("{name}: {} does not exist", file.display()));
        }

        files.push(file);
    }

    return Ok(files);
}

fn android_sdk(args: &Arguments) -> Result<AndroidSdk, String> {
    return AndroidSdk::ensure(
        args.get("compile_sdk")?,
        &args.get::<String>("build_tools")?,
        args.get("accept_sdk_license")?,
    );
}

/// The unit tests of src/test on the JUnit Platform, and the instrumented tests of
/// src/androidTest in an apk, which runs on a device once the script is built
fn android_tests(
    context: &BuildScriptContext,
    args: &Arguments,
    inputs: &ModuleBuildInputs,
    sdk: &AndroidSdk,
    classes: Vec<PathBuf>,
    settings: &ApkSettings,
) -> Vec<TestSuite> {
    if !context.tests.includes(&inputs.name) {
        return vec![];
    }

    let unit_tests = UnitTests {
        inputs,
        args,
        classes,
        platform: TargetPlatform::Android,
        android_jar: Some(sdk.android_jar.clone()),
    };

    let instrumented = instrumented_tests(context, args, sdk, settings).unwrap_or_else(|error| {
        ModuleTests::Known(TestOutcome::Failed {
            counts: None,
            error,
        })
    });

    return vec![
        unit_tests.prepare(),
        TestSuite {
            source_set: "androidTest",
            tests: instrumented,
        },
    ];
}

/// The module and its instrumented tests in one apk that instruments itself, like with
/// Gradle. An app keeps its application id and takes the app's place while tests run.
fn instrumented_tests(
    context: &BuildScriptContext,
    args: &Arguments,
    sdk: &AndroidSdk,
    settings: &ApkSettings,
) -> Result<ModuleTests, String> {
    let mut inputs = read_android_inputs(context, args)?;

    let test_source_dirs: Vec<PathBuf> = args.get::<Vec<String>>("android_test_source_dirs")?
        .iter()
        .map(|dir| inputs.directory.join(dir))
        .collect();

    let (kotlin_tests, java_tests) = kotlin_and_java_sources(&test_source_dirs);

    if kotlin_tests.is_empty() && java_tests.is_empty() {
        return Ok(ModuleTests::Known(TestOutcome::NoTests));
    }

    let runner: String = args.get("test_instrumentation_runner")?;

    let mut test_dependencies =
        Dependencies::read(args, "android_test_dependencies", &inputs.kotlin_version)?;

    let names_runner = test_dependencies.libraries.iter()
        .any(|library| library.group == "androidx.test" && library.artifact == "runner");

    if runner == DEFAULT_TEST_RUNNER && !names_runner {
        test_dependencies
            .libraries
            .extend(MavenCoordinate::parse(DEFAULT_TEST_RUNNER_LIBRARY));
    }

    inputs.source_dirs.extend(test_source_dirs);
    inputs
        .dependencies
        .libraries
        .extend(test_dependencies.libraries);
    inputs
        .dependencies
        .platforms
        .extend(test_dependencies.platforms);
    inputs
        .dependencies
        .modules
        .extend(test_dependencies.modules);

    // where the Gradle plugin puts them
    let build_dir = system::build_dir(&inputs.directory);
    let manifest = build_dir.join("androidTest/intermediates/manifest/AndroidManifest.xml");
    let apk = build_dir.join(format!("outputs/apk/androidTest/debug/{}-debug-androidTest.apk", inputs.name));
    let reports_dir = build_dir.join("outputs/androidTest-results/connected");

    let mut input_paths = android_input_paths(&inputs, args, BuildType::Debug)?;
    input_paths.push(inputs.directory.join("src/androidTest"));

    let fingerprint =
        inputs.input_fingerprint(&format!("{}-androidTest", inputs.name), args, &input_paths)?;

    // the apk may be up to date, the tests run anyway: their device is an input too
    if fingerprint.is_up_to_date(&[&apk])? {
        system::log(&inputs.name, "Apk of the instrumented tests up to date");
    } else {
        let build = AndroidBuild::prepare_android_tests(inputs, args)?;

        let source_manifest =
            Some(build.main_dir.join("AndroidManifest.xml")).filter(|manifest| manifest.is_file());

        let test_manifest = test_manifest(&build.inputs.directory, settings, &runner)?;
        Apk {
            settings,
            source_manifest: source_manifest.as_deref(),
            test_manifest: Some(&test_manifest),
            manifest: &manifest,
            signature: &Signature::Debug,
            output: &apk,
            bundle: None,
        }
        .build(&build)?;
        fingerprint.store_done(&build.inputs.name, &apk)?;
    }

    return Ok(ModuleTests::Run(Box::new(InstrumentedTests {
        apk,
        sdk_root: sdk.root.clone(),
        package: settings.application_id.clone(),
        runner,
        min_sdk: args.get("min_sdk")?,
        reports_dir,
    })));
}

/// src/androidTest/AndroidManifest.xml with the <instrumentation> of the tests, which instrument their own apk
fn test_manifest(
    module_dir: &Path,
    settings: &ApkSettings,
    runner: &str,
) -> Result<XmlElement, String> {
    let source = module_dir.join("src/androidTest/AndroidManifest.xml");
    let mut manifest = match source.is_file() {
        true => read_manifest_with_placeholders(&source, &settings.application_id, &[])?,
        false => XmlElement {
            name: "manifest".to_string(),
            ..XmlElement::default()
        },
    };

    manifest.children.push(XmlElement {
        name: "instrumentation".to_string(),
        attributes: vec![
            ("android:name".to_string(), runner.to_string()),
            (
                "android:targetPackage".to_string(),
                settings.application_id.clone(),
            ),
            (
                "android:label".to_string(),
                format!("Tests for {}", settings.application_id),
            ),
        ],
        ..XmlElement::default()
    });

    return Ok(manifest);
}
