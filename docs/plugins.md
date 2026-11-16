# Plugins

A plugin brings a language, or any tool, to build-script without changing build-script itself. It is a build script like `project.build`: functions that run the compiler of the language with [`build_command`](#build_command) and download it with [`tool`](#tool).

```kotlin
use go

build_go("server")
```

```bash
build run
```

This page shows how scripts use plugins, and the functions plugins are made of. How plugins work inside, how to write one and what they cannot do is explained in [build-script/plugins](https://github.com/build-script/plugins).

## Using plugins

| `use`                            | Loads                                                                                                 |
|----------------------------------|-------------------------------------------------------------------------------------------------------|
| `use go`                         | `go/plugin.build` of the [official list](https://github.com/build-script/plugins), its newest release |
| `use go@1`, `use go@1.4`         | its newest release 1.x or 1.4.x                                                                       |
| `use go@main`, `use go@<commit>` | a branch or a commit instead of a release                                                             |
| `use "someone/zig"`              | `plugin.build` of the GitHub repository `someone/zig`                                                 |
| `use "gitlab.com/someone/zig"`   | the same of another host, named by its first part with a dot                                          |
| `use "someone/plugins/zig"`      | `zig/plugin.build` in a directory of the repository                                                   |
| `use "someone/zig@2"`            | its newest release 2.x                                                                                |
| `use "./tools/docs.build"`       | a file of the project, relative to the file with the `use`                                            |

- A name without quotes is a language of build-script, like `kotlin`, or else a plugin of the official list. A repository or a file needs quotes.
- `build plugins` lists the official plugins and their templates. `build new go hello` starts a project from a plugin's template, see [commands.md](commands.md#new-and-add).
- A plugin defines the variables it assigns at its top level, except those starting with `_`. When two plugins define one name with different values, the build stops.
- An error inside a plugin names both places, the call in the script and the line of the plugin: `project.build:3:1: build_docs: ... (in tools/docs.build:12:5)`.

## Versions and project.lock

A release is a git tag with a version: `go-v1.4.2` for the plugin `go` of the official list, `v1.4.2` in a repository of its own, or `zig-v1.4.2` for a plugin in the directory `zig`.

- `use go` takes the newest release, `use go@1.4` the newest release whose version starts with 1.4. A repository without releases is taken at its default branch.
- [project.lock](commands.md#projectlock) keeps the tag and the commit it pointed to: `plugin:go commit=9327d85... ref=go-v1.4.2`. Later builds and other machines take the same commit, without asking the repository again.
- A tag that points to another commit than the lock says stops the build. `build --refresh` takes the newest release again, within the major version the lock had: `go-v1.9.0` after `go-v1.4.2`, but not `go-v2.0.0`.
- Plugins are fetched once with `git`, only the commit itself, into the cache: `caches/git/checkouts/`.
- `BUILD_SCRIPT_PLUGINS` replaces the official list with another repository, like a mirror in a company, or a `file://` repository to try out changes to a plugin.

## build_command

`build_command` makes a module of any program: it runs when one of its inputs changed, and other modules can use what it wrote.

```kotlin
greeting = build_command(
    "greeting",
    list("python3", "tools/greeting.py", "{output}/greeting.h"),
    inputs = list("tools/greeting.py", "greetings.txt"),
    outputs = list("{output}/greeting.h"),
)

build_c("app", list(greeting), program = true)   // includes greeting.h
```

| Parameter            | Default  | Meaning                                                                                                      |
|----------------------|----------|--------------------------------------------------------------------------------------------------------------|
| `name`               | required | Name of the module, its files go to `build/commands/<name>/`                                                 |
| `command`            | required | The program and its arguments. A module in it stands for its program or file, see below                      |
| `inputs`             | `[]`     | Patterns of the files the command reads, relative to `directory`. A directory stands for everything below it |
| `exclude`            | `[]`     | Patterns left out of `inputs`                                                                                |
| `outputs`            | `[]`     | Files the command writes, relative to `directory`, mostly below `{output}`                                   |
| `dependencies`       | `[]`     | Modules whose results count as inputs                                                                        |
| `environment`        | `[]`     | `"NAME=value"` for the command, its `run` and its `test`. A later one wins over an earlier one               |
| `environment_inputs` | `[]`     | Variables of the environment build-script was started in whose values count as inputs, like `GOOS`           |
| `path`               | `[]`     | Directories put in front of `PATH`                                                                           |
| `directory`          | `"."`    | Where the command runs, relative to the script                                                               |
| `run`                | `[]`     | What `build run` starts, with the arguments after `--`                                                       |
| `test`               | `[]`     | What `build test` runs                                                                                       |
| `test_timeout`       | `0`      | Seconds the tests may run, `0` for no limit                                                                  |
| `always`             | `false`  | Runs on every build, for commands whose inputs cannot be named                                               |
| `targets`            | `[]`     | The targets it runs for, each into `build/<target>/commands/<name>/`, see below                              |

- `{output}` stands for `build/commands/<name>/` in `command`, `outputs`, `environment`, `path`, `run` and `test`. It is emptied before every run, so files of renamed sources do not stay behind.
- The command runs again when its command line, an input, the environment, the result of a module it uses, or build-script itself changed, or when an output is missing. An output that it did not write fails the build.
- `inputs` skip `build/` and hidden directories like `.git`. A pattern like `**/*.go` takes every Go file below `directory`.
- A module in `command` stands for how it starts: a Java or Kotlin program with `main_class` for `java -jar` with its JDK, a C or C++ program or a library for its file, a command for its `run` or its first output. It is always the module of this machine, which runs the command.
- `test` runs once the command is built. With `{test_reports}` in it, the test writes JUnit XML reports there and the summary counts their tests, otherwise its exit code decides. Tests that passed do not run again until the command or its tests change.

With `targets` the command runs once for every target, with `{output}` in `build/<target>/commands/<name>/` and the script evaluated for that target, like for Go with the `GOOS` of the target. Without `targets` it runs for this machine. A module for another target that depends on it gets the command of this machine when the command is the same for that target, like a code generator, which then runs once. Otherwise the command runs for that target too. `build run --target` starts its `run` through the runner of the target, see [cpp.md](cpp.md#programs-of-other-targets).

What other modules make of a command among their `dependencies`:

| Module                                | Takes                                                                                                    |
|---------------------------------------|----------------------------------------------------------------------------------------------------------|
| `build_c`, `build_cpp`                | `{output}` as an include directory, the C and C++ sources among `outputs`                                |
| `build_java`, `build_kotlin`, Android | the jars among `outputs`, and `{output}` as a source directory when `outputs` has `.java` or `.kt` files |
| `build_command`                       | the module as an input, and as a program in `command`                                                    |

## tool

`tool` downloads a program once per version and returns its directory. Plugins bring the compilers of their language with it:

```kotlin
url = "https://dl.google.com/go/go1.27.4.darwin-arm64.tar.gz"
go = tool("go", "1.27.4", url, sha256_url = url + ".sha256") + "/bin/" + host_executable("go")
```

| Parameter    | Default  | Meaning                                                                                     |
|--------------|----------|---------------------------------------------------------------------------------------------|
| `name`       | required | Name of the tool, the directory it goes to                                                  |
| `version`    | required | Its version, the directory below the name                                                   |
| `url`        | required | The archive: zip, tar, tar.gz or tar.xz                                                     |
| `sha256`     | `""`     | The SHA-256 of the archive                                                                  |
| `sha256_url` | `""`     | A file with it: only the checksum, or lines of a checksum and a file name like `SHA256SUMS` |

- A tool runs on this machine: a plugin picks its archive with `host_operating_system()` and `host_architecture()`, and its program with `host_executable()`. Those stay the same when the script is evaluated for another target.
- The archive is checked before it is unpacked. A single directory at the top of the archive is dropped: the directory of `go1.27.4.darwin-arm64.tar.gz` is that of `go/`.
- project.lock keeps the SHA-256 of every archive, `tool:go/1.27.4/go1.27.4.darwin-arm64.tar.gz sha256=...`. Without `sha256` and `sha256_url`, the first download is trusted and the lock keeps it, like the first build with a new Maven library.
- Tools go next to the JDKs, `tools/<name>/<version>-<hash of the URL>/`, and `clean --cache` keeps them.
- `BUILD_SCRIPT_TOOL_<NAME>` names a directory with the tool already installed, like in a CI image or from Nix: `BUILD_SCRIPT_TOOL_GO=/usr/local/go`. Then nothing is downloaded.

### Mirrors

A setting `mirror.<URL>` downloads tools whose URL starts with `<URL>` from another URL, also their `sha256_url`:

```bash
build config mirror.https://dl.google.com/go/ https://artifacts.example.com/go/
```

The longest prefix that fits wins. The SHA-256 stays the same, so the mirror has to serve the same files.

## Functions for plugins

Every script has these, but plugins need them most:

| Function                   | What it does                                                                                                     |
|----------------------------|------------------------------------------------------------------------------------------------------------------|
| `operating_system()`       | `"macos"`, `"linux"`, `"windows"` or `"android"`, of the target the script is evaluated for                      |
| `architecture()`           | `"aarch64"`, `"x86_64"`, `"armv7"` or `"x86"`, of that target                                                    |
| `executable(name)`         | `name`, for Windows `name.exe`                                                                                   |
| `target()`                 | The target, like `"linux-aarch64"`                                                                               |
| `host_operating_system()`  | The same for the machine build-script runs on, for the tools a plugin downloads                                  |
| `host_architecture()`      | Its processor                                                                                                    |
| `host_executable(name)`    | A program on it                                                                                                  |
| `host_target()`            | Its target                                                                                                       |
| `cache_directory(name)`    | A directory in build-script's cache for the caches of a tool, like the modules of Go. `clean --cache` removes it |
| `read_file(path, default)` | The content of a file relative to the script, `default` when there is none                                       |
| `match(text, pattern)`     | The first group of a regular expression, like the version in `go.mod`                                            |
| `fail(message)`            | Stops the build with a message, like for a system the plugin does not support                                    |
| `env(name, default)`       | An environment variable                                                                                          |

A plugin also sees `plugin_directory`, the directory of its `plugin.build`, for files it brings along.
