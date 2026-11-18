//! A WebOS library file carries the native catalog and references to immutable account originals.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use lightcraft_engine::catalog::{Catalog, Journal, Source};
use serde::{Deserialize, Serialize};

use crate::files::{Files, LIBRARY_FILES};
use crate::store::hash_of_path;

const FORMAT: &str = "webos-lightcraft-library/1";
const MAX_BYTES: usize = 128 * 1024 * 1024;
const SNAPSHOT: &str = "catalog.snap";
const LOG: &str = "catalog.log";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LibraryFile {
    format: String,
    files: BTreeMap<String, String>,
    originals: Vec<Original>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Original {
    key: String,
    sha256: String,
}

pub struct OpenedLibrary {
    pub files: Files,
    /// Native content hash → server-confirmed SHA-256. The host derives account paths.
    pub originals: BTreeMap<String, String>,
}

fn lowercase_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn original_keys(catalog: &Catalog) -> Result<BTreeSet<&str>, String> {
    catalog
        .photos()
        .filter_map(|photo| match &photo.source {
            Source::Demo { .. } => None,
            Source::File { path } => Some(
                hash_of_path(path)
                    .filter(|hash| lowercase_hex(hash, 32))
                    .ok_or_else(|| "This LightCraft library contains an original outside account storage.".to_string()),
            ),
        })
        .collect()
}

#[derive(Default)]
struct BoundedBytes(Vec<u8>);

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("The LightCraft catalog exceeds the account file size limit."));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Capture the live native catalog without flushing or changing the editor's local journal.
/// Only originals confirmed by the account host can appear in a saved catalog.
pub fn capture(catalog: &Catalog, auxiliary: &Files, confirmed: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
    let originals = original_keys(catalog)?
        .into_iter()
        .map(|key| {
            let sha256 = confirmed
                .get(key)
                .filter(|digest| lowercase_hex(digest, 64))
                .ok_or_else(|| "Wait for LightCraft to finish saving the originals to your account.".to_string())?;
            Ok(Original { key: key.to_owned(), sha256: sha256.clone() })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let snapshot_files = Files::default();
    let (mut writer, _, _) = Journal::open(Box::new(snapshot_files.store())).map_err(|error| error.to_string())?;
    writer.snapshot(catalog).map_err(|error| error.to_string())?;
    let mut files = BTreeMap::new();
    for name in LIBRARY_FILES {
        let bytes = match name {
            SNAPSHOT | LOG => snapshot_files.get(name),
            _ => auxiliary.get(name),
        };
        if let Some(bytes) = bytes {
            let text = String::from_utf8(bytes).map_err(|_| format!("{name} did not contain UTF-8 library data."))?;
            files.insert(name.to_owned(), text);
        }
    }
    let library = LibraryFile { format: FORMAT.to_owned(), files, originals };
    let mut encoded = BoundedBytes::default();
    serde_json::to_writer(&mut encoded, &library).map_err(|error| error.to_string())?;
    Ok(encoded.0)
}

/// Validate the native catalog in an isolated mirror before any live or persistent state changes.
pub fn open(bytes: &[u8]) -> Result<OpenedLibrary, String> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err("Invalid LightCraft account catalog size.".into());
    }
    let file: LibraryFile = serde_json::from_slice(bytes).map_err(|error| format!("Invalid LightCraft account library: {error}"))?;
    if file.format != FORMAT || !file.files.contains_key(SNAPSHOT) || !file.files.keys().all(|name| LIBRARY_FILES.contains(&name.as_str())) {
        return Err("Unsupported LightCraft account library format.".into());
    }
    let files = Files::default();
    for (name, data) in file.files {
        files.preload(&name, data.into_bytes());
    }
    let (_, catalog, report) = Journal::open(Box::new(files.store())).map_err(|error| error.to_string())?;
    if report.failed != 0 || report.torn_bytes != 0 || report.torn_fragments != 0 || report.damaged.is_some() || !report.repaired.is_empty() {
        return Err("The LightCraft account catalog contains damaged native records.".into());
    }
    let mut originals = BTreeMap::new();
    for Original { key, sha256 } in file.originals {
        if !lowercase_hex(&key, 32) || !lowercase_hex(&sha256, 64) || originals.insert(key, sha256).is_some() {
            return Err("Invalid LightCraft account original reference.".into());
        }
    }
    let expected = original_keys(&catalog)?;
    if expected.len() != originals.len() || !expected.iter().all(|key| originals.contains_key(*key)) {
        return Err("The LightCraft account library is missing an original reference.".into());
    }
    Ok(OpenedLibrary { files, originals })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_engine::catalog::{Op, Photo, PhotoId};

    fn library() -> (Catalog, String) {
        let key = "a".repeat(32);
        let mut catalog = Catalog::new();
        let photo = Photo::new(PhotoId(1), Source::File { path: format!("web/{key}/original.jpg") }, "original.jpg", "JPEG", 12, 10, "2026-11-18");
        catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        (catalog, key)
    }

    #[test]
    fn round_trip_preserves_native_catalog_and_detached_original() {
        let (catalog, key) = library();
        let confirmed = BTreeMap::from([(key, "b".repeat(64))]);
        let auxiliary = Files::default();
        auxiliary.preload("view.json", b"{}".to_vec());
        let payload = capture(&catalog, &auxiliary, &confirmed).unwrap();
        let opened = open(&payload).unwrap();
        let (_, restored, _) = Journal::open(Box::new(opened.files.store())).unwrap();
        assert_eq!(restored.to_snapshot(), catalog.to_snapshot());
        assert_eq!(opened.files.get("view.json").as_deref(), Some(b"{}".as_slice()));
        assert_eq!(opened.originals, confirmed);
        assert!(auxiliary.get(SNAPSHOT).is_none());
    }

    #[test]
    fn refuses_to_save_unconfirmed_and_open_unbound_originals() {
        let (catalog, key) = library();
        let auxiliary = Files::default();
        assert!(capture(&catalog, &auxiliary, &BTreeMap::new()).is_err());
        let payload = capture(&catalog, &auxiliary, &BTreeMap::from([(key, "b".repeat(64))])).unwrap();
        let mut decoded: LibraryFile = serde_json::from_slice(&payload).unwrap();
        decoded.originals.clear();
        assert!(open(&serde_json::to_vec(&decoded).unwrap()).is_err());
    }
}
