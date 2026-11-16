# Commands

| Command                                                                                                                                      | What it does                                                                    |
|----------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------|
| `build [<script>]`                                                                                                                           | Builds every module. Modules that did not change are up to date.                |
| `build run [--module <name>] [<script>] [-- <args>...]`                                                                                      | Builds, then starts a module. Arguments after `--` go to the program.           |
| `build test [--module <name>] [<script>] [-- <args>...]`                                                                                     | Builds, then runs the tests of every module, or of one, and sums them up.       |
| `build idea [<script>]`                                                                                                                      | Builds and sets the project up for IntelliJ, see [IntelliJ](#intellij).         |
| `build new <kind> <path> [--package <package>]`                                                                                              | Creates a project in a new directory.                                           |
| `build add <kind> <name> [<script>] [--package <package>]`                                                                                   | Adds a module next to the script and its build call to the script.              |
| `build install <group:artifact[:version]> [<script>] [--module <name>] [--compile-only \| --test \| --android-test \| --processor \| --ksp]` | Adds a Maven library to a module, or a vcpkg port to a C or C++ module.         |
| `build clean [<script>]`, `clean --cache`                                                                                                    | Removes the build directories build-script wrote, `--cache` also the downloads. |
| `build config [<setting> [<value>]]`, `config --unset <setting>`                                                                             | Shows the settings, shows one, sets one or removes one.                         |
| `build plugins`                                                                                                                              | Lists the official plugins and their templates.                                 |
| `build --help`                                                                                                                               | Shows the commands.                                                             |
| `build --version`                                                                                                                            | Shows the version of build-script.                                              |
| `build <command> --help`, `help <command>`                                                                                                   | Shows the usage of a command and what its arguments may be.                     |

> Without a script, the commands use `project.build` in the current directory.

> In a terminal the log is colored: the modules in orange, the tools build-script brings, like the JDK or vcpkg, in yellow, failed tests in red.
> `NO_COLOR=1` turns the colors off, and output into a file or a pipe has none.

> In a terminal, `new`, `add`, `install` and `config --unset` ask for what is missing. A kind or setting is picked from a list: typing filters it
> like fzf, the arrow keys move, enter takes the selection and escape cancels. A kind that matches none of them opens the list filtered by it.
> Without a terminal they stop and show what is missing and the usage. A misspelled command or option is answered with what it most likely is.

## build, run and test

- Modules that do not depend on each other build at the same time, as many as half the cores of the machine. `--jobs <n>` sets how many, `--jobs 1` builds one after another.
- A module waits for the modules it depends on. When one fails, the modules depending on it are not built, and only its error is reported.
- Every log line names its module: `[app] Compiling 3 Kotlin file(s)`.
- A module builds into `build/` in its directory. Where a file is in the way, like a `BUILD` file of Bazel on macOS, which does not tell upper and lower case apart, it builds into `.build/`.
- `--refresh` asks the repositories again: Maven repositories for newer SNAPSHOT builds, which [project.lock](#projectlock) keeps otherwise, and for the versions `install` offers, which are kept a day otherwise. Plugins move to their newest release of the same major version.
- `--release` builds Android apps and C and C++ modules for release, only with `build` and `run`. See [android.md](android.md#release-builds).
- `--target <name>` builds for that target instead of the targets the script names, see [cpp.md](cpp.md#other-targets). `build` takes it several times, `run` and `test` once: they start the programs of that target, this machine itself where it can, otherwise through `runner.<target>`, see [cpp.md](cpp.md#programs-of-other-targets).
- `run` starts the module `--module` names, otherwise the last one that can start: one with a `main_class`, an Android app, a C or C++ program, or a `build_command` with a `run`. Jars start with the JDK they were built with, in their module directory, C and C++ programs too. Android apps are installed on a device, see [android.md](android.md).
- `test` builds everything first, then runs the tests module by module. A failing module does not stop the others. Arguments after `--` go to the test programs of C and C++ modules, like `--gtest_filter=Lexer.*`. At the end it lists every module, and exits with 1 when one failed:

  ```
  > Tests
    core               passed, 12 tests
    app                FAILED, 2 of 10 tests
    app (androidTest)  passed, 4 tests
    ui                 up to date
  1 failed, 2 passed, 1 up to date
  ```

## Up to date

Every module keeps a fingerprint of its inputs in `build/fingerprints/`: the arguments of its build call, the JDK, its sources and resources, what the modules it depends on built, the builds of its SNAPSHOT libraries, and build-script itself.
When none of it changed and the outputs exist, the module is not built again. A build that fails half way never counts as up to date.

## IntelliJ

`build idea` builds and sets the project up for IntelliJ: a module file per module in `.idea/modules/`, and `.idea/modules.xml`, which lists them. Then open the script's directory in IntelliJ.

- From then on, every `build`, `run` and `test` keeps the files current. A module writes its file once its libraries are resolved, before it compiles: a new library shows up in IntelliJ even while the code does not compile yet.
- Only files that changed are written, because IntelliJ reloads every file that changes.
- A module file has the sources, resources and tests, the sources generated by KSP, javac processors and Android, the modules and libraries with their scopes, and the Kotlin compiler plugins, like Compose.
- After a successful build, the files of modules that are gone from the script are deleted. The script's directory gets a module of its own when no module lives in it.
- IntelliJ keeps its JDKs outside the project, by name. A JDK that build-script installed is named the way IntelliJ names it, like `temurin-21`. If IntelliJ does not know it yet, add it once under *Project Structure > SDKs* with that name.
- Release builds leave the files as they are, they describe the debug build.
- build-script only writes `.idea/modules.xml` and `.idea/modules/`, IntelliJ's own files stay untouched.
- To IntelliJ, Android modules are Kotlin modules with `android.jar`: completion and navigation work, layout previews do not.

## new and add

```bash
build new kotlin hello          # hello/project.build and hello/src/main/kotlin/...
cd hello && build run           # Hello from hello

build new workspace shop        # shop/project.build without modules
cd shop
build add kotlin-library payments
build add android-app shop-android
build add go api                # a module of the Go plugin, next to the others
```

| Project kind | What `new` creates                                                            |
|--------------|-------------------------------------------------------------------------------|
| `java`       | a Java program                                                                |
| `kotlin`     | a Kotlin program                                                              |
| `android`    | an Android app with Jetpack Compose                                           |
| `cpp`, `c`   | a C++ or a C program                                                          |
| `workspace`  | a `project.build` without modules, `add` adds modules of any language         |
| a plugin     | the template of a plugin: `go`, `go:<template>`, `go@1`, `someone/repository` |

| Module kind                      | Built with                                                                                    |
|----------------------------------|-----------------------------------------------------------------------------------------------|
| `java-app`, `java-library`       | `build_java`, the app with a `main_class`                                                     |
| `kotlin-app`, `kotlin-library`   | `build_kotlin`, the app with a `main_class`                                                   |
| `android-app`, `android-library` | `build_android` and `build_android_library`, with Compose                                     |
| `cpp-app`, `cpp-library`         | `build_cpp`, the app with `program = true`, the library with its headers in `include/<name>/` |
| `c-app`, `c-library`             | `build_c`, the same in C                                                                      |
| a plugin                         | the build function of the plugin's template, like `build_go`                                  |

- Without a kind, in a terminal, `new` and `add` offer a list: the kinds of build-script and the templates of the official plugins, each with a short description. Without network, the list shows the kinds of build-script and a note that the plugins could not be listed.
- After `new workspace`, a terminal asks whether to add a first module, and then continues like `add`.
- A plugin's template comes from the release that `project.lock` keeps, or from its newest release, which then goes into the lock: `add` uses the same release as the modules already there. `go:cli` picks the template `cli`, without one the plugin's `app` is taken. `build plugins` lists the templates.
- C and C++ modules have no packages, so `new` and `add` do not ask for a domain then. A plugin's template asks only when it uses the domain, like the `go.mod` of the Go plugin.
- Packages start with your domain, reversed: `mvmo.dev` gives `dev.mvmo.<project>`, and a module in its own directory `dev.mvmo.<project>.<module>`. `--package` sets the whole package.
- Without a remembered domain, `new` and `add` ask for it and offer to remember it: `y` remembers it, `n` asks again next time, `never` stops offering. Without a terminal, like in CI, they use `example.com`.
- `add` changes the script only where it has to: the `use` the module needs, `google()` for Android modules, and the build call. Libraries go before the first app, apps to the end.
- An added module does not depend on anything yet. Add its variable to the dependencies of another module: `list(payments)`.

## install

`install` adds a Maven library to a module, like `cargo add`:

```bash
build install com.google.code.gson:gson            # the newest release
build install com.google.code.gson:gson:2.10.1     # this version, replacing another one
build install org.spigotmc:spigot-api --compile-only --module plugin
```

- Without a version, it takes the newest final version in the module's repositories. When there are only milestones, release candidates or SNAPSHOTs, it takes the newest of those and says so.
- The library goes into `dependencies`. `--compile-only`, `--test`, `--android-test`, `--processor` and `--ksp` put it into `compile_only`, `test_dependencies`, `android_test_dependencies`, `annotation_processors` or `ksp` instead.
- In a script with several modules, `--module` picks one.
- Before the script changes, the library is resolved for the module. Android libraries are refused for Java and Kotlin modules, and so are libraries that need a newer Java than the module's `jdk`:

  ```
  spring-core-6.2.12.jar needs Java 17, app builds with JDK 11: give it jdk = "17"
  ```

- The script changes only where it has to. The library goes to the end of the `list(...)`, in the style of the list. A list in a variable is changed where the variable is assigned. A missing argument is added to the build call. Another version of the same library is replaced.
- A library that is in none of the module's repositories needs its repository in the script first, as `maven(url)`.
- For C and C++ modules, `install` adds vcpkg ports, like `build install fmt` or `install doctest --test`. A port is added once vcpkg knows it, without a version: the vcpkg release pins every port to one.

## clean

`clean` removes the build directories below the script's directory that build-script wrote, those with `fingerprints/` in them. Build directories of other tools and the downloads stay, and so do files that git tracks in a build directory: some repositories keep scripts in `build/`.

`clean --cache` removes the downloads of Maven, git and vcpkg, the caches of plugins and what was made of them, and the next build downloads them again. The JDKs, SDKs, toolchains and tools stay.
Files that a stopped build left behind are removed by themselves once they have not changed for an hour.

## project.lock

The first build writes `project.lock` next to the script, like `Cargo.lock`. Keep it in version control. It holds:

| Line                                           | What it pins                                                         |
|------------------------------------------------|----------------------------------------------------------------------|
| `<path in the repository> sha256=<hex>`        | a file from a Maven repository                                       |
| `<path of a SNAPSHOT> build=<build>`           | the build a SNAPSHOT resolved to                                     |
| `tool:<name>/<version>/<archive> sha256=<hex>` | the archive of a [tool](plugins.md#tool)                             |
| `plugin:<name> commit=<hash> ref=<tag>`        | the release of a [plugin](plugins.md), the commit its tag pointed to |
| `git:<repository>@<tag> commit=<hash>`         | the commit a tag of `git(...)` pointed to                            |

- Later builds take the same SNAPSHOT builds, plugin releases and commits. A download that does not match the lock stops the build, also on another machine, and so does a tag that was moved.
- A new library or another version goes into the lock with the next build, and the old version leaves it. Libraries only `test` needs stay when `build` does not need them, and so do the tools of other systems.
- Plugins and tags of `git(...)` stay in the lock only while the script uses them.
- `--refresh` writes it again: with the newest SNAPSHOT builds, the newest plugin release of the same major version, and the files of a repository that changed them.
- A script that downloads nothing gets no lock.

## config and settings

The settings are in the file `settings` next to the downloads, one `name = value` per line.

| Setting           | Meaning                                                                                                               |
|-------------------|-----------------------------------------------------------------------------------------------------------------------|
| `domain`          | Your domain, like `mvmo.dev`: the packages of new modules start with it                                               |
| `remember_domain` | `never`: `new` and `add` do not offer to remember the domain                                                          |
| `mirror.<URL>`    | Another URL for tools whose URL starts with `<URL>`, see [plugins.md](plugins.md#mirrors)                             |
| `runner.<target>` | What starts the programs of a target this machine does not run itself, see [cpp.md](cpp.md#programs-of-other-targets) |

```bash
build config domain mvmo.dev                                         # remembers the domain
build config --unset remember_domain                                 # offers to remember it again
build config mirror.https://dl.google.com/go/ https://mirror.example.com/go/
build config runner.linux-aarch64 'qemu-aarch64 -L /usr/aarch64-linux-gnu'
```

## Where downloads go

Everything build-script downloads is shared by all projects.

| Variable                                             | Effect                                                                                                                                                                                                                      |
|------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `BUILD_SCRIPT_HOME`                                  | One directory for everything, with the caches in `caches/` below it, like `~/.gradle`                                                                                                                                       |
| `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME` | Without `BUILD_SCRIPT_HOME`: JDKs, SDK, emulators and tools in `$XDG_DATA_HOME/build-script`, Maven artifacts, git checkouts and plugins in `$XDG_CACHE_HOME/build-script`, the settings in `$XDG_CONFIG_HOME/build-script` |
| neither                                              | `~/.build-script`, with the caches in `~/.build-script/caches`                                                                                                                                                              |
