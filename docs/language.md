# The build language

A script is a list of statements. It looks like Python, but it is much smaller: values, variables, conditions and functions.
There are no loops, no IO and no HTTP.

```kotlin
// line comments
/* and block comments */
use kotlin // loads build_kotlin and the functions around it

gson = "2.14.0" // variables hold strings, numbers, booleans, lists, modules and functions
libraries = list("com.google.code.gson:gson:" + gson)

core = build_kotlin("core", list(), list(maven_central()), directory = "core")

app = build_kotlin(
    "app",
    libraries + list(core), // lists concatenate with +
    list(maven_central()),
    main_class = "AppKt", // named arguments, like in Kotlin
    directory = "app", // trailing commas are fine
)

print("built", app) // a module prints as the file it built

// conditions are values: the value of the block that runs
jdk = if env("CI", default = "false") == "true" { "17" } else { "21" }
```

> Relative paths in a script are relative to the directory of the script.

## Values

- Strings in double quotes. `\"` and `\\` stand for a quote and a backslash, `\n` and `\t` for a new line and a tab, like in `defines = list("PLATFORM=\"darwin\"")`. Other backslashes stay as they are, like in `C:\Users`.
- Integers, `true` and `false`.
- Lists, made with `list(...)`.
- Modules, which the build functions return.
- Functions.

## Operators

- `+` adds numbers, concatenates strings and lists, and appends a number to a string.
- `-`, `*` and `/` work on numbers.
- `==` and `!=` compare two values of the same type, `<`, `<=`, `>` and `>=` order numbers and strings.
- `!` negates a boolean, `&&` and `||` combine two. Their right side only runs when the left side does not decide the result.

## Conditions

`if condition { ... } else if other { ... } else { ... }` is a value: the value of the block that runs, or `unit` when there is no `else`. The condition must be a boolean. Variables defined in a block stay in it.

## Calls

Arguments are passed by position or by name. Like in Kotlin, a positional argument may follow named ones only when those are at their own positions.
Every argument is checked against the type of its parameter, and an error names the place:

```
project.build:4:5: build_kotlin: argument 'dependencies' must be list<str | module>, got str
```

Built-in functions and their parameters are written in lower case with underscores: `build_kotlin`, `maven_central`, `main_class`. A script that still uses the old camel case names is told the new ones.

## use

`use` loads functions into the script. It takes a language of xkit, a plugin of the official list, a plugin of a repository, or a file of the project:

```kotlin
use kotlin                    // a language of xkit
use go                        // the Go plugin of the official list
use go@1                      // its newest release 1.x
use "someone/zig"             // a plugin from GitHub, "gitlab.com/someone/zig" from another host
use "./tools/docs.build"      // a file of the project, relative to the file with the use
```

A plugin is itself a script. It runs once, in a scope of its own, and the script gets the variables it assigns at its top level, except those starting with `_`. project.lock keeps the commit of every plugin from a repository, see [plugins.md](plugins.md).

## Functions every script has

| Function                   | What it does                                                                                    |
|----------------------------|-------------------------------------------------------------------------------------------------|
| `print(values...)`         | Prints the values                                                                               |
| `list(items...)`           | A list                                                                                          |
| `type(value)`              | The type of a value, like `list<str>`                                                           |
| `env(name, default = ...)` | An environment variable, an error when it is not set and there is no default                    |
| `has_env(name)`            | Whether an environment variable is set                                                          |
| `git(url, commit)`         | A commit of a repository, checked out into the cache, see below                                 |
| `operating_system()`       | The system the script is evaluated for: `"macos"`, `"linux"`, `"windows"` or `"android"`        |
| `architecture()`           | Its processor: `"aarch64"`, `"x86_64"`, `"armv7"` or `"x86"`                                    |
| `executable(name)`         | The name of a program on that system: `name`, on Windows `name.exe`                             |
| `target()`                 | The target the script is evaluated for, like `"linux-aarch64"`, see below                       |
| `host_target()`            | The target of the machine xkit runs on                                                          |
| `host_operating_system()`  | The system xkit runs on, like `operating_system()`                                              |
| `host_architecture()`      | The processor xkit runs on                                                                      |
| `host_executable(name)`    | The name of a program on the machine xkit runs on                                               |
| `read_file(path, default)` | The content of a file, relative to the script, `default` when it does not exist                 |
| `match(text, pattern)`     | The first group of a regular expression in `text`, the whole match without groups, `""` without |
| `fail(message)`            | Stops the build with this message                                                               |
| `current_time_millis()`    | The time in milliseconds                                                                        |
| `build_command(...)`       | Any program as a module, see [plugins.md](plugins.md#build_command)                             |
| `tool(name, version, url)` | A program downloaded once and checked, see [plugins.md](plugins.md#tool)                        |
| `cache_directory(name)`    | A directory in the cache of xkit for the caches of tools, like the modules of Go                |

`git(url, commit)` fetches only this commit and returns the directory of a plain checkout in the cache, `caches/git/checkouts/<repository>/<commit>/`, shared by all projects. Modules take it as their `directory`, and what they build stays in the project, in `build/git/<repository>/<commit>/`:

```kotlin
fmt = build_cpp("fmt", list(), directory = git("https://github.com/fmtlib/fmt", "407c905e45ad75fc29bf0f9bb7c5c2fd3475976f"))
```

The commit is best given as its whole hash, which is then checked. A tag like `"v1.3.1"` works too: project.lock keeps the commit it pointed to, and a tag that was moved since stops the build.

A script is evaluated once for every target its modules are built for, see [cpp.md](cpp.md#other-targets): first for this machine, then for the others, like those of `targets` and the ABIs of Android apps with libraries of C and C++. `operating_system()`, `architecture()`, `executable()` and `target()` describe the target of each evaluation, the `host_` functions always the machine xkit runs on. For this machine both describe the same. Plugins download their tools with the `host_` functions, which run on this machine, see [plugins.md](plugins.md#tool).

The functions of a language come with its `use`, see [java.md](java.md), [kotlin.md](kotlin.md), [android.md](android.md) and [cpp.md](cpp.md).

## Your own functions

Functions are values like strings or lists: `fn`, the parameters with their types, the type of the result, and a block whose last expression is the result.

```kotlin
greet = fn (name: str, greeting: str = "Hello") -> str {
    text = greeting + ", " + name      // variables of a call stay in the call
    text
}

greet("Ada")                           // Hello, Ada
greet(greeting = "Hi", name = "Bob")   // named arguments and defaults

// what every module of a project shares, in one place
repositories = list(google(), maven_central())

library = fn (name: str, namespace: str, dependencies: list<str | module> = list()) -> module {
    build_android_library(
        name,
        namespace,
        dependencies,
        repositories,
        min_sdk = 28,
        directory = name
    )
}

core = library("core", "com.example.core")
```

- Types are `str`, `int`, `bool`, `module`, `any`, `unit`, `list<str>`, unions like `str | module`, function types like `fn(str) -> bool`, and strings that stand for themselves, see below. Errors and `type(...)` use the same names.
- Without `-> type`, a function may return anything. A function without a value returns `unit`.
- Arguments and results are checked against their types.
- Defaults are evaluated once, when the function is defined.
- A function sees the variables of the place where it was defined. Its own assignments stay in the call.
- Functions can be passed to functions and returned from them:

  ```kotlin
  apply = fn (f: fn(str) -> str, value: str) -> str { f(value) }
  adder = fn (a: int) -> fn(int) -> int { fn (b: int) -> int { a + b } }
  adder(2)(3)                          // 5
  ```

> A function that calls itself has to end with an `if`, and may go at most 64 calls deep.

### Strings as types

A string in a type stands for exactly that string. Joined with `|`, like in TypeScript, they list the values a parameter takes:

```kotlin
build = fn (name: str, toolchain: "auto" | "local" = "auto") -> str { name + " with " + toolchain }

build("app", toolchain = "local")      // app with local
build("app", toolchain = "locl")       // error: must be "auto" | "local", got "locl", did you mean "local"?
```

- A value that is none of them is an error, which suggests the nearest one for a typo.
- They mix with other types: `"auto" | int` takes the string `"auto"` or any number.
- A string twice in one union, or a string next to `str`, which takes it anyway, is an error.
- `target` is the union of the names of the targets: a parameter `targets: list<target>` takes `list("linux-aarch64")`, and `list("linux-arm64")` is an error that lists the names.
