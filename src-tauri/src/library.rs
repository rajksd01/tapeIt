//! The recordings library: lists the save folder and caches per-file facts, thumbnails and Drive links.
//!
//!   save folder ──scan──▶ [Item] ──▶ dashboard
//!        │                  ▲
//!        └── library.json ──┘   (name → size, modified, facts, thumb, drive link)

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use objc2_foundation::{NSFileManager, NSString, NSURL};
use serde::{Deserialize, Serialize};

use crate::media::{self, Facts};

const VIDEO_EXTENSIONS: [&str; 3] = ["mov", "mp4", "gif"];
const GIF_EXTENSION: &str = "gif";

// The queue thread and the commands both write library.json.
static CACHE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    size: u64,
    modified: u64,
    facts: Option<Facts>,
    thumb: Option<PathBuf>,
    drive_id: Option<String>,
    drive_link: Option<String>,
}

type Cache = HashMap<String, Entry>;

/// One recording as the dashboard shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: u64,
    pub seconds: f64,
    pub width: u32,
    pub height: u32,
    pub thumb: Option<PathBuf>,
    pub drive_link: Option<String>,
}

/// Where the library keeps its files.
pub struct Paths<'a> {
    pub folder: &'a Path,
    pub cache: &'a Path,
    pub thumbs: &'a Path,
}

fn load(file: &Path) -> Cache {
    fs::read(file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn save(file: &Path, cache: &Cache) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let bytes = serde_json::to_vec(cache).map_err(|error| error.to_string())?;
    fs::write(file, bytes).map_err(|error| error.to_string())
}

fn extension_of(path: &Path) -> String {
    path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase()
}

fn file_name(path: &Path) -> Option<String> {
    Some(path.file_name()?.to_string_lossy().to_string())
}

fn thumb_path(thumbs: &Path, name: &str, size: u64, modified: u64) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    (name, size, modified).hash(&mut hasher);
    thumbs.join(format!("{:x}.jpg", hasher.finish()))
}

/// Fills facts and thumbnail for a new or changed file.
fn refresh(entry: &mut Entry, path: &Path, thumbs: &Path, name: &str) {
    entry.facts = media::probe(path);

    // A GIF is its own preview; videos get a JPEG poster frame.
    if extension_of(path) == GIF_EXTENSION {
        entry.thumb = Some(path.to_path_buf());
        return;
    }

    let output = thumb_path(thumbs, name, entry.size, entry.modified);
    entry.thumb = media::thumbnail(path, &output).ok().map(|_| output);
}

/// Lists recordings in the folder, newest first, updating the cache for new or changed files.
pub fn scan(paths: &Paths) -> Result<Vec<Item>, String> {
    let _guard = CACHE_LOCK.lock().unwrap();
    let mut cache = load(paths.cache);
    let _ = fs::create_dir_all(paths.thumbs);

    // A missing folder is an empty library, not an error: nothing was recorded yet.
    let Ok(listing) = fs::read_dir(paths.folder) else {
        return Ok(Vec::new());
    };

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for found in listing.flatten() {
        let path = found.path();
        let Ok(meta) = found.metadata() else {
            continue;
        };
        // Zero bytes means a copy is still in progress.
        if !meta.is_file() || meta.len() == 0 || !VIDEO_EXTENSIONS.contains(&extension_of(&path).as_str()) {
            continue;
        }

        let name = found.file_name().to_string_lossy().to_string();
        let modified = meta.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map_or(0, |time| time.as_secs());
        let entry = cache.entry(name.clone()).or_default();
        let changed = entry.size != meta.len() || entry.modified != modified;
        if changed {
            entry.size = meta.len();
            entry.modified = modified;
            refresh(entry, &path, paths.thumbs, &name);
        }

        let facts = entry.facts.unwrap_or_default();
        items.push(Item {
            path,
            name: name.clone(),
            size: entry.size,
            modified,
            seconds: facts.seconds,
            width: facts.width,
            height: facts.height,
            thumb: entry.thumb.clone(),
            drive_link: entry.drive_link.clone(),
        });
        seen.insert(name);
    }

    // Files deleted or renamed in Finder drop out of the cache.
    cache.retain(|name, _| seen.contains(name));
    save(paths.cache, &cache)?;

    items.sort_by(|left, right| right.modified.cmp(&left.modified));
    Ok(items)
}

/// Checks a user-typed name: no path separators, not empty, not hidden.
fn clean_stem(stem: &str) -> Result<&str, String> {
    let stem = stem.trim();
    if stem.is_empty() || stem.starts_with('.') || stem.contains(['/', ':']) {
        return Err("Use a name without / or : that does not start with a dot".into());
    }
    Ok(stem)
}

/// Renames a recording inside its folder and keeps its extension. Never overwrites.
pub fn rename(cache_file: &Path, path: &Path, stem: &str) -> Result<PathBuf, String> {
    let stem = clean_stem(stem)?;
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or_default();
    let new_name = format!("{stem}.{extension}");
    let target = path.with_file_name(&new_name);

    if target == path {
        return Ok(target);
    }
    if target.exists() {
        return Err("A recording with that name already exists".into());
    }

    let _guard = CACHE_LOCK.lock().unwrap();
    fs::rename(path, &target).map_err(|error| error.to_string())?;

    // Move the cache entry so the Drive link survives the rename.
    let mut cache = load(cache_file);
    if let Some(entry) = file_name(path).and_then(|old| cache.remove(&old)) {
        cache.insert(new_name, entry);
    }
    save(cache_file, &cache)?;
    Ok(target)
}

/// Moves a recording to the Trash, so a delete can be undone from Finder.
pub fn trash(path: &Path) -> Result<(), String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|error| error.localizedDescription().to_string())
}

/// Remembers the Drive file for a recording after an upload.
pub fn set_drive(cache_file: &Path, path: &Path, id: &str, link: &str) -> Result<(), String> {
    let _guard = CACHE_LOCK.lock().unwrap();
    let mut cache = load(cache_file);
    let entry = cache.entry(file_name(path).ok_or("Bad file name")?).or_default();
    entry.drive_id = Some(id.into());
    entry.drive_link = Some(link.into());
    save(cache_file, &cache)
}

/// The Drive link for a recording, if it was uploaded.
#[cfg(test)]
fn drive_link(cache_file: &Path, path: &Path) -> Option<String> {
    let _guard = CACHE_LOCK.lock().unwrap();
    load(cache_file).get(&file_name(path)?)?.drive_link.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox {
        folder: PathBuf,
        cache: PathBuf,
        thumbs: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("screenpen-library-{name}"));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("folder")).unwrap();
            Sandbox { folder: root.join("folder"), cache: root.join("library.json"), thumbs: root.join("thumbs") }
        }

        fn scan(&self) -> Vec<Item> {
            scan(&Paths { folder: &self.folder, cache: &self.cache, thumbs: &self.thumbs }).unwrap()
        }
    }

    #[test]
    fn scan_empty_and_missing_folder() {
        let sandbox = Sandbox::new("empty");
        assert!(sandbox.scan().is_empty());

        let missing = sandbox.folder.join("nope");
        assert!(scan(&Paths { folder: &missing, cache: &sandbox.cache, thumbs: &sandbox.thumbs }).unwrap().is_empty());
    }

    #[test]
    fn scan_skips_partial_and_non_video() {
        let sandbox = Sandbox::new("partial");
        fs::write(sandbox.folder.join("copying.mov"), b"").unwrap();
        fs::write(sandbox.folder.join("notes.txt"), b"hello").unwrap();
        fs::write(sandbox.folder.join("clip.mov"), b"not really a video").unwrap();

        let items = sandbox.scan();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "clip.mov");
    }

    #[test]
    fn scan_external_change_drops_stale_entries() {
        let sandbox = Sandbox::new("external");
        let clip = sandbox.folder.join("clip.mov");
        fs::write(&clip, b"data").unwrap();
        sandbox.scan();
        set_drive(&sandbox.cache, &clip, "id1", "link1").unwrap();
        assert_eq!(sandbox.scan()[0].drive_link.as_deref(), Some("link1"));

        fs::remove_file(&clip).unwrap();
        assert!(sandbox.scan().is_empty());
        assert!(!load(&sandbox.cache).contains_key("clip.mov"));
    }

    #[test]
    fn rename_collision_and_link_survives() {
        let sandbox = Sandbox::new("rename");
        let first = sandbox.folder.join("a.mov");
        fs::write(&first, b"1").unwrap();
        fs::write(sandbox.folder.join("b.mov"), b"2").unwrap();
        sandbox.scan();
        set_drive(&sandbox.cache, &first, "id1", "link1").unwrap();

        assert!(rename(&sandbox.cache, &first, "b").is_err());
        assert!(rename(&sandbox.cache, &first, "../x").is_err());
        assert!(rename(&sandbox.cache, &first, "  ").is_err());

        let renamed = rename(&sandbox.cache, &first, "demo").unwrap();
        assert_eq!(renamed, sandbox.folder.join("demo.mov"));
        assert_eq!(drive_link(&sandbox.cache, &renamed).as_deref(), Some("link1"));
    }
}
