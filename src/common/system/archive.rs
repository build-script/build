// Zip and tar archives, also compressed with gzip or xz: extracted, written and read entry by entry
use std::fs;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use lzma_rust2::XzReader;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::*;

/// Zip archives, also .aar and .jar, and tar archives, also compressed with gzip or xz: their first
/// bytes tell them apart
pub fn extract(archive: &Path, target: &Path) -> Result<(), String> {
    create_dir(target)?;

    let failed = |error: std::io::Error| format!("could not extract {}: {error}", archive.display());
    let mut file = fs::File::open(archive).map_err(failed)?;
    let mut magic = [0; 6];
    let read = file.read(&mut magic).map_err(failed)?;
    file.seek(SeekFrom::Start(0)).map_err(failed)?;
    let reader = BufReader::new(file);

    let unpacked = match &magic[..read] {
        [b'P', b'K', ..] => return extract_zip(archive, target, |_| true),
        [0x1f, 0x8b, ..] => tar::Archive::new(GzDecoder::new(reader)).unpack(target),
        [0xfd, b'7', b'z', b'X', b'Z', 0] => tar::Archive::new(XzReader::new(reader, true)).unpack(target),
        _ => tar::Archive::new(reader).unpack(target),
    };

    return unpacked.map_err(failed);
}

/// The entries of a zip archive `keep` takes by their names, like a jar without its signature.
/// Executables stay executable, symbolic links stay links within the target.
pub fn extract_zip(archive: &Path, target: &Path, keep: impl Fn(&str) -> bool) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("could not extract {}: {error}", archive.display());
    let mut zip = open_zip(archive)?;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| failed(&error))?;
        let name = entry.name()
            .map_err(|error| failed(&error))?
            .to_string();

        if !keep(&name) {
            continue;
        }

        // a name like ../x would be written outside of the target
        let path = entry.enclosed_name()
            .map(|relative| target.join(relative))
            .ok_or_else(|| failed(&format!("{name} is outside of the archive")))?;

        if entry.is_dir() {
            create_dir(&path)?;
            continue;
        }

        create_parent_dir(&path)?;
        let _ = fs::remove_file(&path);

        #[cfg(unix)]
        if entry.is_symlink() {
            let mut link = String::new();
            entry.read_to_string(&mut link).map_err(|error| failed(&error))?;

            // entries after a link out of the target would be written there
            use std::path::Component;
            let mut resolved = path.parent()
                .unwrap_or(target)
                .to_path_buf();
            for component in Path::new(&link).components() {
                match component {
                    Component::Normal(part) => resolved.push(part),
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    Component::CurDir => {}
                    // absolute
                    Component::RootDir | Component::Prefix(_) => resolved = PathBuf::new(),
                }
            }

            if !resolved.starts_with(target) {
                return Err(failed(&format!("{name} links to {link}, outside of the archive")));
            }

            std::os::unix::fs::symlink(&link, &path).map_err(|error| failed(&error))?;
            continue;
        }

        let mut file = fs::File::create(&path).map_err(|error| failed(&error))?;
        std::io::copy(&mut entry, &mut file).map_err(|error| failed(&error))?;

        // like the tools of the Android SDK
        #[cfg(unix)]
        if entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0) {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(|error| failed(&error))?;
        }
    }

    return Ok(());
}

/// Adds the files to a zip archive by their names in it, to a new one where there is none. Stored files
/// stay uncompressed, like the native libraries an app loads from its apk.
pub fn add_to_zip(archive: &Path, files: &[(String, PathBuf)], stored: bool) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("could not write {}: {error}", archive.display());

    // renamed once complete, the entries already there copied as they are: appending would write a
    // central directory that differs from the headers aapt2 wrote, zipalign warns about that
    return produce_atomically(archive, |partial| {
        // buffered, the headers of the entries are many small writes
        let file = fs::File::create(partial).map_err(|error| failed(&error))?;
        let mut zip = ZipWriter::new(BufWriter::new(file));

        if archive.is_file() {
            let mut existing = open_zip(archive)?;

            for index in 0..existing.len() {
                let entry = existing.by_index_raw(index).map_err(|error| failed(&error))?;
                zip.raw_copy_file(entry).map_err(|error| failed(&error))?;
            }
        }

        let method = if stored { CompressionMethod::Stored } else { CompressionMethod::Deflated };
        let options = SimpleFileOptions::default().compression_method(method);

        for (name, file) in files {
            zip.start_file(name, options).map_err(|error| failed(&error))?;
            let mut input = fs::File::open(file)
                .map_err(|error| format!("could not read {}: {error}", file.display()))?;
            std::io::copy(&mut input, &mut zip).map_err(|error| failed(&error))?;
        }

        zip.finish()
            .map_err(|error| failed(&error))?
            .flush()
            .map_err(|error| failed(&error))?;

        return Ok(());
    });
}

/// The files below the directory by their paths relative to it with /, as an archive names them
pub fn relative_files(directory: &Path) -> Vec<(String, PathBuf)> {
    return files_below(&[directory.to_path_buf()])
        .into_iter()
        .filter_map(|file| {
            let parts: Vec<String> = file.strip_prefix(directory)
                .ok()?
                .iter()
                .map(|part| part.to_string_lossy().to_string())
                .collect();
            return Some((parts.join("/"), file));
        })
        .collect();
}

/// The names of the entries of a zip archive, like a jar
pub fn zip_entries(archive: &Path) -> Result<Vec<String>, String> {
    return open_zip(archive)?
        .file_names()
        .map(|name| name.map(|name| name.to_string()).map_err(|_| format!("{} is no zip archive", archive.display())))
        .collect();
}

/// One file of a zip archive, like a configuration inside a jar
pub fn read_zip_entry(archive: &Path, entry: &str) -> Result<Vec<u8>, String> {
    let mut zip = open_zip(archive)?;
    let mut file = zip.by_name(entry).map_err(|_| format!("{} has no {entry}", archive.display()))?;

    let mut content = vec![];
    file.read_to_end(&mut content)
        .map_err(|error| format!("could not read {entry} of {}: {error}", archive.display()))?;

    return Ok(content);
}

/// The entries of `from` with these names, copied as they are into the new zip archive `to`. Names `from`
/// does not have are left out, without any entry `to` is not written.
pub fn copy_zip_entries(from: &mut ZipReader, names: &[String], to: &Path) -> Result<(), String> {
    let indices: Vec<usize> = names.iter()
        .filter_map(|name| from.index_for_name(name))
        .collect();

    if indices.is_empty() {
        return Ok(());
    }

    let failed = |error: &dyn std::fmt::Display| format!("could not write {}: {error}", to.display());
    let file = fs::File::create(to).map_err(|error| failed(&error))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));

    for index in indices {
        let entry = from.by_index_raw(index).map_err(|error| failed(&error))?;
        zip.raw_copy_file(entry).map_err(|error| failed(&error))?;
    }

    zip.finish()
        .map_err(|error| failed(&error))?
        .flush()
        .map_err(|error| failed(&error))?;

    return Ok(());
}

/// A zip archive read entry by entry
pub type ZipReader = ZipArchive<BufReader<fs::File>>;

pub fn open_zip(archive: &Path) -> Result<ZipReader, String> {
    let file = fs::File::open(archive)
        .map_err(|error| format!("could not read {}: {error}", archive.display()))?;

    return ZipArchive::new(BufReader::new(file)).map_err(|_| format!("{} is no zip archive", archive.display()));
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn zip_archives_are_written_added_to_and_extracted() {
        let directory = ScratchDir::new("zip");
        write_file(&directory.join("content/a.txt"), "a").unwrap();
        write_file(&directory.join("content/lib/x86/libb.so"), "b").unwrap();

        let archive = directory.join("archive.zip");
        add_to_zip(&archive, &relative_files(&directory.join("content")), false).unwrap();
        add_to_zip(&archive, &[("c.txt".to_string(), directory.join("content/a.txt"))], true).unwrap();

        assert_eq!(zip_entries(&archive).unwrap(), ["a.txt", "lib/x86/libb.so", "c.txt"]);
        assert_eq!(read_zip_entry(&archive, "lib/x86/libb.so").unwrap(), b"b");
        assert!(read_zip_entry(&archive, "d.txt").is_err());

        extract(&archive, &directory.join("extracted")).unwrap();
        assert_eq!(read_file(&directory.join("extracted/c.txt")).unwrap(), "a");

        extract_zip(&archive, &directory.join("kept"), |name| name.ends_with(".so")).unwrap();
        assert!(directory.join("kept/lib/x86/libb.so").is_file());
        assert!(!directory.join("kept/a.txt").exists());

    }

    #[test]
    fn zip_entries_are_copied_by_name() {
        let directory = ScratchDir::new("zip-copy");
        write_file(&directory.join("content/a/A.dex"), "a").unwrap();
        write_file(&directory.join("content/b/B.dex"), "b").unwrap();

        let archive = directory.join("classes.zip");
        add_to_zip(&archive, &relative_files(&directory.join("content")), false).unwrap();
        let mut classes = open_zip(&archive).unwrap();

        let copied = directory.join("a.zip");
        copy_zip_entries(&mut classes, &["a/A.dex".to_string(), "a/Missing.dex".to_string()], &copied).unwrap();
        assert_eq!(zip_entries(&copied).unwrap(), ["a/A.dex"]);
        assert_eq!(read_zip_entry(&copied, "a/A.dex").unwrap(), b"a");

        // none of the names, no archive
        let empty = directory.join("empty.zip");
        copy_zip_entries(&mut classes, &["c/C.dex".to_string()], &empty).unwrap();
        assert!(!empty.exists());

    }

    #[cfg(unix)]
    #[test]
    fn extracted_executables_and_links_stay_what_they_were() {
        use std::os::unix::fs::PermissionsExt;

        let directory = ScratchDir::new("zip-tools");
        let archive = directory.join("tools.zip");

        let mut zip = ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.start_file("bin/tool", SimpleFileOptions::default().unix_permissions(0o755)).unwrap();
        zip.write_all(b"#!/bin/sh\n").unwrap();
        zip.add_symlink("bin/alias", "tool", SimpleFileOptions::default()).unwrap();
        zip.finish().unwrap();

        extract(&archive, &directory.join("extracted")).unwrap();

        let mode = fs::metadata(directory.join("extracted/bin/tool")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(fs::read_link(directory.join("extracted/bin/alias")).unwrap(), PathBuf::from("tool"));

        let escaping = directory.join("escaping.zip");
        let mut zip = ZipWriter::new(fs::File::create(&escaping).unwrap());
        zip.add_symlink("lib", "../../outside", SimpleFileOptions::default()).unwrap();
        zip.finish().unwrap();

        let error = extract(&escaping, &directory.join("escaped")).unwrap_err();
        assert!(error.contains("lib links to ../../outside"), "{error}");

    }

    #[test]
    fn tar_archives_are_extracted_plain_and_compressed() {
        let directory = ScratchDir::new("tar");

        for (name, compressed) in [("plain.tar", false), ("compressed.tar.gz", true)] {
            let mut tar = tar::Builder::new(vec![]);
            let mut header = tar::Header::new_gnu();
            header.set_size(6);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "dir/file.txt", &b"inside"[..]).unwrap();
            let bytes = tar.into_inner().unwrap();

            let bytes = match compressed {
                true => {
                    let mut gzip = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
                    gzip.write_all(&bytes).unwrap();
                    gzip.finish().unwrap()
                }
                false => bytes,
            };

            fs::write(directory.join(name), bytes).unwrap();
            extract(&directory.join(name), &directory.join(name).with_extension("extracted")).unwrap();
            assert_eq!(read_file(&directory.join(name).with_extension("extracted").join("dir/file.txt")).unwrap(), "inside");
        }

    }
}
