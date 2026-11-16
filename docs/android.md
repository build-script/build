# Android

`use android` brings `build_android` for apps and `build_android_library` for library modules.
They compile Kotlin and Java like [build_kotlin](kotlin.md) and take its parameters from `compile_only` on, including the test parameters.

```kotlin
use android

repositories = list(google(), maven_central())

ui = build_android_library(
    "ui",
    "com.example.ui",                        // namespace
    list(
        platform("androidx.compose:compose-bom:2026.09.00"),
        "androidx.compose.material3:material3"
    ),
    repositories,
    plugins = list("compose"),
    directory = "ui",
)

app = build_android(
    "app",
    "com.example.app",
    list(ui, "androidx.activity:activity-compose:1.13.0"),
    repositories,
    plugins = list("compose"),
    version_code = 3,
    version_name = "1.2",
)
```

`build_android` builds a signed debug apk into `build/outputs/apk/debug/<name>-debug.apk`: resources with aapt2, Kotlin and Java against `android.jar`, dex with d8, and the Java resources, assets and native libraries of the app and of all its libraries.
`build_android_library` builds a library that apps use like an aar. `build --release` builds for release, see [Release builds](#release-builds).

> The debug source set (`src/debug/...`) overlays `src/main`, as with the Android Gradle plugin.

## Parameters

| Parameter                     | Default                                          | Meaning                                                                                   |
|-------------------------------|--------------------------------------------------|-------------------------------------------------------------------------------------------|
| `name`, `namespace`           | required                                         | Module name and package of R and BuildConfig                                              |
| `min_sdk`                     | `24`                                             | Lowest API level                                                                          |
| `compile_sdk`                 | `36`                                             | API level to compile against                                                              |
| `view_binding`                | `false`                                          | `<Layout>Binding` classes, also for the layouts of library modules                        |
| `non_transitive_r_class`      | `true`                                           | Each R class has only the resources of its own module, like `android.nonTransitiveRClass` |
| `build_config_fields`         | `[]`                                             | `"Type NAME value"`, like `buildConfigField`                                              |
| `manifest_placeholders`       | `[]`                                             | `"name=value"` for `${name}` in the manifests                                             |
| `res_dirs`                    | `src/main/res`, `src/debug/res`                  | Android resources                                                                         |
| `asset_dirs`                  | `src/main/assets`, `src/debug/assets`            | Assets                                                                                    |
| `aidl_dirs`                   | `src/main/aidl`, `src/debug/aidl`                | AIDL interfaces                                                                           |
| `targets`                     | all four ABIs                                    | The ABIs of its native libraries, like `list("android-aarch64", "android-x86_64")`        |
| `cmake`                       | none                                             | `CMakeLists.txt` of native libraries, built with the NDK for every ABI of `targets`       |
| `ndk_version`                 | `"28.2.13676358"`                                | NDK for `cmake` and the libraries of `build_c` and `build_cpp`                            |
| `build_tools`                 | `"36.1.0"`                                       | Version of the build tools                                                                |
| `accept_sdk_license`          | `false`                                          | Accepts the SDK license where there is no terminal to ask                                 |
| `android_test_dependencies`   | `[]`                                             | Libraries of the instrumented tests, like `androidTestImplementation`                     |
| `android_test_source_dirs`    | `src/androidTest/kotlin`, `src/androidTest/java` | Instrumented tests                                                                        |
| `test_instrumentation_runner` | `"androidx.test.runner.AndroidJUnitRunner"`      | Runner of the instrumented tests                                                          |

> Native libraries come from `jniLibs`, from `cmake`, or from shared libraries of `build_c` and `build_cpp` among the `dependencies`, which the script builds for the ABIs of the app and its `min_sdk`, see [cpp.md](cpp.md#shared-libraries-and-jni). The apk takes those of its `targets`.

Only `build_android`:

| Parameter                      | Default                 | Meaning                                                                  |
|--------------------------------|-------------------------|--------------------------------------------------------------------------|
| `application_id`               | the namespace           | Application ID of the apk                                                |
| `target_sdk`                   | `compile_sdk`           | Target API level                                                         |
| `version_code`, `version_name` | `1`, `"1.0"`            | Version of the apk                                                       |
| `core_library_desugaring`      | none                    | Version of `desugar_jdk_libs`, for `java.time` and others on old devices |
| `license_report`               | none                    | Asset name of a JSON report of the libraries' licenses                   |
| `minify`                       | `true`                  | Release builds shrink, optimize and rename the code with R8              |
| `proguard_files`               | `[]`                    | The app's own ProGuard rules for R8, relative to the module              |
| `app_bundle`                   | `false`                 | Release builds also build an app bundle for Google Play                  |
| `keystore`                     | none                    | Keystore of the release key, relative to the script                      |
| `key_alias`                    | none                    | Alias of the release key                                                 |
| `keystore_password_env`        | none                    | Environment variable with the password of the keystore                   |
| `key_password_env`             | `keystore_password_env` | Environment variable with the password of the key                        |

Only `build_android_library`:

| Parameter                 | Default | Meaning                                                         |
|---------------------------|---------|-----------------------------------------------------------------|
| `consumer_proguard_files` | `[]`    | ProGuard rules for release builds of the apps using the library |

## Release builds

A release build only happens on purpose: `build --release` or `run --release`. The script only says how to build it, never whether. Tests always run against debug builds.

```kotlin
app = build_android(
    "app",
    "com.example.app",
    dependencies,
    repositories,
    proguard_files = list("proguard-rules.pro"),
    keystore = "release.jks",
    key_alias = "upload",
    keystore_password_env = "ANDROID_KEYSTORE_PASSWORD",
    app_bundle = true,
)
```

```bash
ANDROID_KEYSTORE_PASSWORD=... build --release
```

What changes for release:

- The app is not debuggable, `BuildConfig.DEBUG` is `false` and `BUILD_TYPE` is `"release"`.
- `src/release` overlays `src/main` instead of `src/debug`.
- Native libraries are built by CMake as `Release`.
- R8 shrinks, optimizes and renames the code of the app and of all its libraries, then dexes it. The mapping of the renamed classes goes to `build/outputs/mapping/release/mapping.txt`, all rules together to `configuration.txt` next to it. `minify = false` dexes with d8 instead.
- The apk is signed with the release key. Without a `keystore` it stays unsigned, `<name>-release-unsigned.apk`, and `run --release` refuses it.
- With `app_bundle = true` there is also an app bundle for Google Play, built with [bundletool](https://developer.android.com/tools/bundletool) and checked by its `validate`. It has one module, `base`, and the mapping, so Google Play shows readable stack traces. It is signed with jarsigner and the same key, without a `keystore` it stays unsigned too.

R8 gets its rules like with the Android Gradle plugin:

1. build-script's own, like `proguard-android-optimize.txt`
2. those aapt2 writes for the classes the manifest and the layouts name
3. `proguard.txt` of the aars and `consumer_proguard_files` of the library modules
4. the rules libraries keep in their jars, in `META-INF/com.android.tools` and `META-INF/proguard`
5. rules that keep the classes of `META-INF/services`
6. the app's `proguard_files`

The passwords stay out of the script, the logs and the process list: the script only names the environment variables, apksigner and jarsigner read them themselves.
A variable that is not set is an error.

## The SDK, devices and emulators

- An Android SDK in `ANDROID_HOME` is used when it is complete. Otherwise build-script installs what it needs into its own SDK, once its license is accepted: in the terminal, or with `accept_sdk_license = true`.
- `run` installs the app with `adb` and starts its launcher activity. With several devices `ANDROID_SERIAL` picks one.
- Without a device, build-script offers system images and sets up an emulator. `BUILD_SCRIPT_EMULATOR_ARGS` passes options to it, like `-no-window`.

## How the app is put together

- Library manifests are merged into the app's manifest. Entries with `tools:node="remove"` are left out, other `tools:` attributes are dropped.
- R classes are written the way the Gradle plugin writes them: one per package, with the ids of the app. A library may use resources only the app brings, references are checked when the app is linked.
- View bindings, BuildConfig and AIDL interfaces are generated like with the Gradle plugin.
- Libraries are dexed once and kept until one of them changes. Every build dexes only the own classes and those of the library modules.

## Tests

- **Unit tests** in `src/test` run on the JUnit Platform like those of [Kotlin](kotlin.md#tests). They compile against `android.jar`, whose methods throw `Stub!`: tests that call Android need a fake.
- **Instrumented tests** in `src/androidTest` run on a device. The module and its tests become one apk that instruments itself. An app keeps its application id, a library gets `<namespace>.test`. build-script uninstalls it afterwards. The tests run every time, the device is an input too.

```kotlin
test_dependencies = list("junit:junit:4.13.2"),
android_test_dependencies = list("androidx.test.ext:junit:1.3.0", "androidx.test.espresso:espresso-core:3.7.0"),
```

> With the default runner, build-script adds `androidx.test:runner` when it is missing.

## Outputs

| Output                        | Where                                                                                       |
|-------------------------------|---------------------------------------------------------------------------------------------|
| Debug apk                     | `build/outputs/apk/debug/<name>-debug.apk`                                                  |
| Release apk                   | `build/outputs/apk/release/<name>-release.apk`, unsigned `<name>-release-unsigned.apk`      |
| App bundle                    | `build/outputs/bundle/release/<name>-release.aab`, unsigned `<name>-release-unsigned.aab`   |
| Mapping of a release build    | `build/outputs/mapping/release/mapping.txt`                                                 |
| Apk of the instrumented tests | `build/outputs/apk/androidTest/debug/<name>-debug-androidTest.apk`                          |
| Test reports                  | `build/test-results/`, of instrumented tests `build/outputs/androidTest-results/connected/` |
| Library jar                   | `build/libs/<name>.jar`                                                                     |
