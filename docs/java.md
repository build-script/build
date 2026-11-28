# Java

`use java` brings `build_java`, which compiles Java with javac into a jar.

```kotlin
use java

repositories = list(maven_central())

app = build_java(
    "app",
    list("com.google.code.gson:gson:2.14.0"),
    repositories,
    main_class = "com.example.app.Main",
)
```

The module is built into `build/libs/<name>.jar`, its libraries are copied to `build/libs/lib/`. The manifest lists them, so a jar with a `main_class` starts with `java -jar`.
`build_java` returns the module, so other modules can depend on it.

Everything on this page also applies to [Kotlin](kotlin.md) and, where it makes sense, to [Android](android.md).

## Parameters

| Parameter                    | Default              | Meaning                                                                                      |
|------------------------------|----------------------|----------------------------------------------------------------------------------------------|
| `name`                       | required             | Name of the module and of its jar                                                            |
| `dependencies`               | required             | Coordinates, platforms and modules, like `implementation` in Gradle                          |
| `repositories`               | required             | Where to look for Maven artifacts                                                            |
| `main_class`                 | none                 | Main class, which makes the module startable with `run`                                      |
| `fat_jar`                    | `false`              | Also builds `build/libs/<name>-all.jar` with the libraries inside, see [Fat jars](#fat-jars) |
| `relocate`                   | `[]`                 | `"from=to"`: packages of the libraries moved inside the fat jar                              |
| `compile_only`               | `[]`                 | Only to compile against, like `compileOnly` in Gradle                                        |
| `annotation_processors`      | `[]`                 | javac annotation processors, like `annotationProcessor`                                      |
| `jdk`                        | `"21"`               | Java version, the result of `jdk(...)` or a JDK directory                                    |
| `directory`                  | `"."`                | Module directory, relative to the script                                                     |
| `source_dirs`                | `src/main/java`      | Source directories                                                                           |
| `generated_sources`          | `[]`                 | Directories from `generate_sources(...)`                                                     |
| `resource_dirs`              | `src/main/resources` | Resource directories                                                                         |
| `resource_files`             | `[]`                 | `"path in the jar=file or directory"`                                                        |
| `resource_texts`             | `[]`                 | `"path in the jar=content"`                                                                  |
| `resource_tokens`            | `[]`                 | `"name=value"`: `@name@` in text resources becomes `value`                                   |
| `test_dependencies`          | `[]`                 | Libraries of the tests, like `testImplementation`                                            |
| `test_source_dirs`           | `src/test/java`      | Test sources                                                                                 |
| `test_annotation_processors` | `[]`                 | javac processors of the tests                                                                |
| `test_main_class`            | none                 | Runs the tests with this class instead of the JUnit Platform                                 |

> A `.kt` file among the sources is an error: modules with Kotlin are built with [build_kotlin](kotlin.md).

## Dependencies and repositories

```kotlin
repositories = list(
    google(),
    maven_central(),
    maven("https://jitpack.io"),
    maven_local()
)

dependencies = list(
    "com.squareup.okhttp3:okhttp:5.5.0",                    // group:artifact:version[:classifier]
    platform("org.springframework.boot:spring-boot-dependencies:4.0.0"),  // a BOM
    "org.springframework:spring-core",                      // the version comes from the BOM
    core,                                                   // a module of the script
)
```

Libraries are resolved the way Gradle does it, from POMs and Gradle module metadata: the highest requested version wins, BOMs and constraints apply, exclusions count, and Kotlin Multiplatform and AndroidX libraries get their variant for the JVM or for Android.
A module passes its libraries on to the modules that depend on it.
Shared libraries of `build_c` and `build_cpp` among the dependencies are for `System.loadLibrary`: `run` and the tests find them, and the jar packs those with `targets` for each of them, see [cpp.md](cpp.md#shared-libraries-and-jni).

`maven(url)` also takes a directory relative to the script. `maven_local()` is `~/.m2/repository`.

**Downloads** are checked against the `.sha1` the repository keeps next to every file, before they go into the cache. A file that does not match stops the build, and a repository without checksums is trusted, as Maven does.
[project.lock](commands.md#projectlock) next to the script keeps the SHA-256 of every file, so every machine builds with the same files.

**Credentials** come from environment variables. The script only names them, the values are read when something is downloaded and go only to that server: a redirect to another host does not get them.
They never show up in a script, a log or the process list:

```kotlin
maven(
    "https://nexus.example.com/repository/releases",
    username_env = "NEXUS_USER",
    password_env = "NEXUS_TOKEN"
)
```

**SNAPSHOTs** resolve to the newest build of the repository, by its `maven-metadata.xml`:
`spigot-api:26.3-R0.1-SNAPSHOT` becomes `spigot-api-26.3-R0.1-20261004.024323-9.jar`. `project.lock` keeps that build, `--refresh` takes the newest again.
A newer build rebuilds the modules that use it.

## Tests

`xkit test` compiles `src/test/java` against the module and runs it on the JUnit Platform.
List only the test framework, xkit adds its engine:

| Framework               | Runs with                                    |
|-------------------------|----------------------------------------------|
| JUnit 5 and 6           | the Jupiter engine in the version of the API |
| JUnit 4                 | the vintage engine                           |
| TestNG                  | the TestNG engine                            |
| Kotest, jqwik, Cucumber | they are engines themselves                  |

Any other engine runs once it is in `test_dependencies`. Reports go to `build/test-results`. Tests that passed and whose inputs did not change do not run again.

A framework without an engine runs with `test_main_class`: that class starts with the classpath of the tests, and its exit code decides.

## Annotation processors

Processors like Dagger, Lombok or MapStruct run with javac on the Java sources:

```kotlin
compile_only = list("org.projectlombok:lombok:1.18.42"),
annotation_processors = list("org.projectlombok:lombok:1.18.42", "org.mapstruct:mapstruct-processor:1.6.3"),
```

A processor can also be a module of the script.

## Resources

Resources are collected in `build/resources/main`, like with Gradle's `processResources`, and end up in the jar next to the classes:

```kotlin
resource_files = list("README.md=../docs/README.md", "scripts=./docs/scripts"),
resource_texts = list("version.txt=" + version),
resource_tokens = list("version=" + version),        // @version@ -> 1.5.0
```

- Paths in the jar are relative and without `..`.
- Two resources for one path are an error.
- Tokens replace only the declared `@name@`, and only in text files.
- Files like `.DS_Store` are left out.

## Generated sources

`generate_sources` runs a code generator from Maven before the module is compiled and returns the directory it wrote to:

```kotlin
protos = generate_sources(
    "wire",
    tool = list("com.squareup.wire:wire-compiler:7.1.0"),
    list(maven_central()),
    main_class = "com.squareup.wire.WireCompiler",
    args = list("--proto_path=src/main/proto", "--java_out={output}"),
    inputs = list("src/main/proto"),
)

build_java(
    "app",
    list("com.squareup.wire:wire-runtime:7.1.0"),
    list(maven_central()),
    generated_sources = list(protos)
)
```

`{output}` stands for the output directory, and `inputs` count for the up-to-date check. A generator without a command line can be driven by a small Java class in `tool_sources`.
For generators that are not on Maven, see [build_command](plugins.md#build_command).

## JDKs

`jdk` is a Java version, a JDK directory or the result of `jdk(...)`. Versions are installed from the [foojay](https://foojay.io) API, Temurin by default, with a checked SHA-256:

```kotlin
jdk = "17"
jdk = jdk("25", distribution = "corretto")    // any foojay distribution
jdk = "tools/jdk-21"                         // an existing JDK, relative to the script
```

The bytecode level matches the JDK, like `jvmToolchain(...)` in Gradle.

## Fat jars

With `fat_jar = true` the module also gets `build/libs/<name>-all.jar`, like the shadow plugin of Gradle: its classes and resources, those of the modules it depends on and its libraries.
`compile_only` libraries stay out, which suits plugins whose server provides the API.

```kotlin
plugin = build_java(
    "plugin",
    list("com.google.code.gson:gson:2.14.0", "org.bstats:bstats-bukkit:3.2.1"),
    repositories,
    compile_only = list("org.spigotmc:spigot-api:26.3-R0.1-SNAPSHOT"),
    fat_jar = true,
    relocate = list("org.bstats=dev.example.plugin.libs.bstats"),
)
```

- `run` starts the fat jar. Modules depending on the module get its plain jar. There is no `build/libs/lib/` then.
- When several jars have the same file, the first one wins: the module, then its modules, then the libraries. Classes that differ between jars are counted in the log.
- `META-INF/services` files are concatenated.
- Signatures, manifests and `module-info.class` of the libraries are left out. `META-INF/versions` stays, the manifest then says `Multi-Release: true`.
- `relocate` moves packages in the bytecode, in string constants, in resource paths and in the service files, with [jar-relocator](https://github.com/lucko/jar-relocator). Plugins that bring the same library then do not clash.
- Spring Boot's `spring.factories` and Log4j's plugin caches are not merged. Spring Boot apps keep the plain jar with `build/libs/lib/`.

## Outputs

| Output                           | Where                                      |
|----------------------------------|--------------------------------------------|
| Jar of the module, its libraries | `build/libs/<name>.jar`, `build/libs/lib/` |
| Fat jar                          | `build/libs/<name>-all.jar`                |
| Test reports                     | `build/test-results/`                      |
| Generated sources                | `build/generated/`                         |
