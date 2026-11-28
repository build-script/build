# build-script

build-script is a build tool that is not tied to one language. It aims to build all of them, with one small build language and one command line.

It starts with the languages whose builds hurt the most today. That is why Java comes first: it has no official build tool, only a third-party one (Maven) and a third party on top of that (Gradle).
Neither ever made building easy. How many Gradle builds work on the first try, without installing something first?
And working outside an IDE is close to impossible, because the language that builds the jar is so expressive that hardly anyone understands it anymore.

Building should be simple and reproducible, and it does not need a general-purpose language with IO and HTTP.
Small helper functions, so large projects do not repeat themselves, and managed dependencies are enough. build-script brings these together: one language and one tool, only for building.

```kotlin
use kotlin

build_kotlin(
    "hello",
    list("com.google.code.gson:gson:2.14.0"),
    list(maven_central()),
    main_class = "MainKt",
)
```

```bash
build run
```

build-script downloads what a build needs by itself: the JDK, the compilers, the libraries, the Android SDK and, when needed, an emulator.

# Installation

Every [release](https://github.com/build-script/build/releases) has build-script for Linux, macOS and Windows, with their SHA-256 checksums in `SHA256SUMS`. Unpack the archive for your system and put `build` into a directory on your `PATH`:

```bash
curl -L https://github.com/build-script/build/releases/latest/download/build-script-aarch64-apple-darwin.tar.gz | tar xz
mv build-script-aarch64-apple-darwin/build ~/.local/bin/
```

| System               | Archive                                          |
|----------------------|--------------------------------------------------|
| macOS, Apple silicon | `build-script-aarch64-apple-darwin.tar.gz`       |
| macOS, Intel         | `build-script-x86_64-apple-darwin.tar.gz`        |
| Linux, x86-64        | `build-script-x86_64-unknown-linux-musl.tar.gz`  |
| Linux, ARM           | `build-script-aarch64-unknown-linux-musl.tar.gz` |
| Windows              | `build-script-x86_64-pc-windows-msvc.zip`        |

The Linux binaries are static and run on every distribution. On macOS, a `build` downloaded with a browser needs `xattr -d com.apple.quarantine build` first, because it is not notarized.

To build it from source, with Rust:

```bash
git clone https://github.com/build-script/build.git build-script
cd build-script
cargo build --release
cp target/release/build-script ~/.local/bin/build
```

The command is `build`. Cargo names the file `build-script`, because a program called `build` would clash with Cargo's own `target/debug/build/`. `build --version` prints the version.

build-script downloads, unpacks and verifies what it needs itself, without tools of the system, on Linux even without CA certificates. C and C++ modules use the compiler of the system unless `toolchain = "zig"` or `toolchain = "gcc"` brings one. vcpkg ports and `git()` need `git`, and on Linux vcpkg also needs `curl`, `zip`, `unzip` and `tar`.

# Languages

A script loads a language with `use`:

| Language  | `use`              | Builds                                  | Docs                                                               |
|-----------|--------------------|-----------------------------------------|--------------------------------------------------------------------|
| Java      | `use java`         | jars                                    | [docs/java.md](docs/java.md)                                       |
| Kotlin    | `use kotlin`       | jars                                    | [docs/kotlin.md](docs/kotlin.md)                                   |
| Android   | `use android`      | apks, app bundles and Android libraries | [docs/android.md](docs/android.md)                                 |
| C and C++ | `use c`, `use cpp` | static and shared libraries, programs   | [docs/cpp.md](docs/cpp.md)                                         |
| Go        | `use go`, a plugin | programs, also for other systems        | [plugins/go](https://github.com/build-script/plugins/tree/main/go) |

Other languages come as plugins: build scripts that drive the compiler and tools of a language. `use go` loads the official Go plugin, `use "owner/repository"` a plugin from GitHub.
How plugins work and how to write one is explained in [build-script/plugins](https://github.com/build-script/plugins), the functions they are made of in [docs/plugins.md](docs/plugins.md).

The build language itself is described in [docs/language.md](docs/language.md), every command in [docs/commands.md](docs/commands.md).

## Commands for every language

| Command                                                  | What it does                                                                        |
|----------------------------------------------------------|-------------------------------------------------------------------------------------|
| `build [<script>]`                                       | Builds every module. Modules that did not change are up to date.                    |
| `build run [--module <name>] [<script>] [-- <args>...]`  | Builds, then starts a module.                                                       |
| `build test [--module <name>] [<script>] [-- <args>...]` | Builds, then runs the tests of every module, or of one.                             |
| `build new <kind> <path>`                                | Creates a project, like `cargo new`: of a language, a plugin or an empty workspace. |
| `build add <kind> <name> [<script>]`                     | Adds a module next to the script and its build call to the script.                  |
| `build clean [<script>]`, `clean --cache`                | Removes what build-script built, `--cache` also the downloads.                      |
| `build config [<setting> [<value>]]`                     | Shows or changes the settings, like your domain.                                    |
| `build plugins`                                          | Lists the official plugins and their templates.                                     |

Without a script, the commands use `project.build` in the current directory. The first build writes `project.lock` next to it, like `Cargo.lock`: the SHA-256 of every file it downloaded and the commit of every plugin, so later builds and other machines get the same. Keep it in version control.
`build`, `run` and `test` build modules that do not depend on each other at the same time, `--jobs <n>` sets how many.

The commands are the same for every language, what they do depends on the language: `run` starts a jar with its JDK, but installs an Android app on a device.
`new` and `add` know the project and module kinds of every language and plugin. A workspace starts empty and takes modules of any language.

## Commands for some languages

| Command                                               | Languages             | What it does                                                                                         |
|-------------------------------------------------------|-----------------------|------------------------------------------------------------------------------------------------------|
| `build install <group:artifact[:version]>`            | Java, Kotlin, Android | Adds a Maven library to a module, after checking that it resolves.                                   |
| `build install <port>`                                | C and C++             | Adds a vcpkg port to a module.                                                                       |
| `build idea [<script>]`                               | Java, Kotlin, Android | Sets the project up for IntelliJ, every build after keeps it current.                                |
| `--refresh` with `build`, `run`, `test` and `install` | all                   | Asks the repositories again for new versions, SNAPSHOTs and plugin releases.                         |
| `--release` with `build` and `run`                    | Android, C and C++    | Builds for release: Android apps with R8, signed with the script's key, app bundles for Google Play. |

# Notes

What build-script does not do yet, or what is not known yet:

- Its regression scenarios, real projects in Java, Kotlin, Android, C, C++ and Go, build on Linux, Windows and macOS. Other Unix systems still have to be tested.
- On Windows, C and C++ build with clang and the libraries of Visual Studio. vcpkg ports do not build with `zig` there.
- Android builds debug and release apks and app bundles of one module, but no dynamic feature modules, product flavors, further build types, data binding or Robolectric.
- No Kotlin Multiplatform modules, only the JVM and Android variants of their libraries. No kapt: use KSP or javac annotation processors.
- No publishing to Maven repositories.
- C and C++ build for other targets with zig and the NDK. The ABI of MSVC builds only on Windows itself, and programs for macOS from other machines go without the frameworks of its SDK. CMake projects do not build for Android yet.
- `toolchain = "gcc"` builds only for this machine: on macOS, on Linux for x86_64 and aarch64, on Windows for x86_64.
- The build language has no loops. A function that calls itself has to end with an `if` and may go at most 64 calls deep.
- Modules that share a directory must not build at the same time: give them `--jobs 1`.

# License

MIT, see [LICENSE](LICENSE).
