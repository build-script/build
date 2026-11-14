# Kotlin

`use kotlin` brings `build_kotlin`. It compiles Kotlin and Java with kotlinc and javac and brings kotlin-stdlib, which the modules depending on it get as well.

```kotlin
use kotlin

repositories = list(maven_central())

core = build_kotlin("core", list(), repositories, directory = "core")

app = build_kotlin(
    "app",
    list(core, "org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2"),
    repositories,
    main_class = "com.example.app.MainKt",
    directory = "app",
)
```

A Kotlin module is a [Java module](java.md) with Kotlin on top:
the same jars and fat jars, the same dependencies, repositories, tests, resources, generated sources and JDKs.
This page only shows what Kotlin adds.

## Parameters

In addition to the parameters of [build_java](java.md#parameters):

| Parameter        | Default    | Meaning                                              |
|------------------|------------|------------------------------------------------------|
| `kotlin_version` | `"2.4.21"` | Version of the Kotlin compiler and of kotlin-stdlib  |
| `plugins`        | `[]`       | Compiler plugins, by short name or Maven coordinates |
| `plugin_options` | `[]`       | `-P` options of the plugins                          |
| `kotlin_args`    | `[]`       | Further `kotlinc` arguments, like `freeCompilerArgs` |
| `ksp`            | `[]`       | Kotlin symbol processors, like `ksp(...)`            |
| `ksp_options`    | `[]`       | `"key=value"` options of the symbol processors       |
| `ksp_version`    | `"2.3.12"` | Version of KSP2                                      |
| `test_ksp`       | `[]`       | Symbol processors of the tests                       |

The sources are in `src/main/kotlin` and `src/main/java`, the tests in `src/test/kotlin` and `src/test/java`. Tests see the `internal` declarations of their module.

Kotlin's own libraries can leave out the version and get the module's Kotlin version:
`"org.jetbrains.kotlin:kotlin-reflect"`.

## Compiler plugins

| Short name      | Plugin                                                          |
|-----------------|-----------------------------------------------------------------|
| `compose`       | Compose compiler, from Kotlin 2.0 on                            |
| `serialization` | kotlinx.serialization                                           |
| `parcelize`     | Parcelize, with its runtime                                     |
| `spring`        | all-open with the Spring preset, like `kotlin("plugin.spring")` |
| `jpa`           | no-arg with the JPA preset, like `kotlin("plugin.jpa")`         |

```kotlin
plugins = list("serialization", "dev.drewhamilton.poko:poko-compiler-plugin:0.23.2"),
plugin_options = list("plugin:dev.drewhamilton.poko:enabled=true"),
```

## Symbol processors

KSP2 runs before kotlinc:

```kotlin
ksp = list("androidx.room:room-compiler:2.8.5"),
ksp_options = list("room.schemaLocation=schemas"),
```

A processor can also be a module of the script, like `ksp(project(":codegen"))` in Gradle:

```kotlin
codegen = build_kotlin(
    "codegen",
    list("com.google.devtools.ksp:symbol-processing-api:2.3.12"),
    repositories,
    directory = "codegen"
)

build_kotlin("app", list(), repositories, ksp = list(codegen))
```

KSP runs incrementally: with the same processors, options and classpath it only gets the changed sources, and it does not run when nothing changed.

## Tests

`kotlin-test` binds to the framework the tests use, as with the Kotlin Gradle plugin.

```kotlin
test_dependencies = list("org.jetbrains.kotlin:kotlin-test"),
```
