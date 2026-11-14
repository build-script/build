# C and C++

`use cpp` brings `build_cpp`: a static or shared library or a program of C and C++, with libraries from [vcpkg](https://vcpkg.io). `use c` brings `build_c`, the same for C alone: it takes `.c` files and links with the C compiler, without the C++ standard library, unless a module of C++ is among what it links.

```kotlin
use cpp

core = build_cpp(
    "core",
    list("fmt"),
    directory = "core",
    test_dependencies = list("gtest"),
)

app = build_cpp(
    "app",
    list(core, "spdlog"),
    program = true,
    directory = "app",
)
```

A library goes to `build/lib/lib<name>.a`, a program to `build/bin/<name>`, which `build run` starts in its module directory. On Windows they are `build/lib/<name>.lib` and `build/bin/<name>.exe`. A script with C and C++ modules has both `use` lines.
`build --release` builds with `-O2`, other builds with `-O0 -g`.

## Parameters

| Parameter            | Default     | Meaning                                                                                                               |
|----------------------|-------------|-----------------------------------------------------------------------------------------------------------------------|
| `name`               | required    | Name of the module and of its library or program                                                                      |
| `dependencies`       | required    | vcpkg ports, like `"fmt"`, and modules of the script                                                                  |
| `program`            | `false`     | A program that `run` starts, otherwise a static library                                                               |
| `shared`             | `false`     | A shared library, `lib<name>.dylib`, `.so` or `<name>.dll`, see [Shared libraries and JNI](#shared-libraries-and-jni) |
| `jni`                | `false`     | `jni.h` of the JDK                                                                                                    |
| `toolchain`          | `"system"`  | `"system"` or `"zig"`, see [Toolchains](#toolchains)                                                                  |
| `cpp_standard`       | `"c++20"`   | `-std` of the C++ sources, only `build_cpp`                                                                           |
| `c_standard`         | `"c17"`     | `-std` of the C sources                                                                                               |
| `defines`            | `[]`        | `"NAME"` or `"NAME=value"`, for the module itself                                                                     |
| `public_defines`     | `[]`        | Defines also for the modules depending on it                                                                          |
| `flags`              | `[]`        | Further compiler flags, their paths relative to the module directory                                                  |
| `link_flags`         | `[]`        | Further linker flags, like `"-framework", "Cocoa"`, also for the modules depending on a library                       |
| `directory`          | `"."`       | Module directory, relative to the script                                                                              |
| `sources`            | `"src/**"`  | Sources by pattern, see [Sources](#sources)                                                                           |
| `exclude`            | `[]`        | Patterns of sources the module leaves out                                                                             |
| `include_dirs`       | `include`   | Headers the modules depending on it see                                                                               |
| `configured_headers` | `[]`        | Templates like `"src/config.h.in"`, see [Generated headers](#generated-headers)                                       |
| `configured_values`  | `[]`        | Their values: `"NAME=value"`, or `"NAME"`, which is 1                                                                 |
| `export_header`      | `""`        | `"cmark"` writes `cmark_export.h` with `CMARK_EXPORT`                                                                 |
| `tablegen`           | `[]`        | Headers of TableGen, see [TableGen](#tablegen)                                                                        |
| `test_dependencies`  | `[]`        | vcpkg ports and modules of the tests                                                                                  |
| `test_sources`       | `"test/**"` | Test sources by pattern                                                                                               |
| `test_exclude`       | `[]`        | Patterns of test sources the tests leave out                                                                          |
| `test_programs`      | `[]`        | Patterns of test sources that are a program each, see [Tests](#tests)                                                 |
| `test_arguments`     | `[]`        | Arguments of the test programs, like `"--order", "decl"` for Catch2                                                   |
| `test_timeout`       | `600`       | Seconds a test program may run, then it is stopped and fails                                                          |

> `.c` files compile with the C compiler and `c_standard`, `.cpp`, `.cc` and `.cxx` files with the C++ compiler.

## Sources

A module compiles the sources its `sources` match, patterns relative to the module directory, `src/**` by default. Projects that keep their sources elsewhere name them:

```kotlin
// zlib: the library of the .c files next to the script, minigzip a program of one file in test/
zlib = build_c("z", list(), sources = list("*.c"), include_dirs = list("."))
build_c("minigzip", list(zlib), program = true, sources = list("test/minigzip.c"))
```

| Pattern          | Matches                                    |
|------------------|--------------------------------------------|
| `*.c`            | the `.c` files in the module directory     |
| `examples/a.cpp` | this file                                  |
| `**/*_test.cc`   | `_test.cc` files in every directory        |
| `testes/**`      | everything below `testes`                  |
| `port_?osix.cc`  | `?` is one character, like `port_posix.cc` |

- The directory of a pattern, up to its first `*` or `?`, is where its sources are searched and where the module finds its private headers: `src` of `src/**`, the module directory of `*.c`, `examples` of `examples/a.cpp`.
- `exclude` and `test_exclude` take the same patterns and leave those files out, like the tests among the sources, or files of another platform.
- A pattern of `sources` or `test_sources` that matches no source is an error, most likely it is misspelled. The defaults may match nothing: a library of only headers has no `src/`, a module without tests no `test/`.
- A module with other `sources` takes its tests only from `test_sources` of its own, not from `test/**`: modules that share a directory do not compile the tests of the others.
- A library without sources has only headers: modules depending on it get its `include_dirs`, nothing to link.
- The sources compile in parallel, of all modules together as many at a time as there are cores. What a compiler prints comes in one piece once it has finished, with its colors in a terminal.
- A source compiles again when it, one of its headers, its command line or the version of its compiler changed: after an update of Xcode or GCC everything compiles again.
- C programs on Linux link the math library, `-lm`, like C++ programs through the C++ standard library.
- On Windows the `system` toolchain is clang with the ABI of MSVC, see [Toolchains](#toolchains). Programs and shared libraries link the libraries of Windows that CMake and Visual Studio give every program, like `user32` and `shell32`, programs are for the console. Command lines longer than Windows takes go to clang in a response file.

For what differs between systems, a script asks `operating_system()`:

```kotlin
macos = operating_system() == "macos"

build_c(
    "lua",
    list(),
    program = true,
    sources = list("*.c"),
    exclude = list("onelua.c"),
    include_dirs = list("."),
    defines = list("LUA_COMPAT_5_3", if macos { "LUA_USE_MACOSX" } else { "LUA_USE_LINUX" }),
    link_flags = if macos { list("-lreadline") } else { list("-ldl") },
)
```

## New projects

```bash
build new cpp hello            # hello/project.build and hello/src/main.cpp
cd hello
build add cpp-library greet    # greet/include/greet/greeting.hpp and greet/src/greeting.cpp
build install fmt              # "fmt" in the dependencies of hello
build install doctest --test --module greet
```

`c` and `c-app`, `c-library` do the same in C. See [commands.md](commands.md#new-and-add).

## Modules

- A module depending on a library gets the library's headers and `public_defines` and links it, and everything the library needs.
- Modules depend on modules of `build_c` and `build_cpp`. Kotlin, Java and Android modules depend on their shared libraries, see [Shared libraries and JNI](#shared-libraries-and-jni).
- A [build_command](plugins.md#build_command) among the dependencies adds its `{output}` to the include path, and the C and C++ sources it wrote compile with the module: code generators that vcpkg does not have.

## Shared libraries and JNI

With `shared = true` a library becomes `build/lib/lib<name>.dylib`, on Linux `.so`, with what it needs inside. A program, a shared library and the tests that load shared libraries of other modules get them copied into their own `build/lib` and find them relative to themselves, by `@loader_path` or `$ORIGIN`: a `build/` directory runs wherever it is copied. Kotlin and Java modules take it among their `dependencies` and load it with `System.loadLibrary`: `run` and the tests start Java with its directory in `java.library.path`.

On Windows it is `build/bin/<name>.dll`, next to the programs that load it, and `build/lib/<name>.lib` links it. Windows exports only what `__declspec(dllexport)` marks, so build-script exports every function and variable of the library's own objects through a `.def` file, like `WINDOWS_EXPORT_ALL_SYMBOLS` of CMake. Programs reach its variables only through `__declspec(dllimport)`.

```kotlin
use cpp
use kotlin

// for System.loadLibrary("greeter"), with jni.h of the JDK
greeter = build_cpp("greeter", list(), shared = true, jni = true, directory = "greeter")

build_kotlin("app", list(greeter), repositories, main_class = "com.example.MainKt", directory = "app")
```

```cpp
#include <jni.h>

extern "C" JNIEXPORT jstring JNICALL Java_com_example_NativeKt_greet(JNIEnv *env, jclass, jstring name) { ... }
```

Android apps take it among their `dependencies` too: `build_android` builds it again with the clang of the NDK, for every ABI and the `min_sdk` of the app, into `build/android/<debug|release>/<abi>/lib<name>.so`, with libc++ inside, and packs it like the libraries of `jniLibs`:

```kotlin
greeter = build_cpp("greeter", list(), shared = true, directory = "greeter")

build_android("app", "com.example.app", list(greeter, ...), repositories)
```

- For Android the library gets its own sources, headers and `defines`. `flags` and `link_flags` are for this machine, vcpkg ports and libraries of other modules are not built for Android yet.
- The NDK is the `ndk_version` of the app, of `ANDROID_HOME`, else build-script installs it next to its Android SDK, once its license is accepted, like with `accept_sdk_license` of the app.

## Generated headers

Many CMake projects generate headers into the build: `configure_file` fills templates like `config.h.in`, `generate_export_header` writes the macros that mark what a library exports. build-script writes both into `build/generated/`, which the module and the modules depending on it see.

```kotlin
cmark = build_cpp(
    "cmark",
    list(),
    include_dirs = list("src"),
    // build/generated/cmark_version.h and build/generated/cmark_export.h
    configured_headers = list("src/cmark_version.h.in"),
    configured_values = list("PROJECT_VERSION_MAJOR=0", "PROJECT_VERSION_MINOR=31", "PROJECT_VERSION_PATCH=2"),
    export_header = "cmark",
)
```

| In the template          | Becomes                                                           |
|--------------------------|-------------------------------------------------------------------|
| `@NAME@`, `${NAME}`      | the value of `NAME`, empty without one                            |
| `#cmakedefine NAME rest` | `#define NAME rest` when `NAME` is true, else `/* #undef NAME */` |
| `#cmakedefine01 NAME`    | `#define NAME 1` or `#define NAME 0`                              |

- True is what CMake's `if()` takes for true: not empty, `0`, `OFF`, `NO`, `FALSE` or `NOTFOUND`.
- A header keeps its path below the include or source directory of its template, without `.in`: with `include_dirs = list(".")` the template `port/port_config.h.in` becomes `port/port_config.h`.
- The export header is that of a static library: `CMARK_EXPORT` is empty, `CMARK_DEPRECATED` marks deprecated functions.
- A header is only written when it changed, so the sources that include it do not compile again.

## CMake projects

`build_cmake` builds libraries that vcpkg does not have, or not in the version needed, from their sources, like those of `git(...)`. Modules depending on it get the headers and the static libraries it installed.

```kotlin
llvm = build_cmake(
    "llvm",
    git("https://github.com/llvm/llvm-project", "b08e005995ed29a2f89aa9a3f599a9d45db65523") + "/llvm",
    options = list("LLVM_ENABLE_PROJECTS=clang;lld", "LLVM_TARGETS_TO_BUILD=AArch64;X86"),
    // -fno-rtti and the libraries of the components, in their order
    compile_flags_from = list("bin/llvm-config", "--cxxflags"),
    link_flags_from = list("bin/llvm-config", "--ldflags", "--libs", "core", "orcjit", "native", "--system-libs"),
)

jit = build_cpp("jit", list(llvm), program = true)
```

| Parameter            | Default  | Meaning                                                                                                                                         |
|----------------------|----------|-------------------------------------------------------------------------------------------------------------------------------------------------|
| `name`               | required | Name of the module                                                                                                                              |
| `source`             | required | The directory with `CMakeLists.txt`                                                                                                             |
| `options`            | `[]`     | `-D` of CMake: `"NAME=value"`                                                                                                                   |
| `compile_flags_from` | `[]`     | A program of the installation and its arguments, whose output are the compile flags of the modules depending on it, else `-I` of its `include/` |
| `link_flags_from`    | `[]`     | The same for their link flags                                                                                                                   |
| `libraries`          | `[]`     | What they link besides, like `"z"` for `libz.a`. Without both every static library it installed.                                                |
| `link_flags`         | `[]`     | Further linker flags for the modules depending on it                                                                                            |

- CMake and Ninja are those vcpkg downloads for its ports. The compilers are those of the system, or those `CC` and `CXX` name. On Windows clang, with the static C library of release builds, like the ports.
- A source of `git()` builds once on the machine, in the cache: `caches/cmake/<name>-<hash>/`, its commit, options and compilers make the hash. Another project with the same LLVM takes it as it is. Other sources build into `build/cmake/<name>/` of the project. Both build for release, into `build/`, and install into `install/`.
- The output of `compile_flags_from` is without `-std=`: the module depending on it has its own `cpp_standard`.
- CMake configures again when the arguments changed, Ninja builds only what changed. The modules depending on it build again only when it built something.

`system_library` takes a library the system has, like the LLVM of Homebrew or apt, through a tool that prints its flags:

```kotlin
llvm_config = "/opt/homebrew/opt/llvm/bin/llvm-config"

llvm = system_library(
    "llvm",
    compile_flags_from = list(llvm_config, "--cxxflags"),
    link_flags_from = list(llvm_config, "--ldflags", "--libs", "support", "--system-libs"),
)
```

The tool is a path or a program of the `PATH`, like `pkg-config` with `"--cflags", "sdl2"`. Modules depending on it build again when the tool changed, like with an update of Homebrew.

## TableGen

Projects of LLVM, MLIR and Clang describe operations, options or tables in `.td` files, which TableGen turns into headers. `tablegen` writes them into `build/generated/`:

```kotlin
standalone = build_cpp(
    "standalone",
    list(llvm),
    sources = list("lib/Standalone/**"),
    tablegen = list(
        // the tool, the .td file, the header, the arguments of the tool
        list("mlir-tblgen", "include/Standalone/StandaloneOps.td", "Standalone/StandaloneOps.h.inc", "-gen-op-decls"),
        list("mlir-tblgen", "include/Standalone/StandaloneOps.td", "Standalone/StandaloneOps.cpp.inc", "-gen-op-defs"),
    ),
)
```

- The tool comes from the libraries of `build_cmake` and `system_library` the module depends on, their `bin/`, else from the `PATH`.
- It gets the `-I` of the module, those of LLVM among them: `include "mlir/IR/OpBase.td"` works.
- A header is written again when its `.td` file, one it includes or the arguments changed: TableGen names them in a depfile next to the header.

## Libraries from vcpkg

- Strings in `dependencies` are [vcpkg ports](https://vcpkg.io/en/packages). build-script installs vcpkg at the release `2026.10.16`, which pins every port to one version, and builds a port once: then it comes from the cache.
- Features of a port go in brackets: `"llvm[clang,lld]"`, `"sqlite3[fts5]"`. `install` checks that the port has them.
- The flags come from the pkg-config files of the ports, like `-DSPDLOG_FMT_EXTERNAL` and `-lfmt`, with their private libraries like `pkg-config --static`: the ports are static libraries. Packages vcpkg did not install, like the `x11` that `glfw3` requires on Linux, come from the `pkg-config` of the system. Ports without a pkg-config file bring their include directory and their libraries.
- Modules install their ports one after another, because vcpkg gives up waiting for its lock.
- A port brings all its libraries, `gtest` also brings gmock. Those whose names end in `main`, like `gtest_main`, only the tests take, when they have no `main()` of their own.
- vcpkg needs `git`, and with the `system` toolchain the compiler of the system. CMake and Ninja it downloads itself.
- On Windows vcpkg builds the ports with MSVC of Visual Studio, static and only for release, with the static C library: `x64-windows-static-release`. Debug builds link them too, clang links that C library by default. Flags of MSVC in their pkg-config files, like `/DNOMINMAX` of abseil, become those of clang. With `zig` vcpkg is not set up on Windows.

## Toolchains

| `toolchain` | Compiles with                                                                                                                                                                                                                                                                                                                                     |
|-------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `"system"`  | `cc`, `c++` and `ar` of the system: Apple Clang of the Xcode Command Line Tools, gcc or clang on Linux. On Windows `clang`, `clang++` and `llvm-ar` of LLVM or of Visual Studio, with the ABI of MSVC: they link with the linker and the C library of Visual Studio, its Build Tools are enough. `CC`, `CXX` and `AR` name others, like for make. |
| `"zig"`     | [zig](https://ziglang.org) `0.17.0`, downloaded once with a checked SHA-256. It brings clang, libc and libc++, which it links into the program. vcpkg builds the ports with zig too. On Windows it builds for MinGW and needs no Visual Studio.                                                                                                   |

> The system's compiler builds faster and links the system's libc++, zig is the same on every machine.

## Building again

Every source compiles on its own, at the same time as the others. A source compiles again when it, one of its headers or its command line changed: the compiler names the headers in a depfile next to the object. Every module keeps its objects apart, its debug and release builds too.

## Tests

`build test` compiles the files of `test_sources`, `test/**` by default, into the program `build/bin/<name>-test`, with the library, or with everything of a program but the source with `int main(`.
The tests see the headers of the module and those next to them, in the directories of their patterns.

| In `test_dependencies` | Runs with  |
|------------------------|------------|
| `"gtest"`              | GoogleTest |
| `"catch2"`             | Catch2     |
| `"doctest"`            | doctest    |

- Tests without a `main()` of their own get the one of GoogleTest or Catch2.
- The frameworks write JUnit reports into `build/test-results`, the summary counts their test cases: disabled tests of GoogleTest count as skipped. With another framework the exit code of the test program decides.
- Tests whose program did not change since they passed do not run again.
- A test program that runs longer than `test_timeout` seconds is stopped and fails, the other tests still run.
- Arguments after `build test --` go to every test program, after `test_arguments`: `build test --module parser -- --gtest_filter=Lexer.*`.

Many projects build a program per test, whose tests define the same helpers or change the locale for the others. `test_programs` names those test sources: each becomes `build/bin/<name>-<file>`, with the other test sources, like a shared `main()`.

```kotlin
build_cpp(
    "fmt",
    list(),
    test_dependencies = list("gtest"),
    test_sources = list("test/*-test.cc", "test/test-main.cc", "test/gtest-extra.cc", "test/util.cc"),
    // build/bin/fmt-args-test, build/bin/fmt-format-test, ...
    test_programs = list("test/*-test.cc"),
)
```

## Editors

Every module writes `build/compile_commands.json`, modules in one directory share it. clangd finds it from the sources, like in VS Code, Neovim or Zed, CLion opens it as a project.

## Outputs

| Output            | Where                                                                                                     |
|-------------------|-----------------------------------------------------------------------------------------------------------|
| Library           | `build/lib/lib<name>.a`, on Windows `build/lib/<name>.lib`                                                |
| Shared library    | `build/lib/lib<name>.dylib`, on Linux `.so`, on Windows `build/bin/<name>.dll` and `build/lib/<name>.lib` |
| For Android       | `build/android/<build type>/<abi>/lib<name>.so`, the build type is debug or release                       |
| Program           | `build/bin/<name>`, on Windows `<name>.exe`                                                               |
| Test program      | `build/bin/<name>-test`, with `test_programs` `build/bin/<name>-<file>`                                   |
| Test reports      | `build/test-results/<test program>/`                                                                      |
| Generated headers | `build/generated/`                                                                                        |
| Compile commands  | `build/compile_commands.json`                                                                             |
| Ports of vcpkg    | `build/vcpkg/<triplet>/installed/`                                                                        |

> Where a file is in the way of `build/`, like `BUILD` of Bazel, the outputs go to `.build/`, see [commands.md](commands.md#build-run-and-test).
