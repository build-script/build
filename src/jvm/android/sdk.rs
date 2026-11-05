// Android SDK: packages from Google's catalogs, installed after their license is accepted
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::common::system::{self, HostArchitecture, HostOperatingSystem};

pub const SDK_REPOSITORY: &str = "https://dl.google.com/android/repository";

/// The parts of an Android SDK a build needs
pub struct AndroidSdk {
    pub root: PathBuf,
    pub build_tools: String,
    pub android_jar: PathBuf,
    pub aapt2: PathBuf,
    pub zipalign: PathBuf,
    pub d8: PathBuf,
    pub apksigner: PathBuf,
    pub aidl: PathBuf,
    /// the framework's parcelables for aidl
    pub framework_aidl: PathBuf,
}

/// A list of SDK packages: repository2-3.xml, or one of the system image lists
#[derive(Debug, Default, Deserialize)]
pub struct SdkCatalog {
    #[serde(rename = "license", default)]
    licenses: Vec<SdkLicense>,
    #[serde(rename = "remotePackage", default)]
    pub packages: Vec<RemotePackage>,
    /// archive urls are relative to the catalog
    #[serde(skip)]
    pub base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct RemotePackage {
    #[serde(rename = "@path")]
    pub path: String,
    #[serde(rename = "@obsolete", default)]
    pub obsolete: bool,
    #[serde(rename = "type-details", default)]
    pub details: PackageDetails,
    #[serde(rename = "uses-license")]
    license: Option<Reference>,
    #[serde(rename = "channelRef")]
    channel: Option<Reference>,
    #[serde(default)]
    archives: Archives,
}

/// What a package is: system images have an API level and a tag
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PackageDetails {
    /// 36, or 37.0 for newer images
    #[serde(rename = "api-level")]
    pub api_level: Option<String>,
    #[serde(rename = "tag")]
    pub tags: Vec<PackageTag>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PackageTag {
    pub display: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Archive {
    pub complete: Option<ArchiveFile>,
    #[serde(rename = "host-os")]
    host_os: Option<String>,
    #[serde(rename = "host-arch")]
    host_arch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ArchiveFile {
    pub size: u64,
    pub url: String,
    checksum: Option<Checksum>,
}

/// The NDK of the version: that of ANDROID_HOME, else the one in <build home>/android-sdk, installed once
/// its license is accepted
pub fn ndk(version: &str, accept_license: bool) -> Result<PathBuf, String> {
    let installed = ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter()
        .filter_map(|variable| std::env::var_os(variable))
        .map(|root| PathBuf::from(root).join("ndk").join(version))
        .find(|ndk| ndk.is_dir());

    if let Some(ndk) = installed {
        return Ok(ndk);
    }

    let root = system::data_dir("android-sdk");
    let ndk = root.join("ndk").join(version);

    return system::installing(&root, || {
        if !ndk.is_dir() {
            SdkCatalog::load()?.install(&root, &format!("ndk;{version}"), &ndk, accept_license)?;
        }

        return Ok(ndk.clone());
    });
}

impl AndroidSdk {
    /// adb from platform-tools: of ANDROID_HOME, of the SDK the app was built
    /// with, or installed there like the other SDK packages
    pub fn adb(sdk_root: &Path) -> Result<PathBuf, String> {
        let name = format!("adb{}", std::env::consts::EXE_SUFFIX);

        let roots = ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter()
            .filter_map(|variable| std::env::var_os(variable).map(PathBuf::from))
            .chain([sdk_root.to_path_buf()]);

        for root in roots {
            let adb = root.join("platform-tools").join(&name);
            if adb.is_file() {
                return Ok(adb);
            }
        }

        let target = sdk_root.join("platform-tools");

        system::installing(&sdk_root, || {
            if !target.join(&name).is_file() {
                SdkCatalog::load()?.install(sdk_root, "platform-tools", &target, false)?;
            }

            return Ok(());
        })?;

        return Ok(target.join(name));
    }

    /// An installed SDK (ANDROID_HOME) as it is, otherwise the packages in <build home>/android-sdk,
    /// once their license is accepted: in the terminal, or with accept_sdk_license
    pub fn ensure(
        compile_sdk: i64,
        build_tools: &str,
        accept_license: bool,
    ) -> Result<Self, String> {
        for variable in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
            if let Some(root) = std::env::var_os(variable) {
                let sdk = Self::at(Path::new(&root), compile_sdk, build_tools);
                if sdk.is_complete() {
                    return Ok(sdk);
                }
            }
        }

        let root = system::data_dir("android-sdk");
        let sdk = Self::at(&root, compile_sdk, build_tools);

        if sdk.is_complete() {
            return Ok(sdk);
        }

        // builds on other threads may need the same packages, the first installs them
        return system::installing(&root, || {
            let sdk = Self::at(&root, compile_sdk, build_tools);

            if sdk.is_complete() {
                return Ok(sdk);
            }

            let catalog = SdkCatalog::load()?;

            if !sdk.android_jar.is_file() {
                let platform = Self::platform_names(compile_sdk)
                    .into_iter()
                    .find(|name| catalog.package(&format!("platforms;{name}")).is_some())
                    .ok_or_else(|| format!("the Android SDK has no platform for API {compile_sdk}"))?;

                catalog.install(
                    &root,
                    &format!("platforms;{platform}"),
                    &root.join("platforms").join(&platform),
                    accept_license,
                )?;
            }

            if !sdk.aapt2.is_file() {
                catalog.install(
                    &root,
                    &format!("build-tools;{build_tools}"),
                    &root.join("build-tools").join(build_tools),
                    accept_license,
                )?;
            }

            let sdk = Self::at(&root, compile_sdk, build_tools);

            if !sdk.is_complete() {
                return Err(format!(
                    "the Android SDK in {} is incomplete",
                    root.display()
                ));
            }

            return Ok(sdk);
        });
    }

    /// Platforms are android-36 up to API 36, from API 37 on android-37.0
    fn platform_names(compile_sdk: i64) -> [String; 2] {
        return [
            format!("android-{compile_sdk}"),
            format!("android-{compile_sdk}.0"),
        ];
    }

    fn at(root: &Path, compile_sdk: i64, build_tools: &str) -> Self {
        let tools = root.join("build-tools").join(build_tools);
        let executable = |name: &str| tools.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let platforms = root.join("platforms");

        let android_jar = Self::platform_names(compile_sdk)
            .iter()
            .map(|name| platforms.join(name).join("android.jar"))
            .find(|jar| jar.is_file())
            .unwrap_or_else(|| platforms.join(format!("android-{compile_sdk}/android.jar")));

        return AndroidSdk {
            root: root.to_path_buf(),
            build_tools: build_tools.to_string(),
            aidl: executable("aidl"),
            framework_aidl: android_jar.with_file_name("framework.aidl"),
            android_jar,
            aapt2: executable("aapt2"),
            zipalign: executable("zipalign"),
            d8: tools.join("lib/d8.jar"),
            apksigner: tools.join("lib/apksigner.jar"),
        };
    }

    fn is_complete(&self) -> bool {
        return [
            &self.android_jar,
            &self.aapt2,
            &self.zipalign,
            &self.d8,
            &self.apksigner,
        ]
        .iter()
        .all(|file| file.is_file());
    }
}

impl SdkCatalog {
    pub fn load() -> Result<Self, String> {
        return Self::load_from(&format!("{SDK_REPOSITORY}/repository2-3.xml"));
    }

    pub fn load_from(url: &str) -> Result<Self, String> {
        let (status, body) = system::http_get(url, None)?;

        if status != 200 {
            return Err(format!("{url} responded with HTTP {status}"));
        }

        let mut catalog = Self::parse(&body).map_err(|error| format!("{url}: {error}"))?;
        catalog.base_url = url.rsplit_once('/')
            .map(|(base, _)| base.to_string())
            .unwrap_or_default();

        return Ok(catalog);
    }

    /// The stable channel wins over previews
    pub fn package(&self, path: &str) -> Option<&RemotePackage> {
        let matching: Vec<&RemotePackage> = self.packages.iter()
            .filter(|remote| remote.path == path)
            .collect();

        let stable = matching.iter().find(|remote| remote.is_stable());

        return stable.or(matching.first()).copied();
    }

    /// Download size of a package in bytes
    pub fn size(&self, path: &str) -> Option<u64> {
        return Some(self.package(path)?.archive()?.complete.as_ref()?.size);
    }

    pub fn install(
        &self,
        sdk_root: &Path,
        package: &str,
        target: &Path,
        accept_license: bool,
    ) -> Result<(), String> {
        let Some(remote) = self.package(package) else {
            return Err(format!("the Android SDK has no package {package}"));
        };

        if let Some(license) = &remote.license {
            let mut consent = |id: &str, text: &str, text_file: &Path| {
                if accept_license {
                    Ok(true)
                } else {
                    ask_in_terminal(id, text, text_file)
                }
            };

            self.check_license(sdk_root, &license.id, package, &mut consent)?;
        }

        let archive = remote.archive().ok_or_else(|| {
            format!(
                "{package} is not available for {} on {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;

        let complete = archive.complete.as_ref()
            .ok_or_else(|| format!("{package} has no complete archive"))?;

        let url = &complete.url;

        let checksum = complete.checksum.as_ref()
            .ok_or_else(|| format!("{package} has no checksum"))?;

        let bits = if checksum.kind.as_deref() == Some("sha-256") {
            256
        } else {
            1
        };

        // the url ends up in a path, so only its last component counts
        let file_name = Path::new(url)
            .file_name()
            .ok_or_else(|| format!("invalid url {url}"))?;

        system::log_tool("android-sdk", &format!("Installing {package}"));
        let download = sdk_root.join(".downloads").join(file_name);

        // the archives hold one directory with an arbitrary name
        return system::install_archive(&format!("{}/{url}", self.base_url), &download, Some((bits, checksum.value.trim())), target);
    }

    /// Like sdkmanager: accepted licenses are hashes in <sdk>/licenses/<id>. `consent` gets
    /// the id, the text and its file, and decides about a license not accepted yet.
    pub fn check_license(
        &self,
        sdk_root: &Path,
        id: &str,
        package: &str,
        consent: &mut dyn FnMut(&str, &str, &Path) -> Result<bool, String>,
    ) -> Result<(), String> {
        let text = self.licenses.iter()
            .find(|license| license.id == id)
            .map(|license| license.text.trim().to_string())
            .ok_or_else(|| format!("license {id} is missing in the SDK catalog"))?;

        let licenses = sdk_root.join("licenses");
        let text_file = licenses.join(format!("{id}.txt"));
        system::write_file(&text_file, &text)?;
        let hash = system::sha(&text_file, 1)?;

        let accepted_file = licenses.join(id);

        let accepted = fs::read_to_string(&accepted_file)
            .is_ok_and(|content| content.lines().any(|line| line.trim() == hash));

        if accepted {
            return Ok(());
        }

        if !consent(id, &text, &text_file)? {
            return Err(format!(
                "the {id} was not accepted, so {package} cannot be installed"
            ));
        }

        let previous = fs::read_to_string(&accepted_file).unwrap_or_default();
        system::write_file(&accepted_file, &format!("{previous}\n{hash}"))?;
        println!("Accepted the {id} ({})", text_file.display());

        return Ok(());
    }

    fn parse(content: &str) -> Result<Self, String> {
        return quick_xml::de::from_str(content).map_err(|error| error.to_string());
    }
}

impl RemotePackage {
    /// Of the stable channel, not of previews
    pub fn is_stable(&self) -> bool {
        return self.channel.as_ref()
            .is_some_and(|channel| channel.id == "channel-0");
    }

    /// For this operating system and architecture
    pub fn archive(&self) -> Option<&Archive> {
        let os = match system::host_operating_system().ok()? {
            HostOperatingSystem::MacOs => "macosx",
            HostOperatingSystem::Linux => "linux",
            HostOperatingSystem::Windows => "windows",
        };

        let arch = match system::host_architecture().ok()? {
            HostArchitecture::Aarch64 => "aarch64",
            HostArchitecture::X86_64 => "x64",
        };

        return self.archives.archives.iter().find(|archive| {
            archive.host_os.as_deref().is_none_or(|host| host == os)
                && archive.host_arch.as_deref().is_none_or(|host| host == arch)
        });
    }
}

#[derive(Debug, Deserialize)]
struct SdkLicense {
    #[serde(rename = "@id")]
    id: String,
    #[serde(rename = "$text", default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct Reference {
    #[serde(rename = "@ref")]
    id: String,
}

#[derive(Debug, Default, Deserialize)]
struct Archives {
    #[serde(rename = "archive", default)]
    archives: Vec<Archive>,
}

#[derive(Debug, Deserialize)]
struct Checksum {
    #[serde(rename = "@type")]
    kind: Option<String>,
    #[serde(rename = "$text")]
    value: String,
}

/// Shows the license and asks for a yes or no, like sdkmanager does
fn ask_in_terminal(id: &str, text: &str, text_file: &Path) -> Result<bool, String> {
    if !io::stdin().is_terminal() {
        return Err(format!(
            "the {id} has to be accepted, but there is no terminal to ask. Read it in {} and pass accept_sdk_license = true to accept it",
            text_file.display(),
        ));
    }

    let rule = "-".repeat(72);
    println!("\nLicense {id}:\n{rule}\n{text}\n{rule}");

    return Ok(matches!(
        system::prompt(&format!("Do you accept the {id}? (y/N): "))?.as_str(),
        "y" | "yes"
    ));
}

#[cfg(test)]
pub mod tests {
    use crate::common::test_support::ScratchDir;

    use super::*;

    pub fn catalog(packages: &str) -> SdkCatalog {
        let content = format!(
            r#"<sdk:sdk-repository xmlns:sdk="http://schemas.android.com/sdk/android/repo/repository2/03">{packages}</sdk:sdk-repository>"#
        );
        return SdkCatalog {
            base_url: "https://example.invalid".to_string(),
            ..SdkCatalog::parse(&content).unwrap()
        };
    }

    #[test]
    fn catalogs_prefer_stable_packages_for_this_machine() {
        let os = if cfg!(target_os = "macos") {
            "macosx"
        } else {
            std::env::consts::OS
        };

        let arch = if std::env::consts::ARCH == "x86_64" {
            "x64"
        } else {
            std::env::consts::ARCH
        };

        let packages = format!(
            r#"
            <remotePackage path="emulator"><channelRef ref="channel-2"/>
                <archives><archive><complete><size>1</size><url>preview.zip</url></complete><host-os>{os}</host-os><host-arch>{arch}</host-arch></archive></archives>
            </remotePackage>
            <remotePackage path="emulator"><channelRef ref="channel-0"/>
                <archives>
                    <archive><complete><size>2</size><url>other.zip</url></complete><host-os>{os}</host-os><host-arch>other</host-arch></archive>
                    <archive><complete><size>3</size><url>stable.zip</url></complete><host-os>{os}</host-os><host-arch>{arch}</host-arch></archive>
                </archives>
            </remotePackage>"#
        );

        let catalog = catalog(&packages);
        let archive = catalog.package("emulator")
            .unwrap()
            .archive()
            .unwrap();
        assert_eq!(archive.complete.as_ref().unwrap().url, "stable.zip");
        assert_eq!(catalog.size("emulator"), Some(3));
        assert!(catalog.package("missing").is_none());
    }

    #[test]
    fn sdk_packages_need_an_accepted_license() {
        let sdk = ScratchDir::new("sdk");
        let catalog = catalog(r#"<license id="test-license" type="text">Be nice.</license>"#);

        // the license text is handed to whoever decides, and saved for reading
        let mut shown = String::new();

        let mut decline = |id: &str, text: &str, file: &Path| {
            shown = format!("{id}: {text} ({})", file.display());
            Ok(false)
        };

        let error = catalog.check_license(&sdk, "test-license", "platforms;android-1", &mut decline)
            .unwrap_err();

        assert!(error.contains("was not accepted"), "{error}");
        assert!(shown.starts_with("test-license: Be nice."), "{shown}");
        assert_eq!(
            fs::read_to_string(sdk.join("licenses/test-license.txt")).unwrap(),
            "Be nice."
        );

        catalog.check_license(
            &sdk,
            "test-license",
            "platforms;android-1",
            &mut |_, _, _| Ok(true),
        )
            .unwrap();
        // accepted once, remembered like sdkmanager does: nobody is asked again
        catalog.check_license(
            &sdk,
            "test-license",
            "platforms;android-1",
            &mut |_, _, _| panic!("asked again"),
        )
            .unwrap();

    }
}
