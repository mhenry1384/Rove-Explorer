use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
struct WatcherState {
    watcher: Mutex<Option<RecommendedWatcher>>,
    // Watched path -> whether it is watched recursively (tree view needs changes from every subfolder).
    watched: Mutex<HashMap<String, bool>>,
}

#[derive(Default)]
struct TreeState {
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

#[derive(Default)]
struct IconCacheState {
    cache: Mutex<HashMap<String, Option<String>>>,
}

#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Default)]
struct ClipboardState {
    files: Mutex<Option<(Vec<String>, bool)>>,
}

#[derive(Serialize)]
struct ClipboardFiles {
    paths: Vec<String>,
    cut: bool,
}

#[derive(Serialize)]
struct Entry {
    kind: String,
    name: String,
    extension: String,
    size: String,
    bytes: u64,
    modified: String,
    path: String,
}

#[derive(Serialize)]
struct Drive {
    kind: String,
    name: String,
    path: String,
    total: String,
    free: String,
    #[serde(rename = "fileSystem")]
    file_system: String,
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 { return format!("{} B", bytes); }
    if bytes < 1024 * 1024 { return format!("{:.1} KB", bytes as f64 / 1024.0); }
    if bytes < 1024 * 1024 * 1024 { return format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)); }
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn entry_kind_rank(kind: &str) -> u8 {
    if kind == "folder" { 0 } else { 1 }
}

#[cfg(windows)]
fn is_hidden(path: &Path, name: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;

    extern "system" {
        fn GetFileAttributesW(path: *const u16) -> u32;
    }

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attributes = unsafe { GetFileAttributesW(wide_path.as_ptr()) };
    name.starts_with('.') || (attributes != u32::MAX && attributes & (0x2 | 0x4) != 0)
}

// Finder also hides items carrying the UF_HIDDEN flag (~/Library, /bin, /usr, ...), not just dotfiles.
#[cfg(target_os = "macos")]
fn is_hidden(path: &Path, name: &str) -> bool {
    use std::os::macos::fs::MetadataExt;
    const UF_HIDDEN: u32 = 0x8000;
    name.starts_with('.') || fs::symlink_metadata(path).map(|metadata| metadata.st_flags() & UF_HIDDEN != 0).unwrap_or(false)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn is_hidden(_path: &Path, name: &str) -> bool {
    name.starts_with('.')
}

#[cfg(windows)]
fn drive_details(letter: char) -> Option<Drive> {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn GetDiskFreeSpaceExW(directory: *const u16, available: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32, serial: *mut u32, max_component: *mut u32, flags: *mut u32, filesystem: *mut u16, filesystem_len: u32) -> i32;
    }

    let path = format!("{}:\\", letter);
    let wide_path: Vec<u16> = std::ffi::OsStr::new(&path).encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    let mut total = 0;
    let mut free = 0;
    if unsafe { GetDiskFreeSpaceExW(wide_path.as_ptr(), &mut available, &mut total, &mut free) } == 0 { return None; }
    let mut filesystem = [0u16; 32];
    let mut volume_name = [0u16; 256];
    let mut serial = 0;
    let mut max_component = 0;
    let mut flags = 0;
    let file_system = if unsafe { GetVolumeInformationW(wide_path.as_ptr(), volume_name.as_mut_ptr(), volume_name.len() as u32, &mut serial, &mut max_component, &mut flags, filesystem.as_mut_ptr(), filesystem.len() as u32) } != 0 {
        String::from_utf16_lossy(&filesystem[..filesystem.iter().position(|value| *value == 0).unwrap_or(filesystem.len())])
    } else { "—".to_string() };
    Some(Drive { kind: "drive".to_string(), name: format!("{}:", letter), path, total: format_size(total), free: format_size(free), file_system })
}

#[cfg(windows)]
#[tauri::command]
fn list_drives() -> Vec<Drive> {
    (b'A'..=b'Z').filter_map(|letter| drive_details(letter as char)).collect()
}

// A cheap fingerprint of the mounted drives, polled to notice drives being added or removed.
#[cfg(windows)]
fn drive_signature() -> Vec<String> {
    extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    (0..26).filter(|bit| mask & (1 << bit) != 0).map(|bit| ((b'A' + bit as u8) as char).to_string()).collect()
}

// macOS mounts several system volumes (/System/Volumes/Data, VM, Preboot, ...) that Finder never
// shows; only the startup disk and what's under /Volumes are drives a user would browse.
#[cfg(not(windows))]
fn is_user_volume(mount_point: &Path) -> bool {
    !cfg!(target_os = "macos") || mount_point == Path::new("/") || mount_point.starts_with("/Volumes")
}

#[cfg(not(windows))]
fn drive_signature() -> Vec<String> {
    sysinfo::Disks::new_with_refreshed_list().list().iter().filter(|disk| is_user_volume(disk.mount_point())).map(|disk| disk.mount_point().to_string_lossy().to_string()).collect()
}

// Emits "drives-changed" whenever a drive is mounted or unmounted.
fn watch_drives(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = drive_signature();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let current = drive_signature();
            if current != last {
                last = current;
                let _ = app.emit("drives-changed", ());
            }
        }
    });
}

#[cfg(not(windows))]
#[tauri::command]
fn list_drives() -> Vec<Drive> {
    let mut drives: Vec<Drive> = sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
        .filter(|disk| is_user_volume(disk.mount_point()))
        .map(|disk| {
            let path = disk.mount_point().to_string_lossy().to_string();
            let label = disk.name().to_string_lossy().to_string();
            let name = if label.is_empty() { path.clone() } else { label };
            Drive {
                kind: "drive".to_string(),
                name,
                path,
                total: format_size(disk.total_space()),
                free: format_size(disk.available_space()),
                file_system: disk.file_system().to_string_lossy().to_string(),
            }
        })
        .collect();
    drives.sort_by(|left, right| left.path.cmp(&right.path));
    drives
}

fn entry_from_metadata(path: &Path, name: String, metadata: &fs::Metadata) -> Entry {
    let is_dir = metadata.is_dir();
    let extension = if is_dir { "—".to_string() } else {
        Path::new(&name).extension().map(|value| format!(".{}", value.to_string_lossy())).unwrap_or_else(|| "—".to_string())
    };
    Entry {
        kind: if is_dir { "folder" } else { "file" }.to_string(),
        name,
        extension,
        size: if is_dir { "—".to_string() } else { format_size(metadata.len()) },
        bytes: if is_dir { 0 } else { metadata.len() },
        modified: metadata.modified().ok().and_then(|date| date.duration_since(std::time::UNIX_EPOCH).ok()).map(|date| date.as_secs().to_string()).unwrap_or_else(|| "—".to_string()),
        path: path.to_string_lossy().to_string(),
    }
}

// Listing details for the given paths (the search results on screen); paths that are gone are left out.
#[tauri::command]
fn stat_entries(paths: Vec<String>) -> Vec<Entry> {
    paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path);
            let metadata = fs::metadata(path).ok()?;
            let name = path.file_name()?.to_string_lossy().to_string();
            Some(entry_from_metadata(path, name, &metadata))
        })
        .collect()
}

#[tauri::command]
fn read_directory(path: String, show_hidden: bool) -> Result<Vec<Entry>, String> {
    let mut entries = fs::read_dir(&path).map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter_map(|item| {
            let metadata = item.metadata().ok()?;
            let name = item.file_name().to_string_lossy().to_string();
            if !show_hidden && is_hidden(&item.path(), &name) { return None; }
            Some(entry_from_metadata(&item.path(), name, &metadata))
        }).collect::<Vec<_>>();
    entries.sort_by(|left, right| entry_kind_rank(&left.kind).cmp(&entry_kind_rank(&right.kind)).then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())));
    Ok(entries)
}

#[cfg(windows)]
const INVALID_NAME_CHARS: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
// Finder shows a ':' in a name as '/', and the filesystem layer swaps them, so allow neither.
#[cfg(target_os = "macos")]
const INVALID_NAME_CHARS: &[char] = &['/', ':'];
#[cfg(not(any(windows, target_os = "macos")))]
const INVALID_NAME_CHARS: &[char] = &['/'];

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(INVALID_NAME_CHARS)
}

// Where a rename would put `path`, refusing bad names and anything that would overwrite another item.
fn rename_target(source: &Path, new_name: &str) -> Result<PathBuf, String> {
    if !is_valid_name(new_name) {
        return Err("Invalid folder name".to_string());
    }
    let target = source.parent().ok_or_else(|| "Invalid folder path".to_string())?.join(new_name);
    // fs::rename silently replaces an existing file, so refuse instead. On a case-insensitive drive a
    // case-only rename ("photo" -> "Photo") also "exists", but both paths resolve to the same item.
    if target.exists() && fs::canonicalize(source).ok() != fs::canonicalize(&target).ok() {
        return Err(format!("An item named \"{new_name}\" already exists in this folder."));
    }
    Ok(target)
}

#[tauri::command]
fn rename_entry(path: String, new_name: String) -> Result<(), String> {
    let source = Path::new(&path);
    let target = rename_target(source, &new_name)?;
    fs::rename(source, &target).map_err(|error| {
        // A folder that's in use also reports access denied, so check whether permissions are really the cause.
        if error.raw_os_error() == Some(5) && access_denied(source, DELETE) { NEEDS_ELEVATION.to_string() } else { readable_rename_error(&error, source.is_dir()) }
    })
}

// Windows reports a locked file as a sharing violation (32) or lock violation (33). A folder that
// something has open (a file inside it, or a program's working directory) comes back as access
// denied (5) instead, which is what Explorer calls "Folder In Use".
fn readable_rename_error(error: &std::io::Error, is_dir: bool) -> String {
    match error.raw_os_error() {
        Some(32) | Some(33) => "It is being used by another program.".to_string(),
        Some(5) if is_dir => "It is being used by another program.".to_string(),
        _ => error.to_string(),
    }
}

// One thing that went to the trash. `id` is where it sits now on Windows (a $R item in the Recycle
// Bin) and macOS, and its .trashinfo file elsewhere; `original` is where it came from.
#[derive(Serialize, Deserialize, Debug)]
struct TrashedItem {
    id: String,
    original: String,
}

// A deleted path and what Undo needs to put it back. On Windows the trash crate recycles a folder's
// files one by one and then the (by then empty) folder, so a folder brings several items. `items` is
// empty when nothing could be found (e.g. a drive without a Recycle Bin deletes outright).
#[derive(Serialize, Deserialize)]
struct TrashedEntry {
    path: String,
    items: Vec<TrashedItem>,
}

#[derive(Serialize)]
struct DeleteFailure {
    path: String,
    error: String,
}

#[derive(Serialize)]
struct DeleteOutcome {
    trashed: Vec<TrashedEntry>,
    failures: Vec<DeleteFailure>,
}

#[cfg(windows)]
const TRASH_NAME: &str = "Recycle Bin";
#[cfg(not(windows))]
const TRASH_NAME: &str = "Trash";

#[tauri::command]
fn delete_entries(paths: Vec<String>) -> DeleteOutcome {
    // Deletion times are kept in whole seconds, so allow some slack.
    #[cfg(not(target_os = "macos"))]
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_secs() as i64).unwrap_or(0) - 2;
    let mut outcome = DeleteOutcome { trashed: Vec::new(), failures: Vec::new() };
    for path in paths {
        match delete_entry(path.clone()) {
            Ok(items) => outcome.trashed.push(TrashedEntry { path, items }),
            Err(error) => outcome.failures.push(DeleteFailure { path, error }),
        }
    }
    #[cfg(not(target_os = "macos"))]
    find_trashed_items(&mut outcome.trashed, started);
    outcome
}

fn delete_entry(path: String) -> Result<Vec<TrashedItem>, String> {
    let target = Path::new(&path);
    // Windows refuses to delete a protected item (e.g. a system file at the root of C:) the same
    // way it refuses one that's in use, so check whether permissions are really the cause.
    let items = move_to_trash(&path).map_err(|error| if access_denied(target, DELETE) { NEEDS_ELEVATION.to_string() } else { error })?;
    // The shell recycles silently: an item that is in use is skipped yet the operation still
    // reports success, so check what is left behind. Whatever survived inside a folder is what
    // the other program has open (everything else already went to the Recycle Bin).
    if !still_exists(target) { return Ok(items); }
    if access_denied(target, DELETE) { return Err(NEEDS_ELEVATION.to_string()); }
    let mut remaining = Vec::new();
    if target.is_dir() { collect_files(target, target, &mut remaining); }
    if remaining.iter().any(|name| access_denied(&target.join(name), DELETE)) { return Err(NEEDS_ELEVATION.to_string()); }
    if remaining.is_empty() {
        return Err("It is being used by another program.".to_string());
    }
    let shown = remaining.iter().take(5).map(|name| format!("  {name}")).collect::<Vec<_>>().join("\n");
    let more = if remaining.len() > 5 { format!("\n  ...and {} more", remaining.len() - 5) } else { String::new() };
    Err(format!("These items inside it are being used by another program:\n{shown}{more}"))
}

// A file whose details can't be read (e.g. one only SYSTEM may open) is still there.
fn still_exists(path: &Path) -> bool {
    !matches!(path.try_exists(), Ok(false))
}

// The trashed items are looked up afterwards by find_trashed_items, once for the whole batch, since
// listing the trash is slow.
#[cfg(not(target_os = "macos"))]
fn move_to_trash(path: &str) -> Result<Vec<TrashedItem>, String> {
    trash::delete(path).map_err(readable_trash_error)?;
    Ok(Vec::new())
}

// NSFileManager, unlike the trash crate's Finder route, reports where the item landed in the Trash.
#[cfg(target_os = "macos")]
fn move_to_trash(path: &str) -> Result<Vec<TrashedItem>, String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let mut resulting_url = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting_url))
        .map_err(|error| error.localizedDescription().to_string())?;
    let id = resulting_url.and_then(|url| url.path()).map(|path| path.to_string());
    Ok(id.map(|id| TrashedItem { id, original: path.to_string() }).into_iter().collect())
}

#[cfg(not(target_os = "macos"))]
fn find_trashed_items(trashed: &mut [TrashedEntry], started: i64) {
    if trashed.is_empty() { return; }
    let Ok(items) = trash::os_limited::list() else { return };
    for entry in trashed.iter_mut() {
        let root = comparable_path(Path::new(&entry.path));
        let inside = format!("{root}{}", std::path::MAIN_SEPARATOR);
        // Anything from this path deleted before this batch began was deleted separately, earlier.
        entry.items = items
            .iter()
            .filter(|item| item.time_deleted >= started)
            .filter(|item| { let original = comparable_path(&item.original_path()); original == root || original.starts_with(&inside) })
            .map(|item| TrashedItem { id: item.id.to_string_lossy().into_owned(), original: item.original_path().to_string_lossy().into_owned() })
            .collect();
    }
}

// Windows paths ignore case and accept either slash.
#[cfg(not(target_os = "macos"))]
fn comparable_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) { text.replace('/', "\\").trim_end_matches('\\').to_lowercase() } else { text.trim_end_matches('/').to_string() }
}

fn item_name(path: &str) -> String {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

// Puts deleted items back where they came from. Items whose old spot is taken, or that are no longer
// in the trash, are left alone and reported; the rest are still restored.
#[tauri::command]
fn restore_entries(entries: Vec<TrashedEntry>) -> Result<(), String> {
    let mut problems = Vec::new();
    for entry in entries {
        let name = item_name(&entry.path);
        if Path::new(&entry.path).exists() {
            problems.push(format!("{name}: an item with that name is already there."));
        } else if entry.items.is_empty() {
            problems.push(format!("{name}: it's no longer in the {TRASH_NAME}."));
        } else if let Err(error) = restore_from_trash(entry.items) {
            problems.push(format!("{name}: {error}"));
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems.join("\n")) }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn restore_from_trash(items: Vec<TrashedItem>) -> Result<(), String> {
    let ids: std::collections::HashSet<String> = items.into_iter().map(|item| item.id).collect();
    let found: Vec<_> = trash::os_limited::list().map_err(readable_trash_error)?.into_iter().filter(|item| ids.contains(item.id.to_string_lossy().as_ref())).collect();
    if found.is_empty() { return Err(format!("it's no longer in the {TRASH_NAME}.")); }
    trash::os_limited::restore_all(found).map_err(readable_trash_error)
}

// Here an item's id is where it sits on disk, so it just moves back, folders before what was in
// them. Windows also keeps a $I file beside each $R item recording where it came from; that goes
// too, or the Recycle Bin would go on listing the item.
#[cfg(any(windows, target_os = "macos"))]
fn restore_from_trash(mut items: Vec<TrashedItem>) -> Result<(), String> {
    items.sort_by_key(|item| Path::new(&item.original).components().count());
    let mut missing = false;
    for item in &items {
        let stored = Path::new(&item.id);
        let original = Path::new(&item.original);
        if !stored.exists() { missing = true; continue; }
        if let Some(parent) = original.parent() { fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
        fs::rename(stored, original).map_err(|error| error.to_string())?;
        #[cfg(windows)]
        if let Some(suffix) = stored.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_prefix("$R")) {
            let _ = fs::remove_file(stored.with_file_name(format!("$I{suffix}")));
        }
    }
    if missing { Err(format!("some of it is no longer in the {TRASH_NAME}.")) } else { Ok(()) }
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() { collect_files(root, &path, out); continue; }
        out.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
    }
}

// trash reports Windows failures as "windows error: <message> (0x8007....)"; keep just the message
// so it can be shown to the user (e.g. "...being used by another process.").
fn readable_trash_error(error: trash::Error) -> String {
    let text = match error {
        trash::Error::Os { description, .. } | trash::Error::Unknown { description } => description,
        other => other.to_string(),
    };
    let text = text.strip_prefix("windows error: ").unwrap_or(&text);
    let text = match text.rfind(" (0x") { Some(index) if text.ends_with(')') => &text[..index], _ => text };
    text.trim().to_string()
}

fn unused_name(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{stem}{extension}"));
    let mut attempt = 2u32;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} {attempt}{extension}"));
        attempt += 1;
    }
    candidate
}

#[tauri::command]
fn create_file(dest_dir: String) -> Result<String, String> {
    let candidate = unused_name(Path::new(&dest_dir), "New File", ".txt");
    fs::File::create(&candidate).map_err(|error| create_error(&candidate, error))?;
    Ok(candidate.to_string_lossy().to_string())
}

#[tauri::command]
fn create_folder(dest_dir: String) -> Result<String, String> {
    let candidate = unused_name(Path::new(&dest_dir), "New Folder", "");
    fs::create_dir(&candidate).map_err(|error| create_error(&candidate, error))?;
    Ok(candidate.to_string_lossy().to_string())
}

// Access denied is only recoverable on Windows, where the frontend can offer a UAC prompt.
fn create_error(candidate: &Path, error: std::io::Error) -> String {
    if cfg!(windows) && error.raw_os_error() == Some(5) {
        format!("{NEEDS_ELEVATION}{}", candidate.file_name().unwrap_or_default().to_string_lossy())
    } else {
        error.to_string()
    }
}

// The error from an operation that needs administrator rights. Creating adds a suggested unused name.
const NEEDS_ELEVATION: &str = "needs-elevation:";

const FILE_ADD_FILE: u32 = 0x2;
const FILE_ADD_SUBDIRECTORY: u32 = 0x4;
const DELETE: u32 = 0x1_0000;

// Whether Windows refuses this kind of access to a file or folder. It only asks for the access,
// so nothing on disk changes. Anything other than access denied (e.g. a sharing violation) is false.
#[cfg(windows)]
fn access_denied(path: &Path, access: u32) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000; // required to open a folder
    fs::OpenOptions::new().access_mode(access).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).err().and_then(|error| error.raw_os_error()) == Some(5)
}

#[cfg(not(windows))]
fn access_denied(_path: &Path, _access: u32) -> bool {
    false
}

// The name is chosen before elevating because the new item belongs to the administrators, so
// renaming it afterwards would need a second UAC prompt. Returns None if the UAC prompt is declined.
// The elevated commands run off the main thread (async) so the window keeps painting during the wait.
#[tauri::command(async)]
fn create_entry_elevated(window: tauri::WebviewWindow, dest_dir: String, name: String, is_folder: bool) -> Result<Option<String>, String> {
    if !is_valid_name(&name) { return Err("That name can't be used here.".to_string()); }
    let target = Path::new(&dest_dir).join(&name);
    if target.exists() { return Err(format!("An item named \"{name}\" already exists in this folder.")); }
    let target = target.to_string_lossy().into_owned();
    let flag = if is_folder { ELEVATED_CREATE_FOLDER } else { ELEVATED_CREATE_FILE };
    Ok(run_elevated(&window, flag, &[&target])?.then_some(target))
}

// False if the UAC prompt is declined.
#[tauri::command(async)]
fn rename_entry_elevated(window: tauri::WebviewWindow, path: String, new_name: String) -> Result<bool, String> {
    let target = rename_target(Path::new(&path), &new_name)?;
    run_elevated(&window, ELEVATED_RENAME, &[&path, &target.to_string_lossy()])
}

// A paste can list more paths than fit on a command line, so the elevated copy reads them from a file.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Serialize, Deserialize)]
struct PasteJob {
    paths: Vec<String>,
    dest_dir: String,
    cut: bool,
    conflict_resolution: Option<String>,
}

// The job decides what runs as administrator, so the returned handle stays open until the elevated
// process has finished: it lets others read the file but stops them changing or replacing it.
#[cfg(windows)]
fn create_job_file(job: &impl Serialize) -> std::io::Result<(fs::File, PathBuf)> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 1;
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = std::env::temp_dir().join(format!("rove-job-{}-{unique}.json", std::process::id()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).share_mode(FILE_SHARE_READ).open(&path)?;
    file.write_all(&serde_json::to_vec(job)?)?;
    Ok((file, path))
}

#[cfg(windows)]
fn run_paste_job(job_path: &Path) -> std::io::Result<()> {
    let job: PasteJob = serde_json::from_str(&fs::read_to_string(job_path)?)?;
    paste_items(&job.paths, Path::new(&job.dest_dir), job.cut, job.conflict_resolution.as_deref()).map(|_| ())
}

// Returns what's new in the destination, or None if the UAC prompt is declined. The elevated copy
// can't safely report back: as administrator it shouldn't write to a place a normal program controls.
#[tauri::command(async)]
fn paste_entries_elevated(window: tauri::WebviewWindow, paths: Vec<String>, dest_dir: String, cut: bool, conflict_resolution: Option<String>) -> Result<Option<Vec<String>>, String> {
    #[cfg(windows)]
    {
        let names = || -> std::collections::HashSet<std::ffi::OsString> { fs::read_dir(&dest_dir).into_iter().flatten().flatten().map(|entry| entry.file_name()).collect() };
        let before = names();
        let (job_file, job_path) = create_job_file(&PasteJob { paths, dest_dir: dest_dir.clone(), cut, conflict_resolution }).map_err(|error| error.to_string())?;
        let finished = run_elevated(&window, ELEVATED_PASTE, &[&job_path.to_string_lossy()]);
        drop(job_file);
        let _ = fs::remove_file(&job_path);
        if !finished? { return Ok(None); }
        Ok(Some(names().difference(&before).map(|name| Path::new(&dest_dir).join(name).to_string_lossy().into_owned()).collect()))
    }
    #[cfg(not(windows))]
    {
        let _ = (window, paths, dest_dir, cut, conflict_resolution);
        Err("Pasting as administrator is only supported on Windows.".to_string())
    }
}

// Recycles the paths as administrator. None if the UAC prompt is declined. Whatever is still there
// afterwards failed; what went can't be undone, since putting it back needs administrator rights too.
#[tauri::command(async)]
fn delete_entries_elevated(window: tauri::WebviewWindow, paths: Vec<String>) -> Result<Option<DeleteOutcome>, String> {
    #[cfg(windows)]
    {
        let (job_file, job_path) = create_job_file(&paths).map_err(|error| error.to_string())?;
        let finished = run_elevated(&window, ELEVATED_DELETE, &[&job_path.to_string_lossy()]);
        drop(job_file);
        let _ = fs::remove_file(&job_path);
        let error = match finished { Ok(false) => return Ok(None), Ok(true) => None, Err(error) => Some(error) };
        let mut outcome = DeleteOutcome { trashed: Vec::new(), failures: Vec::new() };
        for path in paths {
            if still_exists(Path::new(&path)) {
                let error = error.clone().unwrap_or_else(|| "It is being used by another program.".to_string());
                outcome.failures.push(DeleteFailure { path, error });
            } else {
                outcome.trashed.push(TrashedEntry { path, items: Vec::new() });
            }
        }
        Ok(Some(outcome))
    }
    #[cfg(not(windows))]
    {
        let _ = (window, paths);
        Err("Deleting as administrator is only supported on Windows.".to_string())
    }
}

// Runs as administrator. Goes on past a failure, then reports the last one: access denied (5) if
// even an administrator may not delete it, otherwise a sharing violation (32), as it's in use.
#[cfg(windows)]
fn run_delete_job(job_path: &Path) -> std::io::Result<()> {
    let paths: Vec<String> = serde_json::from_str(&fs::read_to_string(job_path)?)?;
    let mut result = Ok(());
    for path in &paths {
        let _ = trash::delete(path);
        let target = Path::new(path);
        if still_exists(target) { result = Err(std::io::Error::from_raw_os_error(if access_denied(target, DELETE) { 5 } else { 32 })); }
    }
    result
}

#[cfg(windows)]
fn run_elevated(window: &tauri::WebviewWindow, flag: &str, paths: &[&str]) -> Result<bool, String> {
    let owner = window.hwnd().map(|handle| handle.0 as isize).unwrap_or(0);
    win_shell::run_self_elevated(flag, paths, owner)
}

#[cfg(not(windows))]
fn run_elevated(_window: &tauri::WebviewWindow, _flag: &str, _paths: &[&str]) -> Result<bool, String> {
    Err("Administrator access is only supported on Windows.".to_string())
}

// The arguments that make this program do one privileged job (see elevated_helper_exit_code).
const ELEVATED_CREATE_FOLDER: &str = "--elevated-create-folder";
const ELEVATED_CREATE_FILE: &str = "--elevated-create-file";
const ELEVATED_RENAME: &str = "--elevated-rename";
#[cfg(windows)]
const ELEVATED_PASTE: &str = "--elevated-paste";
#[cfg(windows)]
const ELEVATED_DELETE: &str = "--elevated-delete";

// The exit code when this process is the copy of the app that UAC launched for one privileged job,
// which it does before exiting without opening a window. None for a normal launch.
#[cfg(windows)]
fn elevated_helper_exit_code(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<i32> {
    const ERROR_INVALID_PARAMETER: i32 = 87;
    let flag = args.next()?;
    let flag = flag.to_str()?;
    let paths: Vec<std::ffi::OsString> = args.collect();
    let result = match (flag, paths.as_slice()) {
        (ELEVATED_CREATE_FOLDER, [path]) => fs::create_dir(path),
        (ELEVATED_CREATE_FILE, [path]) => fs::File::create_new(path).map(|_| ()),
        (ELEVATED_RENAME, [from, to]) => fs::rename(from, to),
        (ELEVATED_PASTE, [job]) => run_paste_job(Path::new(job)),
        (ELEVATED_DELETE, [job]) => run_delete_job(Path::new(job)),
        (ELEVATED_CREATE_FOLDER | ELEVATED_CREATE_FILE | ELEVATED_RENAME | ELEVATED_PASTE | ELEVATED_DELETE, _) => return Some(ERROR_INVALID_PARAMETER),
        _ => return None,
    };
    Some(match result { Ok(()) => 0, Err(error) => error.raw_os_error().unwrap_or(1) })
}

#[tauri::command]
fn platform_name() -> &'static str {
    std::env::consts::OS
}

// The first group of candidate folders that has any folder that exists, with duplicates removed
// (the same folder can turn up twice, e.g. under the OneDrive and OneDriveConsumer variables).
fn first_existing(groups: Vec<Vec<PathBuf>>) -> Vec<String> {
    for group in groups {
        let mut keys = Vec::new();
        let mut found = Vec::new();
        for path in group.into_iter().filter(|path| path.is_dir()) {
            let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if keys.contains(&key) { continue; }
            keys.push(key);
            found.push(path.to_string_lossy().to_string());
        }
        if !found.is_empty() { return found; }
    }
    Vec::new()
}

// Folders that macOS cloud providers keep under ~/Library/CloudStorage, one per account,
// named like OneDrive-Personal or GoogleDrive-someone@example.com.
fn cloud_storage_folders(prefix: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut folders: Vec<PathBuf> = fs::read_dir(home.join("Library").join("CloudStorage")).into_iter().flatten().filter_map(Result::ok)
        .filter(|item| item.file_name().to_string_lossy().starts_with(prefix))
        .map(|item| item.path())
        .collect();
    folders.sort();
    folders
}

// The OneDrive client sets one variable per signed-in account on Windows; on macOS each account
// is a File Provider folder. ~/OneDrive is only a last resort, for older clients.
fn onedrive_folders() -> Vec<String> {
    let mut accounts: Vec<PathBuf> = ["OneDriveConsumer", "OneDriveCommercial", "OneDrive"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    accounts.extend(cloud_storage_folders("OneDrive-"));
    first_existing(vec![accounts, dirs::home_dir().map(|home| home.join("OneDrive")).into_iter().collect()])
}

// My Drive folders that Google Drive for desktop mirrors to local disk, as recorded in its settings database.
fn google_drive_mirrors() -> Vec<PathBuf> {
    let Some(database) = dirs::data_local_dir().map(|dir| dir.join("Google").join("DriveFS").join("root_preference_sqlite.db")) else { return Vec::new() };
    let Ok(connection) = rusqlite::Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return Vec::new() };
    let Ok(mut statement) = connection.prepare("SELECT last_seen_absolute_path FROM roots WHERE is_my_drive = 1") else { return Vec::new() };
    statement.query_map([], |row| row.get::<_, String>(0)).map(|rows| rows.filter_map(Result::ok).map(PathBuf::from).collect()).unwrap_or_default()
}

// Drive letters Google Drive for desktop streams files on, recognised by their volume label.
#[cfg(windows)]
fn google_drive_volumes() -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32, serial: *mut u32, max_component: *mut u32, flags: *mut u32, filesystem: *mut u16, filesystem_len: u32) -> i32;
    }
    drive_signature().into_iter().filter_map(|letter| {
        let root = format!("{}:\\", letter);
        let wide_root: Vec<u16> = std::ffi::OsStr::new(&root).encode_wide().chain(Some(0)).collect();
        let mut label = [0u16; 256];
        if unsafe { GetVolumeInformationW(wide_root.as_ptr(), label.as_mut_ptr(), label.len() as u32, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0) } == 0 { return None; }
        let label = String::from_utf16_lossy(&label[..label.iter().position(|value| *value == 0).unwrap_or(label.len())]);
        (label == "Google Drive").then(|| PathBuf::from(root))
    }).collect()
}

#[cfg(not(windows))]
fn google_drive_volumes() -> Vec<PathBuf> {
    Vec::new()
}

// A mirrored My Drive wins over the streamed one, since that's the copy the user chose to keep locally;
// ~/Google Drive is where the old Backup and Sync client put it.
fn google_drive_folders() -> Vec<String> {
    let streamed = cloud_storage_folders("GoogleDrive-").into_iter().chain(google_drive_volumes()).map(|root| root.join("My Drive")).collect();
    first_existing(vec![google_drive_mirrors(), streamed, dirs::home_dir().map(|home| home.join("Google Drive")).into_iter().collect()])
}

// Where this computer keeps the standard user folders and any cloud drive folders, asking the OS
// (known folders on Windows, XDG user dirs on Linux) rather than assuming they sit in the home folder.
// Folders that don't exist are left out; the cloud drives can have one folder per account.
#[tauri::command]
fn known_folders() -> HashMap<&'static str, Vec<String>> {
    let standard = [("desktop", dirs::desktop_dir()), ("documents", dirs::document_dir()), ("downloads", dirs::download_dir()), ("pictures", dirs::picture_dir()), ("music", dirs::audio_dir()), ("videos", dirs::video_dir())];
    let mut folders: HashMap<&'static str, Vec<String>> = standard.into_iter().map(|(key, path)| (key, first_existing(vec![path.into_iter().collect()]))).collect();
    folders.insert("onedrive", onedrive_folders());
    folders.insert("gdrive", google_drive_folders());
    folders
}

// The bookmark list lives in the per-user config folder (%APPDATA%\com.roveexplorer.app on Windows,
// ~/Library/Application Support/com.roveexplorer.app on macOS) so reinstalling the app leaves it alone.
// The copy bundled from config/folders.json is the default, written there the first time it's needed.
const DEFAULT_FOLDERS_CONFIG: &str = include_str!("../../config/folders.json");

fn folders_config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_config_dir().map_err(|error| error.to_string())?.join("folders.json"))
}

#[derive(Serialize)]
struct FoldersConfig {
    path: String,
    contents: String,
}

#[tauri::command]
fn read_folders_config(app: AppHandle) -> Result<FoldersConfig, String> {
    let path = folders_config_path(&app)?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(&path, DEFAULT_FOLDERS_CONFIG).map_err(|error| error.to_string())?;
    }
    let contents = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    Ok(FoldersConfig { path: path.to_string_lossy().into_owned(), contents })
}

#[tauri::command]
fn write_folders_config(app: AppHandle, contents: String) -> Result<(), String> {
    let path = folders_config_path(&app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&path, contents).map_err(|error| error.to_string())
}

#[tauri::command]
fn default_folders_config() -> &'static str {
    DEFAULT_FOLDERS_CONFIG
}

#[tauri::command]
fn existing_folders(paths: Vec<String>) -> Vec<bool> {
    paths.iter().map(|path| Path::new(path).is_dir()).collect()
}

// Whether every path lives on the same volume as dest_dir, so a drop there moves rather than copies.
// Windows answers this from the drive letter on the frontend instead.
#[tauri::command]
fn same_volume(paths: Vec<String>, dest_dir: String) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(dest) = fs::metadata(&dest_dir) else { return false };
        paths.iter().all(|path| fs::symlink_metadata(path).map(|metadata| metadata.dev() == dest.dev()).unwrap_or(false))
    }
    #[cfg(not(unix))]
    {
        let _ = (paths, dest_dir);
        true
    }
}

#[tauri::command]
fn edit_in_text_editor(paths: Vec<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        for path in &paths {
            std::process::Command::new("notepad.exe").arg(path).spawn().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        for path in &paths {
            std::process::Command::new("open").args(["-a", "TextEdit", path]).spawn().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = paths;
        Err("Editing in a text editor is not supported on this platform".to_string())
    }
}

// Zips the given files and folders into a new archive beside them, named after the first one,
// as Explorer's "Compress to ZIP file" does. Returns the archive's path.
#[tauri::command]
async fn compress_entries(paths: Vec<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || compress(&paths)).await.map_err(|error| error.to_string())?
}

fn compress(paths: &[String]) -> Result<String, String> {
    let first = Path::new(paths.first().ok_or("Nothing to compress")?);
    let dir = first.parent().ok_or("A drive can't be compressed")?;
    let stem = if first.is_dir() { first.file_name() } else { first.file_stem() };
    let stem = stem.map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| "Archive".to_string());
    let archive = unused_name(dir, &stem, ".zip");
    let result = write_zip(&archive, paths).map_err(|error| error.to_string());
    if result.is_err() {
        let _ = fs::remove_file(&archive);
    }
    result.map(|()| archive.to_string_lossy().to_string())
}

fn write_zip(archive: &Path, paths: &[String]) -> zip::result::ZipResult<()> {
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(fs::File::create(archive)?));
    for path in paths {
        let path = Path::new(path);
        let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
        add_to_zip(&mut zip, path, &name)?;
    }
    zip.finish()?;
    Ok(())
}

fn add_to_zip<W: std::io::Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, path: &Path, name: &str) -> zip::result::ZipResult<()> {
    let metadata = fs::metadata(path)?;
    // Zip timestamps are local time with no zone; anything outside 1980-2107 keeps the default.
    let mut options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    if let Some(modified) = metadata.modified().ok().and_then(|time| zip::DateTime::try_from(chrono::DateTime::<chrono::Local>::from(time).naive_local()).ok()) {
        options = options.last_modified_time(modified);
    }
    if metadata.is_dir() {
        zip.add_directory(format!("{name}/"), options)?;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            // Following a symlinked folder could loop forever, so those are left out.
            if entry.file_type()?.is_symlink() && entry.path().is_dir() {
                continue;
            }
            add_to_zip(zip, &entry.path(), &format!("{name}/{}", entry.file_name().to_string_lossy()))?;
        }
    } else {
        zip.start_file(name, options.large_file(metadata.len() >= u32::MAX as u64))?;
        std::io::copy(&mut fs::File::open(path)?, zip)?;
    }
    Ok(())
}

#[tauri::command]
fn show_properties(paths: Vec<String>) -> Result<(), String> {
    #[cfg(windows)]
    {
        win_shell::show_properties(&paths)
    }
    // Finder's Get Info window, one per item as Finder itself does. The paths go in as script
    // arguments rather than being spliced into the script, so no quoting is needed.
    #[cfg(target_os = "macos")]
    {
        let script = ["on run argv", "tell application \"Finder\"", "activate", "repeat with itemPath in argv", "open information window of (POSIX file (contents of itemPath) as alias)", "end repeat", "end tell", "end run"];
        let mut command = std::process::Command::new("osascript");
        for line in script { command.args(["-e", line]); }
        command.args(&paths).spawn().map_err(|error| error.to_string())?;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = paths;
        Err("The properties dialog is only available on Windows and macOS".to_string())
    }
}

#[derive(Serialize)]
struct OpenWithApp {
    id: String,
    name: String,
    icon: Option<String>,
}

// The apps the OS suggests for this file, as listed in Explorer's "Open with" submenu or Finder's
// "Open With" menu. An app's id is what open_with_app takes to launch it. Gathering the apps and
// their icons can take a few seconds on Windows, so that runs off the UI thread.
#[cfg(windows)]
#[tauri::command]
async fn list_open_with_apps(path: String) -> Vec<OpenWithApp> {
    tauri::async_runtime::spawn_blocking(move || win_shell::open_with_apps(&path)).await.unwrap_or_default()
}

// AppKit wants the main thread, which a synchronous command runs on.
#[cfg(not(windows))]
#[tauri::command]
fn list_open_with_apps(path: String) -> Vec<OpenWithApp> {
    #[cfg(target_os = "macos")]
    {
        mac_open_with::apps(&path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Vec::new()
    }
}

#[tauri::command]
fn open_with_app(path: String, app_id: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        win_shell::open_with(&path, &app_id)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").args(["-a", &app_id, &path]).spawn().map_err(|error| error.to_string())?;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (path, app_id);
        Err("Open With is only available on Windows and macOS".to_string())
    }
}

// Explorer's "Choose another app" picker; on macOS, a Choose Application dialog.
#[tauri::command]
fn open_with_other_app(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        win_shell::invoke_verb(&path, "openas")
    }
    #[cfg(target_os = "macos")]
    {
        let script = ["on run argv", "set chosenApp to choose application with prompt \"Choose an application to open the document:\" as alias", "tell application \"Finder\" to open (POSIX file (item 1 of argv) as alias) using chosenApp", "end run"];
        let mut command = std::process::Command::new("osascript");
        for line in script { command.args(["-e", line]); }
        command.arg(&path).spawn().map_err(|error| error.to_string())?;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        Err("Open With is only available on Windows and macOS".to_string())
    }
}

#[derive(Serialize)]
struct LockHunterInfo {
    icon: Option<String>,
}

// LockHunter (lockhunter.com) shows which processes hold a file or folder open. When it's installed,
// the context menu offers it; None means it isn't.
#[tauri::command]
fn lockhunter_info() -> Option<LockHunterInfo> {
    #[cfg(windows)]
    {
        let exe = win_shell::lockhunter_exe()?;
        Some(LockHunterInfo { icon: win_shell::exe_icon(&exe.to_string_lossy()) })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[tauri::command]
fn open_in_lockhunter(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe = win_shell::lockhunter_exe().ok_or("LockHunter is not installed")?;
        std::process::Command::new(exe).arg(&path).spawn().map_err(|error| error.to_string())?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("LockHunter is only available on Windows".to_string())
    }
}

// Lists the apps registered for the file's extension, with their icons.
#[cfg(target_os = "macos")]
mod mac_open_with {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    pub fn apps(path: &str) -> Vec<super::OpenWithApp> {
        let workspace = NSWorkspace::sharedWorkspace();
        let file_url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let default_app = workspace.URLForApplicationToOpenURL(&file_url).and_then(|url| url.path()).map(|path| path.to_string());
        let mut app_paths: Vec<String> = workspace.URLsForApplicationsToOpenURL(&file_url).iter().filter_map(|url| url.path()).map(|path| path.to_string()).collect();
        // The default app goes first, as in Finder.
        if let Some(default_app) = &default_app {
            app_paths.retain(|app| app != default_app);
            app_paths.insert(0, default_app.clone());
        }
        let file_manager = NSFileManager::defaultManager();
        app_paths
            .into_iter()
            .map(|app_path| {
                let ns_path = NSString::from_str(&app_path);
                let name = file_manager.displayNameAtPath(&ns_path).to_string();
                let name = name.strip_suffix(".app").map(str::to_string).unwrap_or(name);
                let icon = super::image_png(&workspace.iconForFile(&ns_path)).map(|bytes| format!("data:image/png;base64,{}", super::base64_encode(&bytes)));
                super::OpenWithApp { id: app_path, name, icon }
            })
            .collect()
    }
}

// Explorer's shell dialogs and handlers: Properties (a single item through the "properties" verb,
// several through SHMultiFileProperties, which shows the combined "N files" sheet Explorer does)
// and the "Open with" handlers registered for a file type.
#[cfg(windows)]
mod win_shell {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;

    const SEE_MASK_INVOKEIDLIST: u32 = 0x0000_000C;
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
    const SEE_MASK_NOASYNC: u32 = 0x0000_0100;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const SW_SHOW: i32 = 5;
    const ASSOC_FILTER_RECOMMENDED: u32 = 0x1;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        f_mask: u32,
        hwnd: isize,
        lp_verb: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show: i32,
        h_inst_app: isize,
        lp_id_list: *mut c_void,
        lp_class: *const u16,
        hkey_class: isize,
        dw_hot_key: u32,
        h_icon_or_monitor: isize,
        h_process: isize,
    }

    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);
    const IID_IDATAOBJECT: Guid = Guid(0x0000_010e, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
    const IID_ISHELLITEM: Guid = Guid(0x4382_6d1e, 0xe718, 0x42ee, [0xbc, 0x55, 0xa1, 0xe2, 0x61, 0xc3, 0x7b, 0xfe]);
    const BHID_DATA_OBJECT: Guid = Guid(0xb8c0_bd9f, 0xed24, 0x455c, [0x83, 0xe6, 0xd5, 0x39, 0x0c, 0x4f, 0xe8, 0xc4]);

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
        fn SHParseDisplayName(name: *const u16, bind_ctx: *mut c_void, pidl: *mut *mut c_void, sfgao_in: u32, sfgao_out: *mut u32) -> i32;
        fn SHCreateDataObject(pidl_folder: *const c_void, count: u32, pidls: *const *mut c_void, inner: *mut c_void, riid: *const Guid, out: *mut *mut c_void) -> i32;
        fn SHMultiFileProperties(data_object: *mut c_void, flags: u32) -> i32;
        fn SHAssocEnumHandlers(extension: *const u16, filter: u32, enumerator: *mut *mut c_void) -> i32;
        fn SHCreateItemFromParsingName(path: *const u16, bind_ctx: *mut c_void, riid: *const Guid, item: *mut *mut c_void) -> i32;
        fn ExtractIconExW(file: *const u16, index: i32, large: *mut isize, small: *mut isize, count: u32) -> u32;
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(ptr: *mut c_void);
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    }
    extern "system" {
        fn DestroyIcon(icon: isize) -> i32;
        fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: isize, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    pub fn show_properties(paths: &[String]) -> Result<(), String> {
        match paths {
            [] => Ok(()),
            [path] => invoke_verb(path, "properties"),
            _ => show_multiple(paths),
        }
    }

    pub fn invoke_verb(path: &str, verb: &str) -> Result<(), String> {
        let verb = wide(verb);
        let file = wide(path);
        let mut info = ShellExecuteInfoW {
            cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
            f_mask: SEE_MASK_INVOKEIDLIST,
            hwnd: 0,
            lp_verb: verb.as_ptr(),
            lp_file: file.as_ptr(),
            lp_parameters: std::ptr::null(),
            lp_directory: std::ptr::null(),
            n_show: SW_SHOW,
            h_inst_app: 0,
            lp_id_list: std::ptr::null_mut(),
            lp_class: std::ptr::null(),
            hkey_class: 0,
            dw_hot_key: 0,
            h_icon_or_monitor: 0,
            h_process: 0,
        };
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    // Relaunches this program elevated to do one job, waiting for it to finish.
    // Ok(false) means the user declined the UAC prompt.
    pub fn run_self_elevated(flag: &str, paths: &[&str], owner_window: isize) -> Result<bool, String> {
        const ERROR_CANCELLED: i32 = 1223;
        unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED) };
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let verb = wide("runas");
        let file = wide(&exe.to_string_lossy());
        // Windows names can't contain a quote, and these paths all end in a name rather than a backslash.
        let quoted: Vec<String> = paths.iter().map(|path| format!("\"{path}\"")).collect();
        let parameters = wide(&format!("{flag} {}", quoted.join(" ")));
        let mut info = ShellExecuteInfoW {
            cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
            f_mask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            hwnd: owner_window,
            lp_verb: verb.as_ptr(),
            lp_file: file.as_ptr(),
            lp_parameters: parameters.as_ptr(),
            lp_directory: std::ptr::null(),
            n_show: SW_SHOW,
            h_inst_app: 0,
            lp_id_list: std::ptr::null_mut(),
            lp_class: std::ptr::null(),
            hkey_class: 0,
            dw_hot_key: 0,
            h_icon_or_monitor: 0,
            h_process: 0,
        };
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_CANCELLED) { Ok(false) } else { Err(error.to_string()) };
        }
        if info.h_process == 0 { return Err("Windows didn't start the administrator process.".to_string()); }
        let mut exit_code = 1u32;
        let read_exit_code = unsafe {
            WaitForSingleObject(info.h_process, INFINITE);
            let read = GetExitCodeProcess(info.h_process, &mut exit_code);
            CloseHandle(info.h_process);
            read
        };
        if read_exit_code == 0 { return Err(std::io::Error::last_os_error().to_string()); }
        if exit_code != 0 { return Err(std::io::Error::from_raw_os_error(exit_code as i32).to_string()); }
        Ok(true)
    }

    fn show_multiple(paths: &[String]) -> Result<(), String> {
        with_data_object(paths, |data_object| {
            let hr = unsafe { SHMultiFileProperties(data_object, 0) };
            if hr < 0 {
                return Err(format!("SHMultiFileProperties failed (HRESULT {hr:#x})"));
            }
            Ok(())
        })
    }

    // Runs `use_object` with a shell data object (IDataObject) holding the given paths.
    fn with_data_object(paths: &[String], use_object: impl FnOnce(*mut c_void) -> Result<(), String>) -> Result<(), String> {
        let mut pidls: Vec<*mut c_void> = Vec::with_capacity(paths.len());
        let result = (|| {
            for path in paths {
                let mut pidl = std::ptr::null_mut();
                let hr = unsafe { SHParseDisplayName(wide(path).as_ptr(), std::ptr::null_mut(), &mut pidl, 0, std::ptr::null_mut()) };
                if hr < 0 {
                    return Err(format!("Couldn't resolve {path} (HRESULT {hr:#x})"));
                }
                pidls.push(pidl);
            }
            // With no parent folder, the absolute item IDs are taken relative to the desktop.
            let mut data_object = std::ptr::null_mut();
            let hr = unsafe { SHCreateDataObject(std::ptr::null(), pidls.len() as u32, pidls.as_ptr(), std::ptr::null_mut(), &IID_IDATAOBJECT, &mut data_object) };
            if hr < 0 {
                return Err(format!("SHCreateDataObject failed (HRESULT {hr:#x})"));
            }
            let result = use_object(data_object);
            unsafe { release(data_object) };
            result
        })();
        for pidl in pidls {
            unsafe { CoTaskMemFree(pidl) };
        }
        result
    }

    // The data object a file's IShellItem hands out, the form IAssocHandler::Invoke expects.
    fn with_item_data_object(path: &str, use_object: impl FnOnce(*mut c_void) -> Result<(), String>) -> Result<(), String> {
        let mut item = std::ptr::null_mut();
        let hr = unsafe { SHCreateItemFromParsingName(wide(path).as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEM, &mut item) };
        if hr < 0 {
            return Err(format!("Couldn't resolve {path} (HRESULT {hr:#x})"));
        }
        // IShellItem::BindToHandler
        let bind: extern "system" fn(*mut c_void, *mut c_void, *const Guid, *const Guid, *mut *mut c_void) -> i32 = unsafe { std::mem::transmute(method(item, 3)) };
        let mut data_object = std::ptr::null_mut();
        let hr = bind(item, std::ptr::null_mut(), &BHID_DATA_OBJECT, &IID_IDATAOBJECT, &mut data_object);
        unsafe { release(item) };
        if hr < 0 {
            return Err(format!("Couldn't get a data object for {path} (HRESULT {hr:#x})"));
        }
        let result = use_object(data_object);
        unsafe { release(data_object) };
        result
    }

    // The address in a COM object's vtable slot, to be transmuted to that method's signature.
    unsafe fn method(object: *mut c_void, slot: usize) -> usize {
        *(*(object as *const *const usize)).add(slot)
    }

    // IUnknown::Release is the third vtable slot of every COM interface.
    unsafe fn release(object: *mut c_void) {
        let release: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(method(object, 2));
        release(object);
    }

    // Calls an IAssocHandler method that hands back a CoTaskMemAlloc'd string (GetName, GetUIName).
    unsafe fn handler_string(handler: *mut c_void, slot: usize) -> Option<String> {
        let get: extern "system" fn(*mut c_void, *mut *mut u16) -> i32 = std::mem::transmute(method(handler, slot));
        let mut value: *mut u16 = std::ptr::null_mut();
        if get(handler, &mut value) < 0 || value.is_null() {
            return None;
        }
        Some(take_co_string(value))
    }

    unsafe fn take_co_string(value: *mut u16) -> String {
        let len = (0..).take_while(|&index| *value.add(index) != 0).count();
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(value, len));
        CoTaskMemFree(value as *mut c_void);
        text
    }

    // IAssocHandler::GetIconLocation, drawn as a PNG data URI. Store apps report an
    // indirect resource string rather than a file, and get no icon.
    unsafe fn handler_icon(handler: *mut c_void) -> Option<String> {
        let get: extern "system" fn(*mut c_void, *mut *mut u16, *mut i32) -> i32 = std::mem::transmute(method(handler, 5));
        let mut location: *mut u16 = std::ptr::null_mut();
        let mut index = 0;
        if get(handler, &mut location, &mut index) < 0 || location.is_null() {
            return None;
        }
        icon_data_uri(&take_co_string(location), index)
    }

    // The small icon at `index` in an exe or dll, as a PNG data URI.
    fn icon_data_uri(location: &str, index: i32) -> Option<String> {
        let mut icon = 0;
        unsafe {
            if ExtractIconExW(wide(location).as_ptr(), index, std::ptr::null_mut(), &mut icon, 1) == 0 || icon == 0 {
                return None;
            }
            let png = super::icon_rgba(icon).and_then(|(width, height, rgba)| super::rgba_png(width, height, rgba));
            DestroyIcon(icon);
            png.map(|bytes| format!("data:image/png;base64,{}", super::base64_encode(&bytes)))
        }
    }

    pub fn exe_icon(path: &str) -> Option<String> {
        icon_data_uri(path, 0)
    }

    // Where LockHunter's installer recorded it, falling back to its default install folders.
    pub fn lockhunter_exe() -> Option<std::path::PathBuf> {
        let mut folders: Vec<std::path::PathBuf> = Vec::new();
        for key in [
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\LockHunter_is1",
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\LockHunter_is1",
            r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\LockHunter_is1",
        ] {
            if let Some(location) = registry_string(key, "InstallLocation") {
                folders.push(location.into());
            }
        }
        for variable in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(variable) {
                folders.push(std::path::PathBuf::from(dir).join("LockHunter"));
            }
        }
        folders.into_iter().map(|folder| folder.join("LockHunter.exe")).find(|exe| exe.is_file())
    }

    fn registry_string(key: &str, value: &str) -> Option<String> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = std::process::Command::new("reg").args(["query", key, "/v", value]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
        if !output.status.success() {
            return None;
        }
        // A matching line reads "    InstallLocation    REG_SZ    C:\Program Files\LockHunter\".
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix(value)?.trim_start().strip_prefix("REG_SZ").map(|rest| rest.trim().to_string()))
            .filter(|location| !location.is_empty())
    }

    // Walks the recommended IAssocHandlers for the file's extension, stopping once `visit` returns false.
    fn for_each_handler(path: &str, mut visit: impl FnMut(*mut c_void) -> bool) {
        let Some(extension) = std::path::Path::new(path).extension() else { return };
        let extension = format!(".{}", extension.to_string_lossy());
        let mut enumerator = std::ptr::null_mut();
        if unsafe { SHAssocEnumHandlers(wide(&extension).as_ptr(), ASSOC_FILTER_RECOMMENDED, &mut enumerator) } < 0 || enumerator.is_null() {
            return;
        }
        unsafe {
            // IEnumAssocHandlers::Next
            let next: extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut u32) -> i32 = std::mem::transmute(method(enumerator, 3));
            loop {
                let mut handler = std::ptr::null_mut();
                let mut fetched = 0;
                if next(enumerator, 1, &mut handler, &mut fetched) != 0 || fetched == 0 || handler.is_null() {
                    break;
                }
                let keep_going = visit(handler);
                release(handler);
                if !keep_going {
                    break;
                }
            }
            release(enumerator);
        }
    }

    // Called from a worker thread, which needs COM set up before the shell will talk to it.
    pub fn open_with_apps(path: &str) -> Vec<super::OpenWithApp> {
        unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED) };
        let mut apps = Vec::new();
        for_each_handler(path, |handler| {
            // IAssocHandler::GetName (slot 3) is the app's path or id; GetUIName (slot 4) its display name.
            if let (Some(id), Some(name)) = unsafe { (handler_string(handler, 3), handler_string(handler, 4)) } {
                let icon = unsafe { handler_icon(handler) };
                apps.push(super::OpenWithApp { id, name, icon });
            }
            true
        });
        apps
    }

    pub fn open_with(path: &str, app_id: &str) -> Result<(), String> {
        let mut result = Err("That app is no longer registered for this file type".to_string());
        for_each_handler(path, |handler| {
            if unsafe { handler_string(handler, 3) }.as_deref() != Some(app_id) {
                return true;
            }
            result = with_item_data_object(path, |data_object| {
                // IAssocHandler::Invoke
                let invoke: extern "system" fn(*mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(method(handler, 8)) };
                let hr = invoke(handler, data_object);
                if hr < 0 {
                    return Err(format!("Couldn't start the app (HRESULT {hr:#x})"));
                }
                Ok(())
            });
            false
        });
        result
    }
}

// Copies the OS clipboard's file-list format (CF_HDROP) plus the "Preferred DropEffect" format
// that Explorer uses to mark a copy vs. a cut, so cutting/copying here is interchangeable with
// Explorer: paths copied in Rove Explorer can be pasted in Explorer and vice versa.
#[cfg(windows)]
mod win_clipboard {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Mutex;

    const CF_HDROP: u32 = 15;
    const GMEM_MOVEABLE: u32 = 0x0002;
    const GMEM_ZEROINIT: u32 = 0x0040;
    static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        pt_x: i32,
        pt_y: i32,
        f_nc: i32,
        f_wide: i32,
    }

    extern "system" {
        fn OpenClipboard(owner: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> isize;
        fn SetClipboardData(format: u32, data: isize) -> isize;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(mem: isize) -> *mut u8;
        fn GlobalUnlock(mem: isize) -> i32;
        fn GlobalFree(mem: isize) -> isize;
        fn DragQueryFileW(hdrop: isize, index: u32, buffer: *mut u16, buffer_len: u32) -> u32;
    }

    fn drop_effect_format() -> u32 {
        let name: Vec<u16> = OsStr::new("Preferred DropEffect").encode_wide().chain(Some(0)).collect();
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    pub fn write_paths(paths: &[String], cut: bool) -> Result<(), String> {
        let _guard = CLIPBOARD_LOCK.lock().map_err(|_| "clipboard lock poisoned".to_string())?;
        unsafe {
            if OpenClipboard(0) == 0 {
                return Err("Could not open the clipboard".to_string());
            }
            if EmptyClipboard() == 0 {
                CloseClipboard();
                return Err("Could not clear the clipboard".to_string());
            }

            let mut wide_list: Vec<u16> = Vec::new();
            for path in paths {
                wide_list.extend(OsStr::new(path).encode_wide());
                wide_list.push(0);
            }
            wide_list.push(0);
            let header_size = std::mem::size_of::<DropFiles>();
            let data_bytes = wide_list.len() * 2;
            let hmem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, header_size + data_bytes);
            if hmem == 0 {
                CloseClipboard();
                return Err("Out of memory copying paths".to_string());
            }
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                GlobalFree(hmem);
                CloseClipboard();
                return Err("Could not lock clipboard memory".to_string());
            }
            let header = DropFiles { p_files: header_size as u32, pt_x: 0, pt_y: 0, f_nc: 0, f_wide: 1 };
            std::ptr::copy_nonoverlapping(&header as *const DropFiles as *const u8, ptr, header_size);
            std::ptr::copy_nonoverlapping(wide_list.as_ptr() as *const u8, ptr.add(header_size), data_bytes);
            GlobalUnlock(hmem);
            if SetClipboardData(CF_HDROP, hmem) == 0 {
                GlobalFree(hmem);
                CloseClipboard();
                return Err("Could not set clipboard data".to_string());
            }

            let format_id = drop_effect_format();
            if format_id != 0 {
                let hmem_effect = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, 4);
                if hmem_effect != 0 {
                    let ptr_effect = GlobalLock(hmem_effect);
                    if !ptr_effect.is_null() {
                        let effect: u32 = if cut { 2 } else { 1 };
                        std::ptr::copy_nonoverlapping(&effect as *const u32 as *const u8, ptr_effect, 4);
                        GlobalUnlock(hmem_effect);
                        if SetClipboardData(format_id, hmem_effect) == 0 {
                            GlobalFree(hmem_effect);
                        }
                    } else {
                        GlobalFree(hmem_effect);
                    }
                }
            }

            CloseClipboard();
        }
        Ok(())
    }

    pub fn read_paths() -> Result<Option<(Vec<String>, bool)>, String> {
        let _guard = CLIPBOARD_LOCK.lock().map_err(|_| "clipboard lock poisoned".to_string())?;
        unsafe {
            if OpenClipboard(0) == 0 {
                return Err("Could not open the clipboard".to_string());
            }
            if IsClipboardFormatAvailable(CF_HDROP) == 0 {
                CloseClipboard();
                return Ok(None);
            }
            let hdrop = GetClipboardData(CF_HDROP);
            if hdrop == 0 {
                CloseClipboard();
                return Ok(None);
            }
            let count = DragQueryFileW(hdrop, 0xFFFFFFFF, std::ptr::null_mut(), 0);
            let mut paths = Vec::with_capacity(count as usize);
            for index in 0..count {
                let needed = DragQueryFileW(hdrop, index, std::ptr::null_mut(), 0);
                let mut buffer = vec![0u16; (needed + 1) as usize];
                let written = DragQueryFileW(hdrop, index, buffer.as_mut_ptr(), buffer.len() as u32);
                buffer.truncate(written as usize);
                paths.push(String::from_utf16_lossy(&buffer));
            }

            let mut cut = false;
            let format_id = drop_effect_format();
            if format_id != 0 && IsClipboardFormatAvailable(format_id) != 0 {
                let hmem_effect = GetClipboardData(format_id);
                if hmem_effect != 0 {
                    let ptr_effect = GlobalLock(hmem_effect);
                    if !ptr_effect.is_null() {
                        let mut effect: u32 = 0;
                        std::ptr::copy_nonoverlapping(ptr_effect, &mut effect as *mut u32 as *mut u8, 4);
                        GlobalUnlock(hmem_effect);
                        cut = effect == 2;
                    }
                }
            }

            CloseClipboard();
            Ok(Some((paths, cut)))
        }
    }
}

#[cfg(all(test, windows))]
mod win_clipboard_tests {
    use super::win_clipboard;

    #[test]
    fn roundtrips_our_own_write() {
        let paths = vec!["C:\\Windows\\notepad.exe".to_string(), "C:\\Windows\\explorer.exe".to_string()];
        win_clipboard::write_paths(&paths, true).expect("write_paths failed");
        let (read_back, cut) = win_clipboard::read_paths().expect("read_paths failed").expect("expected clipboard files");
        assert_eq!(read_back, paths);
        assert!(cut);

        win_clipboard::write_paths(&paths, false).expect("write_paths failed");
        let (_, cut) = win_clipboard::read_paths().expect("read_paths failed").expect("expected clipboard files");
        assert!(!cut);
    }
}

#[cfg(windows)]
#[tauri::command]
fn clipboard_write_paths(paths: Vec<String>, cut: bool) -> Result<(), String> {
    win_clipboard::write_paths(&paths, cut)
}

#[cfg(windows)]
#[tauri::command]
fn clipboard_read_paths() -> Result<Option<ClipboardFiles>, String> {
    Ok(win_clipboard::read_paths()?.map(|(paths, cut)| ClipboardFiles { paths, cut }))
}

// Puts file URLs on the general pasteboard, the way Finder's Copy does, so items copied here paste
// in Finder and vice versa. Finder has no Cut (it moves on paste with ⌥⌘V instead), so whether our
// own last write was a cut is remembered alongside the pasteboard's change count: once anything
// else writes to the pasteboard the count moves on and the pasted items are treated as a copy.
#[cfg(target_os = "macos")]
mod mac_clipboard {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::ClassType;
    use objc2_app_kit::{NSPasteboard, NSPasteboardURLReadingFileURLsOnlyKey, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
    use std::sync::Mutex;

    static CUT_CHANGE_COUNT: Mutex<Option<isize>> = Mutex::new(None);

    pub fn write_paths(paths: &[String], cut: bool) -> Result<(), String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
            .iter()
            .map(|path| ProtocolObject::from_retained(NSURL::fileURLWithPath(&NSString::from_str(path))))
            .collect();
        pasteboard.clearContents();
        if !pasteboard.writeObjects(&NSArray::from_retained_slice(&urls)) {
            return Err("Couldn't write to the clipboard".to_string());
        }
        *CUT_CHANGE_COUNT.lock().map_err(|_| "clipboard lock poisoned".to_string())? = cut.then(|| pasteboard.changeCount());
        Ok(())
    }

    pub fn read_paths() -> Result<Option<(Vec<String>, bool)>, String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let classes = NSArray::from_slice(&[NSURL::class()]);
        let file_urls_only = NSNumber::new_bool(true);
        let value: &AnyObject = &file_urls_only;
        let options = NSDictionary::from_slices(&[unsafe { NSPasteboardURLReadingFileURLsOnlyKey }], &[value]);
        let Some(objects) = (unsafe { pasteboard.readObjectsForClasses_options(&classes, Some(&options)) }) else { return Ok(None) };
        let paths: Vec<String> = objects
            .iter()
            .filter_map(|object| object.downcast_ref::<NSURL>().and_then(|url| url.path()).map(|path| path.to_string()))
            .collect();
        if paths.is_empty() { return Ok(None); }
        let cut = *CUT_CHANGE_COUNT.lock().map_err(|_| "clipboard lock poisoned".to_string())? == Some(pasteboard.changeCount());
        Ok(Some((paths, cut)))
    }
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn clipboard_write_paths(paths: Vec<String>, cut: bool) -> Result<(), String> {
    mac_clipboard::write_paths(&paths, cut)
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn clipboard_read_paths() -> Result<Option<ClipboardFiles>, String> {
    Ok(mac_clipboard::read_paths()?.map(|(paths, cut)| ClipboardFiles { paths, cut }))
}

#[cfg(not(any(windows, target_os = "macos")))]
#[tauri::command]
fn clipboard_write_paths(state: State<ClipboardState>, paths: Vec<String>, cut: bool) -> Result<(), String> {
    *state.files.lock().map_err(|_| "clipboard lock poisoned".to_string())? = Some((paths, cut));
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
#[tauri::command]
fn clipboard_read_paths(state: State<ClipboardState>) -> Result<Option<ClipboardFiles>, String> {
    let files = state.files.lock().map_err(|_| "clipboard lock poisoned".to_string())?;
    Ok(files.clone().map(|(paths, cut)| ClipboardFiles { paths, cut }))
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// Mirrors Explorer's conflict handling: never overwrite silently, instead find the next free
// "name - Copy" / "name - Copy (n)" name, which is also what a copy-paste into the same folder
// needs since the source itself already occupies the plain name.
fn unique_destination(dest_dir: &Path, name: &str) -> std::path::PathBuf {
    let candidate = dest_dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(name).file_stem().map(|value| value.to_string_lossy().to_string()).unwrap_or_else(|| name.to_string());
    let extension = Path::new(name).extension().map(|value| format!(".{}", value.to_string_lossy())).unwrap_or_default();
    let mut attempt = 1u32;
    loop {
        let candidate_name = if attempt == 1 { format!("{stem} - Copy{extension}") } else { format!("{stem} - Copy ({attempt}){extension}") };
        let candidate = dest_dir.join(&candidate_name);
        if !candidate.exists() {
            return candidate;
        }
        attempt += 1;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ConflictResolution {
    Replace,
    Skip,
}

// When a pasted folder merges into an existing same-named folder, only files that already exist
// at the corresponding destination path are a real conflict - everything else just lands
// alongside the existing content, the same way Explorer merges two folders.
fn plan_merge_conflicts(source: &Path, dest: &Path, conflicts: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(source) else { return };
    for entry in entries.filter_map(Result::ok) {
        let entry_path = entry.path();
        let target = dest.join(entry.file_name());
        let is_dir = entry.file_type().map(|value| value.is_dir()).unwrap_or(false);
        if is_dir {
            if target.is_dir() {
                plan_merge_conflicts(&entry_path, &target, conflicts);
            }
        } else if target.exists() {
            conflicts.push(target);
        }
    }
}

#[tauri::command]
fn scan_paste_conflicts(paths: Vec<String>, dest_dir: String) -> Result<Vec<String>, String> {
    let dest = Path::new(&dest_dir);
    let mut conflicts = Vec::new();
    for source_path in &paths {
        let source = Path::new(source_path);
        let Some(name) = source.file_name() else { continue };
        let target = dest.join(name);
        if source.is_dir() && target.is_dir() {
            plan_merge_conflicts(source, &target, &mut conflicts);
        }
    }
    Ok(conflicts.into_iter().map(|path| path.to_string_lossy().to_string()).collect())
}

// Merges `source`'s contents into the already-existing `dest` folder. A conflicting file is
// replaced or left alone per `resolution`; for a cut, a file is only removed from `source` once
// it has actually landed in `dest`, so a skipped file simply stays behind instead of being lost,
// and a folder is only pruned once it has been fully drained by removing an (expectedly) empty dir.
fn merge_dir_recursive(source: &Path, dest: &Path, cut: bool, resolution: ConflictResolution) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let entry_path = entry.path();
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            merge_dir_recursive(&entry_path, &target, cut, resolution)?;
            if cut {
                let _ = fs::remove_dir(&entry_path);
            }
        } else if target.exists() {
            if resolution == ConflictResolution::Replace {
                fs::copy(&entry_path, &target)?;
                if cut {
                    fs::remove_file(&entry_path)?;
                }
            }
        } else {
            fs::copy(&entry_path, &target)?;
            if cut {
                fs::remove_file(&entry_path)?;
            }
        }
    }
    Ok(())
}

fn is_same_or_inside(path: &Path, ancestor: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(ancestor)) {
        (Ok(path), Ok(ancestor)) => path.starts_with(ancestor),
        _ => false,
    }
}

// Checked before starting so a paste doesn't get partway and then have to be redone as administrator.
fn paste_needs_elevation(paths: &[String], dest: &Path, cut: bool) -> bool {
    paths.iter().map(Path::new).any(|source| {
        if (cut && source.parent() == Some(dest)) || is_same_or_inside(dest, source) { return false; }
        let add = if source.is_dir() { FILE_ADD_SUBDIRECTORY } else { FILE_ADD_FILE };
        access_denied(dest, add) || (cut && access_denied(source, DELETE))
    })
}

#[tauri::command]
fn paste_entries(paths: Vec<String>, dest_dir: String, cut: bool, conflict_resolution: Option<String>) -> Result<Vec<String>, String> {
    let dest = Path::new(&dest_dir);
    if paste_needs_elevation(&paths, dest, cut) { return Err(NEEDS_ELEVATION.to_string()); }
    paste_items(&paths, dest, cut, conflict_resolution.as_deref()).map_err(|error| error.to_string())
}

fn paste_items(paths: &[String], dest: &Path, cut: bool, conflict_resolution: Option<&str>) -> std::io::Result<Vec<String>> {
    let resolution = if conflict_resolution == Some("replace") { ConflictResolution::Replace } else { ConflictResolution::Skip };
    let mut results = Vec::with_capacity(paths.len());
    for source_path in paths {
        let source = Path::new(source_path);
        let Some(name) = source.file_name().map(|value| value.to_string_lossy().to_string()) else { continue };
        if cut && source.parent() == Some(dest) {
            // Cutting and pasting back into the same folder is a no-op, like Explorer.
            results.push(source_path.clone());
            continue;
        }
        if is_same_or_inside(dest, source) {
            // A folder can't go into itself or one of its own subfolders (e.g. dropped onto its own row).
            continue;
        }
        let existing_target = dest.join(&name);
        if source.is_dir() && existing_target.is_dir() {
            // A same-named folder already lives here: merge into it instead of renaming a copy alongside it.
            merge_dir_recursive(source, &existing_target, cut, resolution)?;
            if cut {
                let _ = fs::remove_dir(source);
            }
            results.push(existing_target.to_string_lossy().to_string());
            continue;
        }
        let target = unique_destination(dest, &name);
        if cut {
            if fs::rename(source, &target).is_err() {
                if source.is_dir() {
                    copy_dir_recursive(source, &target)?;
                    fs::remove_dir_all(source)?;
                } else {
                    fs::copy(source, &target)?;
                    fs::remove_file(source)?;
                }
            }
        } else if source.is_dir() {
            copy_dir_recursive(source, &target)?;
        } else {
            fs::copy(source, &target)?;
        }
        results.push(target.to_string_lossy().to_string());
    }
    Ok(results)
}

#[cfg(test)]
mod paste_entries_tests {
    use super::*;

    struct Sandbox {
        dir: std::path::PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rove-explorer-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Sandbox { dir }
        }
        fn path(&self, name: &str) -> std::path::PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(windows)]
    #[test]
    fn delete_entry_reports_files_in_use() {
        use std::os::windows::fs::OpenOptionsExt;
        let sandbox = Sandbox::new("delete-in-use");
        let folder = sandbox.path("pix");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("photo.jpg"), b"x").unwrap();
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(folder.join("photo.jpg")).unwrap();
        let error = delete_entry(folder.to_string_lossy().to_string()).unwrap_err();
        drop(lock);
        assert!(folder.exists());
        assert!(error.contains("being used by another program") && error.contains("photo.jpg"), "{error}");
    }

    #[test]
    fn deleted_entries_can_be_restored() {
        let sandbox = Sandbox::new("delete-restore");
        fs::write(sandbox.path("note.txt"), b"keep me").unwrap();
        fs::create_dir_all(sandbox.path("pix")).unwrap();
        fs::write(sandbox.path("pix").join("photo.jpg"), b"x").unwrap();
        fs::create_dir_all(sandbox.path("pix").join("trip")).unwrap();
        fs::write(sandbox.path("pix").join("trip").join("beach.jpg"), b"x").unwrap();
        let paths = ["note.txt", "pix"].map(|name| sandbox.path(name).to_string_lossy().to_string()).to_vec();
        let outcome = delete_entries(paths);
        assert!(outcome.failures.is_empty());
        assert!(!sandbox.path("note.txt").exists() && !sandbox.path("pix").exists());
        assert!(outcome.trashed.iter().all(|entry| !entry.items.is_empty()));
        let stored: Vec<_> = outcome.trashed.iter().flat_map(|entry| entry.items.iter().map(|item| PathBuf::from(&item.id))).collect();
        restore_entries(outcome.trashed).unwrap();
        assert!(stored.iter().all(|path| !path.exists()));
        #[cfg(not(target_os = "macos"))]
        assert!(!trash::os_limited::list().unwrap().iter().any(|item| item.original_parent.starts_with(&sandbox.dir)));
        assert_eq!(fs::read_to_string(sandbox.path("note.txt")).unwrap(), "keep me");
        assert!(sandbox.path("pix").join("photo.jpg").exists());
        assert!(sandbox.path("pix").join("trip").join("beach.jpg").exists());
    }

    #[test]
    fn restore_skips_items_whose_name_is_taken() {
        let sandbox = Sandbox::new("restore-taken");
        fs::write(sandbox.path("a.txt"), b"old").unwrap();
        fs::write(sandbox.path("b.txt"), b"b").unwrap();
        let outcome = delete_entries(vec![sandbox.path("a.txt").to_string_lossy().to_string(), sandbox.path("b.txt").to_string_lossy().to_string()]);
        fs::write(sandbox.path("a.txt"), b"new").unwrap();
        let error = restore_entries(outcome.trashed).unwrap_err();
        assert!(error.contains("a.txt") && error.contains("already there"), "{error}");
        assert_eq!(fs::read_to_string(sandbox.path("a.txt")).unwrap(), "new");
        assert!(sandbox.path("b.txt").exists());
    }

    #[test]
    fn rename_entry_refuses_to_overwrite_existing_file() {
        let sandbox = Sandbox::new("rename-existing");
        fs::write(sandbox.path("a.txt"), b"source").unwrap();
        fs::write(sandbox.path("b.txt"), b"keep me").unwrap();
        let error = rename_entry(sandbox.path("a.txt").to_string_lossy().to_string(), "b.txt".to_string()).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(fs::read_to_string(sandbox.path("a.txt")).unwrap(), "source");
        assert_eq!(fs::read_to_string(sandbox.path("b.txt")).unwrap(), "keep me");
    }

    #[test]
    fn rename_entry_refuses_to_overwrite_existing_folder() {
        let sandbox = Sandbox::new("rename-existing-folder");
        fs::create_dir_all(sandbox.path("one")).unwrap();
        fs::create_dir_all(sandbox.path("two")).unwrap();
        fs::write(sandbox.path("two").join("inside.txt"), b"keep me").unwrap();
        let error = rename_entry(sandbox.path("one").to_string_lossy().to_string(), "two".to_string()).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert!(sandbox.path("one").is_dir());
        assert_eq!(fs::read_to_string(sandbox.path("two").join("inside.txt")).unwrap(), "keep me");
    }

    #[test]
    fn rename_entry_allows_case_only_change() {
        let sandbox = Sandbox::new("rename-case");
        fs::write(sandbox.path("photo.jpg"), b"x").unwrap();
        rename_entry(sandbox.path("photo.jpg").to_string_lossy().to_string(), "Photo.jpg".to_string()).unwrap();
        let names: Vec<String> = fs::read_dir(&sandbox.dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["Photo.jpg".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn rename_entry_reports_file_in_use() {
        use std::os::windows::fs::OpenOptionsExt;
        let sandbox = Sandbox::new("rename-file-in-use");
        let file = sandbox.path("report.txt");
        fs::write(&file, b"x").unwrap();
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
        let error = rename_entry(file.to_string_lossy().to_string(), "renamed.txt".to_string()).unwrap_err();
        drop(lock);
        assert!(file.exists());
        assert!(error.contains("being used by another program"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn rename_entry_reports_folder_in_use() {
        use std::os::windows::fs::OpenOptionsExt;
        let sandbox = Sandbox::new("rename-folder-in-use");
        let folder = sandbox.path("pix");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("photo.jpg"), b"x").unwrap();
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(folder.join("photo.jpg")).unwrap();
        let error = rename_entry(folder.to_string_lossy().to_string(), "renamed".to_string()).unwrap_err();
        drop(lock);
        assert!(folder.exists());
        assert!(error.contains("being used by another program"), "{error}");
    }

    #[test]
    fn create_file_uses_default_name_when_folder_is_empty() {
        let sandbox = Sandbox::new("create-file-empty");
        let path = create_file(sandbox.dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(path, sandbox.path("New File.txt").to_string_lossy().to_string());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn create_file_numbers_subsequent_files() {
        let sandbox = Sandbox::new("create-file-numbered");
        fs::write(sandbox.path("New File.txt"), b"existing").unwrap();
        let path = create_file(sandbox.dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(path, sandbox.path("New File 2.txt").to_string_lossy().to_string());
        fs::write(sandbox.path("New File 2.txt"), b"existing too").unwrap();
        let path = create_file(sandbox.dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(path, sandbox.path("New File 3.txt").to_string_lossy().to_string());
    }

    #[test]
    fn create_folder_numbers_subsequent_folders() {
        let sandbox = Sandbox::new("create-folder-numbered");
        let path = create_folder(sandbox.dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(path, sandbox.path("New Folder").to_string_lossy().to_string());
        assert!(Path::new(&path).is_dir());
        let path = create_folder(sandbox.dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(path, sandbox.path("New Folder 2").to_string_lossy().to_string());
    }

    #[cfg(windows)]
    fn helper(flag: &str, paths: &[&Path]) -> Option<i32> {
        let args = std::iter::once(std::ffi::OsString::from(flag)).chain(paths.iter().map(|path| path.as_os_str().to_owned()));
        elevated_helper_exit_code(args.collect::<Vec<_>>().into_iter())
    }

    #[cfg(windows)]
    #[test]
    fn elevated_helper_does_only_the_requested_job() {
        let sandbox = Sandbox::new("elevated-helper");
        let folder = sandbox.path("Name with % & (x86)");
        assert_eq!(helper(ELEVATED_CREATE_FOLDER, &[&folder]), Some(0));
        assert!(folder.is_dir());
        assert_eq!(helper(ELEVATED_CREATE_FOLDER, &[&folder]), Some(183), "already exists");
        let file = folder.join("note.txt");
        assert_eq!(helper(ELEVATED_CREATE_FILE, &[&file]), Some(0));
        assert!(file.is_file());
        assert_eq!(helper(ELEVATED_CREATE_FILE, &[&file]), Some(80), "already exists, and isn't truncated");
        let renamed = folder.join("renamed.txt");
        assert_eq!(helper(ELEVATED_RENAME, &[&file, &renamed]), Some(0));
        assert!(renamed.is_file() && !file.exists());
        assert_eq!(elevated_helper_exit_code(std::iter::empty()), None, "normal launch");
        assert_eq!(helper("C:\\some\\file", &[]), None, "opened with another argument");
        assert_eq!(helper(ELEVATED_CREATE_FOLDER, &[]), Some(87), "no path");
        assert_eq!(helper(ELEVATED_RENAME, &[&file]), Some(87), "no target");
    }

    #[cfg(windows)]
    #[test]
    fn elevated_paste_reads_the_job_while_it_stays_locked() {
        let sandbox = Sandbox::new("elevated-paste");
        fs::write(sandbox.path("a.txt"), "hello").unwrap();
        fs::create_dir_all(sandbox.path("dest")).unwrap();
        let job = PasteJob { paths: vec![sandbox.path("a.txt").to_string_lossy().to_string()], dest_dir: sandbox.path("dest").to_string_lossy().to_string(), cut: true, conflict_resolution: None };
        let (file, job_path) = create_job_file(&job).unwrap();
        assert!(fs::OpenOptions::new().write(true).open(&job_path).is_err(), "others can't change the job");
        assert_eq!(helper(ELEVATED_PASTE, &[&job_path]), Some(0));
        drop(file);
        fs::remove_file(&job_path).unwrap();
        assert_eq!(fs::read_to_string(sandbox.path("dest/a.txt")).unwrap(), "hello");
        assert!(!sandbox.path("a.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn denied_operations_ask_for_elevation() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const EVERYONE: &str = "*S-1-1-0";
        let icacls = |path: &Path, args: &[&str]| { assert!(std::process::Command::new("icacls").arg(path).args(args).creation_flags(CREATE_NO_WINDOW).output().unwrap().status.success()); };
        let sandbox = Sandbox::new("denied-elevation");
        let locked = sandbox.path("locked");
        fs::create_dir_all(&locked).unwrap();
        let held = locked.join("held.txt");
        fs::write(&held, "x").unwrap();
        fs::write(sandbox.path("a.txt"), "x").unwrap();
        fs::create_dir_all(sandbox.path("open")).unwrap();
        let source = sandbox.path("a.txt").to_string_lossy().to_string();
        let open = sandbox.path("open").to_string_lossy().to_string();
        let locked_text = locked.to_string_lossy().to_string();
        let held_text = held.to_string_lossy().to_string();

        icacls(&held, &["/deny", &format!("{EVERYONE}:(DE)")]);
        icacls(&locked, &["/deny", &format!("{EVERYONE}:(WD,AD,DC)")]);
        let folder = create_folder(locked_text.clone());
        let file = create_file(locked_text.clone());
        let rename = rename_entry(held_text.clone(), "other.txt".to_string());
        let paste_in = paste_entries(vec![source.clone()], locked_text.clone(), false, None);
        let move_out = paste_entries(vec![held_text.clone()], open.clone(), true, None);
        let moved_copy_left = sandbox.path("open/held.txt").exists();
        let copy_out = paste_entries(vec![held_text.clone()], open.clone(), false, None);
        let delete_file = delete_entry(held_text.clone());
        let elevated_delete_left_it = held.exists();
        icacls(&locked, &["/remove:d", EVERYONE]);
        icacls(&held, &["/remove:d", EVERYONE]);

        assert_eq!(folder.unwrap_err(), format!("{NEEDS_ELEVATION}New Folder"));
        assert_eq!(file.unwrap_err(), format!("{NEEDS_ELEVATION}New File.txt"));
        assert_eq!(rename.unwrap_err(), NEEDS_ELEVATION);
        assert_eq!(paste_in.unwrap_err(), NEEDS_ELEVATION);
        assert_eq!(move_out.unwrap_err(), NEEDS_ELEVATION);
        assert!(!moved_copy_left, "a refused move leaves no copy behind");
        assert_eq!(copy_out.unwrap().len(), 1, "copying out only needs read access");
        assert_eq!(delete_file.unwrap_err(), NEEDS_ELEVATION, "a protected file isn't reported as in use");
        assert!(elevated_delete_left_it);
    }

    #[test]
    fn compress_zips_files_and_folders_into_an_archive_named_after_the_first() {
        let sandbox = Sandbox::new("compress");
        fs::write(sandbox.path("notes.txt"), "hello").unwrap();
        fs::create_dir_all(sandbox.path("photos/empty")).unwrap();
        fs::write(sandbox.path("photos/cat.jpg"), "meow").unwrap();
        let paths = vec![sandbox.path("notes.txt").to_string_lossy().to_string(), sandbox.path("photos").to_string_lossy().to_string()];
        let archive = compress(&paths).unwrap();
        assert_eq!(archive, sandbox.path("notes.zip").to_string_lossy().to_string());
        let mut zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
        let mut names: Vec<String> = zip.file_names().map(|name| name.unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["notes.txt", "photos/", "photos/cat.jpg", "photos/empty/"]);
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("photos/cat.jpg").unwrap(), &mut contents).unwrap();
        assert_eq!(contents, "meow");
        // A second archive of the same item gets a fresh name instead of overwriting the first.
        assert_eq!(compress(&paths).unwrap(), sandbox.path("notes 2.zip").to_string_lossy().to_string());
    }

    #[test]
    fn paste_skips_folder_into_itself_or_its_subfolder() {
        let sandbox = Sandbox::new("paste-into-self");
        fs::create_dir_all(sandbox.path("a").join("sub")).unwrap();
        let source = sandbox.path("a").to_string_lossy().to_string();
        for cut in [false, true] {
            for dest in [sandbox.path("a"), sandbox.path("a").join("sub")] {
                let result = paste_entries(vec![source.clone()], dest.to_string_lossy().to_string(), cut, None).unwrap();
                assert!(result.is_empty());
            }
        }
        assert!(sandbox.path("a").join("sub").is_dir());
        assert!(!sandbox.path("a").join("a").exists());
        assert!(!sandbox.path("a").join("sub").join("a").exists());
    }

    #[test]
    fn unique_destination_avoids_existing_names() {
        let sandbox = Sandbox::new("unique-dest");
        fs::write(sandbox.path("a.txt"), b"one").unwrap();
        assert_eq!(unique_destination(&sandbox.dir, "b.txt"), sandbox.path("b.txt"));
        assert_eq!(unique_destination(&sandbox.dir, "a.txt"), sandbox.path("a - Copy.txt"));
        fs::write(sandbox.path("a - Copy.txt"), b"two").unwrap();
        assert_eq!(unique_destination(&sandbox.dir, "a.txt"), sandbox.path("a - Copy (2).txt"));
    }

    #[test]
    fn copy_paste_duplicates_into_new_name_in_same_folder() {
        let sandbox = Sandbox::new("copy-same-folder");
        fs::write(sandbox.path("a.txt"), b"hello").unwrap();
        let result = paste_entries(vec![sandbox.path("a.txt").to_string_lossy().to_string()], sandbox.dir.to_string_lossy().to_string(), false, None).unwrap();
        assert_eq!(result, vec![sandbox.path("a - Copy.txt").to_string_lossy().to_string()]);
        assert!(sandbox.path("a.txt").exists());
        assert_eq!(fs::read_to_string(sandbox.path("a - Copy.txt")).unwrap(), "hello");
    }

    #[test]
    fn cut_paste_into_same_folder_is_a_no_op() {
        let sandbox = Sandbox::new("cut-same-folder");
        fs::write(sandbox.path("a.txt"), b"hello").unwrap();
        let result = paste_entries(vec![sandbox.path("a.txt").to_string_lossy().to_string()], sandbox.dir.to_string_lossy().to_string(), true, None).unwrap();
        assert_eq!(result, vec![sandbox.path("a.txt").to_string_lossy().to_string()]);
        assert!(sandbox.path("a.txt").exists());
    }

    #[test]
    fn cut_paste_moves_into_a_different_folder() {
        let source_sandbox = Sandbox::new("cut-source");
        let dest_sandbox = Sandbox::new("cut-dest");
        fs::write(source_sandbox.path("a.txt"), b"hello").unwrap();
        let result = paste_entries(vec![source_sandbox.path("a.txt").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string(), true, None).unwrap();
        assert_eq!(result, vec![dest_sandbox.path("a.txt").to_string_lossy().to_string()]);
        assert!(!source_sandbox.path("a.txt").exists());
        assert_eq!(fs::read_to_string(dest_sandbox.path("a.txt")).unwrap(), "hello");
    }

    #[test]
    fn copy_paste_directory_recurses() {
        let source_sandbox = Sandbox::new("copy-dir-source");
        let dest_sandbox = Sandbox::new("copy-dir-dest");
        fs::create_dir_all(source_sandbox.path("folder/nested")).unwrap();
        fs::write(source_sandbox.path("folder/file.txt"), b"top").unwrap();
        fs::write(source_sandbox.path("folder/nested/inner.txt"), b"deep").unwrap();
        paste_entries(vec![source_sandbox.path("folder").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string(), false, None).unwrap();
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/file.txt")).unwrap(), "top");
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/nested/inner.txt")).unwrap(), "deep");
        assert!(source_sandbox.path("folder").exists());
    }

    #[test]
    fn scan_finds_conflicts_only_for_files_that_already_exist_in_the_destination() {
        let source_sandbox = Sandbox::new("scan-source");
        let dest_sandbox = Sandbox::new("scan-dest");
        fs::create_dir_all(source_sandbox.path("folder/nested")).unwrap();
        fs::write(source_sandbox.path("folder/new.txt"), b"new").unwrap();
        fs::write(source_sandbox.path("folder/clash.txt"), b"source version").unwrap();
        fs::write(source_sandbox.path("folder/nested/deep-clash.txt"), b"source deep").unwrap();
        fs::create_dir_all(dest_sandbox.path("folder/nested")).unwrap();
        fs::write(dest_sandbox.path("folder/clash.txt"), b"dest version").unwrap();
        fs::write(dest_sandbox.path("folder/nested/deep-clash.txt"), b"dest deep").unwrap();

        let conflicts = scan_paste_conflicts(vec![source_sandbox.path("folder").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string()).unwrap();
        let mut conflicts: Vec<_> = conflicts.into_iter().collect();
        conflicts.sort();
        let mut expected = vec![dest_sandbox.dir.join("folder").join("clash.txt").to_string_lossy().to_string(), dest_sandbox.dir.join("folder").join("nested").join("deep-clash.txt").to_string_lossy().to_string()];
        expected.sort();
        assert_eq!(conflicts, expected);
    }

    #[test]
    fn scan_reports_no_conflicts_when_folder_is_new() {
        let source_sandbox = Sandbox::new("scan-new-source");
        let dest_sandbox = Sandbox::new("scan-new-dest");
        fs::create_dir_all(source_sandbox.path("folder")).unwrap();
        fs::write(source_sandbox.path("folder/a.txt"), b"a").unwrap();
        let conflicts = scan_paste_conflicts(vec![source_sandbox.path("folder").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string()).unwrap();
        assert!(conflicts.is_empty());
    }

    #[test]
    fn copy_paste_merges_into_existing_folder_of_the_same_name_and_skips_by_default() {
        let source_sandbox = Sandbox::new("merge-copy-source");
        let dest_sandbox = Sandbox::new("merge-copy-dest");
        fs::create_dir_all(source_sandbox.path("folder")).unwrap();
        fs::write(source_sandbox.path("folder/new.txt"), b"new").unwrap();
        fs::write(source_sandbox.path("folder/clash.txt"), b"source version").unwrap();
        fs::create_dir_all(dest_sandbox.path("folder")).unwrap();
        fs::write(dest_sandbox.path("folder/existing.txt"), b"existing").unwrap();
        fs::write(dest_sandbox.path("folder/clash.txt"), b"dest version").unwrap();

        let result = paste_entries(vec![source_sandbox.path("folder").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string(), false, Some("skip".to_string())).unwrap();
        assert_eq!(result, vec![dest_sandbox.path("folder").to_string_lossy().to_string()]);
        // Merged: both the pre-existing and the newly copied file are present.
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/existing.txt")).unwrap(), "existing");
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/new.txt")).unwrap(), "new");
        // Skip: the conflicting file keeps the destination's content.
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/clash.txt")).unwrap(), "dest version");
        // Copy (not cut): source is untouched either way.
        assert_eq!(fs::read_to_string(source_sandbox.path("folder/clash.txt")).unwrap(), "source version");
    }

    #[test]
    fn copy_paste_merge_replaces_conflicts_when_asked() {
        let source_sandbox = Sandbox::new("merge-replace-source");
        let dest_sandbox = Sandbox::new("merge-replace-dest");
        fs::create_dir_all(source_sandbox.path("folder")).unwrap();
        fs::write(source_sandbox.path("folder/clash.txt"), b"source version").unwrap();
        fs::create_dir_all(dest_sandbox.path("folder")).unwrap();
        fs::write(dest_sandbox.path("folder/clash.txt"), b"dest version").unwrap();

        paste_entries(vec![source_sandbox.path("folder").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string(), false, Some("replace".to_string())).unwrap();
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/clash.txt")).unwrap(), "source version");
    }

    #[test]
    fn cut_paste_merge_moves_non_conflicting_files_and_leaves_skipped_ones_behind() {
        let source_sandbox = Sandbox::new("merge-cut-source");
        let dest_sandbox = Sandbox::new("merge-cut-dest");
        fs::create_dir_all(source_sandbox.path("folder")).unwrap();
        fs::write(source_sandbox.path("folder/moved.txt"), b"moved").unwrap();
        fs::write(source_sandbox.path("folder/clash.txt"), b"source version").unwrap();
        fs::create_dir_all(dest_sandbox.path("folder")).unwrap();
        fs::write(dest_sandbox.path("folder/clash.txt"), b"dest version").unwrap();

        paste_entries(vec![source_sandbox.path("folder").to_string_lossy().to_string()], dest_sandbox.dir.to_string_lossy().to_string(), true, Some("skip".to_string())).unwrap();
        // Non-conflicting file was moved out of the source.
        assert!(!source_sandbox.path("folder/moved.txt").exists());
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/moved.txt")).unwrap(), "moved");
        // Skipped conflicting file stays behind in the source rather than being lost.
        assert_eq!(fs::read_to_string(source_sandbox.path("folder/clash.txt")).unwrap(), "source version");
        assert_eq!(fs::read_to_string(dest_sandbox.path("folder/clash.txt")).unwrap(), "dest version");
    }
}

#[tauri::command]
fn set_watched_paths(app: AppHandle, state: State<WatcherState>, paths: Vec<String>, recursive_paths: Vec<String>) -> Result<(), String> {
    let mut watcher_guard = state.watcher.lock().map_err(|_| "watcher lock poisoned".to_string())?;
    if watcher_guard.is_none() {
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) { return; }
            let mut parents: Vec<String> = event.paths.iter()
                .filter_map(|changed| changed.parent())
                .map(|parent| parent.to_string_lossy().to_string())
                .collect();
            parents.dedup();
            for parent in parents {
                let _ = app.emit("fs-changed", parent);
            }
        }).map_err(|error| error.to_string())?;
        *watcher_guard = Some(watcher);
    }
    let watcher = watcher_guard.as_mut().unwrap();
    let mut watched = state.watched.lock().map_err(|_| "watched lock poisoned".to_string())?;
    // A path asked for both ways (one pane in tree view, the other not) is watched recursively, which covers both.
    let mut desired: HashMap<String, bool> = paths.into_iter().map(|path| (path, false)).collect();
    for path in recursive_paths {
        desired.insert(path, true);
    }
    for (old, old_recursive) in watched.iter() {
        if desired.get(old) != Some(old_recursive) {
            let _ = watcher.unwatch(Path::new(old));
        }
    }
    for (new, recursive) in desired.iter() {
        if watched.get(new) != Some(recursive) {
            let mode = if *recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            let _ = watcher.watch(Path::new(new), mode);
        }
    }
    *watched = desired;
    Ok(())
}

#[derive(Serialize, Clone)]
struct FolderNode {
    name: String,
    path: String,
    #[serde(rename = "fileCount")]
    file_count: u64,
    #[serde(rename = "totalSize")]
    total_size: u64,
    children: Vec<FolderNode>,
}

#[derive(Serialize, Clone)]
struct TreeProgress {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "scannedFiles")]
    scanned_files: u64,
    #[serde(rename = "scannedFolders")]
    scanned_folders: u64,
}

#[derive(Serialize, Clone)]
struct TreeDone {
    #[serde(rename = "requestId")]
    request_id: String,
    tree: FolderNode,
}

#[derive(Serialize, Clone)]
struct TreeCancelled {
    #[serde(rename = "requestId")]
    request_id: String,
}

// Tree view refuses folders with more files than this (a somewhat arbitrary limit; see README).
// Must match TREE_MAX_FILES in src/main.js.
const TREE_MAX_FILES: u64 = 200_000;

enum WalkStop {
    Cancelled,
    TooManyFiles,
}

fn walk_folder(
    path: &Path,
    cancel: &AtomicBool,
    scanned_files: &mut u64,
    scanned_folders: &mut u64,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<FolderNode, WalkStop> {
    if cancel.load(Ordering::Relaxed) {
        return Err(WalkStop::Cancelled);
    }
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let mut file_count = 0u64;
    let mut total_size = 0u64;
    let mut children = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(Result::ok) {
            if cancel.load(Ordering::Relaxed) {
                return Err(WalkStop::Cancelled);
            }
            let metadata = match entry.metadata() {
                Ok(value) => value,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                let child = walk_folder(&entry.path(), cancel, scanned_files, scanned_folders, on_progress)?;
                file_count += child.file_count;
                total_size += child.total_size;
                *scanned_folders += 1;
                on_progress(*scanned_files, *scanned_folders);
                children.push(child);
            } else {
                file_count += 1;
                total_size += metadata.len();
                *scanned_files += 1;
                if *scanned_files > TREE_MAX_FILES {
                    return Err(WalkStop::TooManyFiles);
                }
                on_progress(*scanned_files, *scanned_folders);
            }
        }
    }
    children.sort_by(|left, right| right.total_size.cmp(&left.total_size));
    Ok(FolderNode { name, path: path.to_string_lossy().to_string(), file_count, total_size, children })
}

#[tauri::command]
fn compute_tree_stats(app: AppHandle, state: State<TreeState>, request_id: String, path: String) -> Result<(), String> {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    state
        .cancels
        .lock()
        .map_err(|_| "tree state lock poisoned".to_string())?
        .insert(request_id.clone(), cancel_flag.clone());

    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut scanned_files = 0u64;
            let mut scanned_folders = 0u64;
            let mut last_emit = Instant::now();
            let result = walk_folder(Path::new(&path), &cancel_flag, &mut scanned_files, &mut scanned_folders, &mut |files, folders| {
                if last_emit.elapsed().as_millis() >= 100 {
                    let _ = app.emit("tree-progress", TreeProgress { request_id: request_id.clone(), scanned_files: files, scanned_folders: folders });
                    last_emit = Instant::now();
                }
            });
            if let Ok(mut cancels) = app.state::<TreeState>().cancels.lock() {
                cancels.remove(&request_id);
            }
            match result {
                Ok(tree) => {
                    let _ = app.emit("tree-done", TreeDone { request_id: request_id.clone(), tree });
                }
                Err(WalkStop::Cancelled) => {
                    let _ = app.emit("tree-cancelled", TreeCancelled { request_id: request_id.clone() });
                }
                Err(WalkStop::TooManyFiles) => {
                    let _ = app.emit("tree-too-many", TreeCancelled { request_id: request_id.clone() });
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[derive(Serialize)]
struct FileNameScan {
    // "done", "cancelled" or "too-many"
    status: &'static str,
    paths: Vec<String>,
}

// Lists every file under `path` for the search view. Shares the tree view's cancel flags (cancel it
// with cancel_tree_stats) and its TREE_MAX_FILES limit. Progress goes out as "search-progress".
#[tauri::command]
async fn scan_file_names(app: AppHandle, request_id: String, path: String, show_hidden: bool) -> Result<FileNameScan, String> {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    app.state::<TreeState>()
        .cancels
        .lock()
        .map_err(|_| "tree state lock poisoned".to_string())?
        .insert(request_id.clone(), cancel_flag.clone());
    let worker_app = app.clone();
    let worker_id = request_id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut paths = Vec::new();
        let mut scanned_folders = 0u64;
        let mut last_emit = Instant::now();
        let mut pending = vec![PathBuf::from(&path)];
        while let Some(folder) = pending.pop() {
            let Ok(entries) = fs::read_dir(&folder) else { continue };
            for entry in entries.filter_map(Result::ok) {
                if cancel_flag.load(Ordering::Relaxed) {
                    return FileNameScan { status: "cancelled", paths: Vec::new() };
                }
                let Ok(metadata) = entry.metadata() else { continue };
                let entry_path = entry.path();
                if !show_hidden && is_hidden(&entry_path, &entry.file_name().to_string_lossy()) {
                    continue;
                }
                if metadata.is_dir() {
                    scanned_folders += 1;
                    pending.push(entry_path);
                } else {
                    paths.push(entry_path.to_string_lossy().to_string());
                    if paths.len() as u64 > TREE_MAX_FILES {
                        return FileNameScan { status: "too-many", paths: Vec::new() };
                    }
                }
                if last_emit.elapsed().as_millis() >= 100 {
                    let _ = worker_app.emit("search-progress", TreeProgress { request_id: worker_id.clone(), scanned_files: paths.len() as u64, scanned_folders });
                    last_emit = Instant::now();
                }
            }
        }
        paths.sort_by_cached_key(|value| value.to_lowercase());
        FileNameScan { status: "done", paths }
    })
    .await
    .map_err(|error| error.to_string());
    if let Ok(mut cancels) = app.state::<TreeState>().cancels.lock() {
        cancels.remove(&request_id);
    }
    result
}

#[tauri::command]
fn cancel_tree_stats(state: State<TreeState>, request_id: String) -> Result<(), String> {
    if let Some(flag) = state.cancels.lock().map_err(|_| "tree state lock poisoned".to_string())?.get(&request_id) {
        flag.store(true, Ordering::Relaxed);
    }
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(CHARS[(b0 >> 2) as usize] as char);
        out.push(CHARS[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { CHARS[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { CHARS[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

// Extracts the small (16x16) shell icon associated with a file extension - the same icon
// Windows Explorer shows in its details view - by asking the shell for the icon it would use
// for a hypothetical file with that extension (SHGFI_USEFILEATTRIBUTES, no real file needed),
// then compositing that HICON onto a 32bpp DIB section via DrawIconEx so both old-style
// mask-based icons and modern per-pixel-alpha icons come out with correct transparency.
#[cfg(windows)]
fn extract_extension_icon_rgba(extension: &str) -> Option<(u32, u32, Vec<u8>)> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    const SHGFI_ICON: u32 = 0x100;
    const SHGFI_SMALLICON: u32 = 0x1;
    const SHGFI_USEFILEATTRIBUTES: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

    #[repr(C)]
    struct ShFileInfoW {
        h_icon: isize,
        i_icon: i32,
        dw_attributes: u32,
        sz_display_name: [u16; 260],
        sz_type_name: [u16; 80],
    }

    extern "system" {
        fn SHGetFileInfoW(path: *const u16, attrs: u32, info: *mut ShFileInfoW, size: u32, flags: u32) -> isize;
        fn DestroyIcon(icon: isize) -> i32;
    }

    let dummy_name = format!("dummy{}", extension);
    let wide: Vec<u16> = OsStr::new(&dummy_name).encode_wide().chain(Some(0)).collect();

    let mut info = ShFileInfoW { h_icon: 0, i_icon: 0, dw_attributes: 0, sz_display_name: [0; 260], sz_type_name: [0; 80] };
    let result = unsafe {
        SHGetFileInfoW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL, &mut info, std::mem::size_of::<ShFileInfoW>() as u32, SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES)
    };
    if result == 0 || info.h_icon == 0 {
        return None;
    }
    let rgba = icon_rgba(info.h_icon);
    unsafe { DestroyIcon(info.h_icon) };
    rgba
}

// Draws an HICON at 16x16 and returns its pixels as RGBA. The caller still owns the icon.
#[cfg(windows)]
fn icon_rgba(h_icon: isize) -> Option<(u32, u32, Vec<u8>)> {
    const DIB_RGB_COLORS: u32 = 0;
    const DI_NORMAL: u32 = 0x3;
    const BI_RGB: u32 = 0;
    const ICON_SIZE: i32 = 16;

    #[repr(C)]
    struct BitmapInfoHeader {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_x_pels_per_meter: i32,
        bi_y_pels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }
    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    extern "system" {
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn DeleteDC(hdc: isize) -> i32;
        fn CreateDIBSection(hdc: isize, info: *const BitmapInfo, usage: u32, bits: *mut *mut u8, section: isize, offset: u32) -> isize;
        fn SelectObject(hdc: isize, obj: isize) -> isize;
        fn DeleteObject(obj: isize) -> i32;
        fn DrawIconEx(hdc: isize, x: i32, y: i32, icon: isize, width: i32, height: i32, frame: u32, flicker_brush: isize, flags: u32) -> i32;
    }

    let hdc = unsafe { CreateCompatibleDC(0) };
    if hdc == 0 {
        return None;
    }

    let bitmap_info = BitmapInfo {
        header: BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: ICON_SIZE,
            bi_height: -ICON_SIZE,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            bi_size_image: 0,
            bi_x_pels_per_meter: 0,
            bi_y_pels_per_meter: 0,
            bi_clr_used: 0,
            bi_clr_important: 0,
        },
        colors: [0],
    };

    let mut bits_ptr: *mut u8 = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(hdc, &bitmap_info, DIB_RGB_COLORS, &mut bits_ptr, 0, 0) };
    if bitmap == 0 || bits_ptr.is_null() {
        unsafe { DeleteDC(hdc) };
        return None;
    }

    let buffer_len = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let previous = unsafe { SelectObject(hdc, bitmap) };
    unsafe {
        std::ptr::write_bytes(bits_ptr, 0, buffer_len);
        DrawIconEx(hdc, 0, 0, h_icon, ICON_SIZE, ICON_SIZE, 0, 0, DI_NORMAL);
    }

    let mut rgba = vec![0u8; buffer_len];
    unsafe { std::ptr::copy_nonoverlapping(bits_ptr, rgba.as_mut_ptr(), buffer_len) };
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2); // BGRA -> RGBA
    }

    unsafe {
        SelectObject(hdc, previous);
        DeleteObject(bitmap);
        DeleteDC(hdc);
    }

    Some((ICON_SIZE as u32, ICON_SIZE as u32, rgba))
}

#[cfg(windows)]
fn extension_icon_png(extension: &str) -> Option<Vec<u8>> {
    let (width, height, rgba) = extract_extension_icon_rgba(extension)?;
    rgba_png(width, height, rgba)
}

#[cfg(windows)]
fn rgba_png(width: u32, height: u32, rgba: Vec<u8>) -> Option<Vec<u8>> {
    let image = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .ok()?;
    Some(bytes)
}

// The icon Finder shows for files with this extension, drawn at 32x32 so it stays sharp on Retina
// displays when shown at 16 points. iconForContentType: would need a UTType for the extension;
// iconForFileType: takes the extension directly and still works.
#[cfg(target_os = "macos")]
fn extension_icon_png(extension: &str) -> Option<Vec<u8>> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;

    let file_type = NSString::from_str(extension.trim_start_matches('.'));
    #[allow(deprecated)]
    let image = NSWorkspace::sharedWorkspace().iconForFileType(&file_type);
    image_png(&image)
}

#[cfg(target_os = "macos")]
fn image_png(image: &objc2_app_kit::NSImage) -> Option<Vec<u8>> {
    use objc2::AnyThread;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace, NSGraphicsContext};
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize};

    const ICON_PIXELS: isize = 32;
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), ICON_PIXELS, ICON_PIXELS, 8, 4, true, false, NSDeviceRGBColorSpace, 0, 0,
        )
    }?;
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ICON_PIXELS as f64, ICON_PIXELS as f64));
    image.drawInRect_fromRect_operation_fraction(rect, NSRect::ZERO, NSCompositingOperation::SourceOver, 1.0);
    NSGraphicsContext::restoreGraphicsState_class();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    Some(png.to_vec())
}

#[cfg(any(windows, target_os = "macos"))]
#[tauri::command]
fn get_extension_icon(state: State<IconCacheState>, extension: String) -> Option<String> {
    let key = extension.to_lowercase();
    if let Ok(cache) = state.cache.lock() {
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }
    let result = extension_icon_png(&key).map(|bytes| format!("data:image/png;base64,{}", base64_encode(&bytes)));
    if let Ok(mut cache) = state.cache.lock() {
        cache.insert(key, result.clone());
    }
    result
}

#[cfg(not(any(windows, target_os = "macos")))]
#[tauri::command]
fn get_extension_icon(_state: State<IconCacheState>, _extension: String) -> Option<String> {
    None
}

// Long edge of a generated thumbnail in pixels: covers the 150px thumbnail box at 2x display scaling.
#[cfg(windows)]
const THUMB_MAX_DIM: u32 = 300;
// Concurrent WIC decodes can crash the process, so HEIC thumbnails are made one at a time.
#[cfg(windows)]
static HEIC_DECODE_LOCK: Mutex<()> = Mutex::new(());

// WebView2 can't render HEIC, so Windows decodes it with the Windows Imaging Component (which needs
// the "HEIF Image Extensions" codec) and hands back a small JPEG as a data URI.
#[cfg(windows)]
#[tauri::command]
async fn get_heic_thumbnail(path: String) -> Option<String> {
    let jpeg = tauri::async_runtime::spawn_blocking(move || {
        let _guard = HEIC_DECODE_LOCK.lock().ok()?;
        heic_thumbnail_jpeg(&path).ok()
    }).await.ok().flatten()?;
    Some(format!("data:image/jpeg;base64,{}", base64_encode(&jpeg)))
}

// macOS's WebKit renders HEIC itself, so the frontend never asks for these there.
#[cfg(not(windows))]
#[tauri::command]
async fn get_heic_thumbnail(_path: String) -> Option<String> {
    None
}

// Video thumbnails come from the shell, so they match what Windows Explorer shows (and use its cache).
#[cfg(windows)]
#[tauri::command]
async fn get_video_thumbnail(path: String) -> Option<String> {
    let jpeg = tauri::async_runtime::spawn_blocking(move || shell_thumbnail_jpeg(&path).ok()).await.ok().flatten()?;
    Some(format!("data:image/jpeg;base64,{}", base64_encode(&jpeg)))
}

// macOS's WebKit draws video frames itself, so the frontend never asks for these there.
#[cfg(not(windows))]
#[tauri::command]
async fn get_video_thumbnail(_path: String) -> Option<String> {
    None
}

#[cfg(windows)]
fn shell_thumbnail_jpeg(path: &str) -> Result<Vec<u8>, String> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::SIZE,
            Graphics::Gdi::{DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS},
            System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
            UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY},
        },
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let path_wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR::from_raw(path_wide.as_ptr()), None)
            .map_err(|e| format!("Shell item failed: {e}"))?;
        // THUMBNAILONLY: a file with no thumbnail should fail rather than come back as its icon.
        let size = SIZE { cx: THUMB_MAX_DIM as i32, cy: THUMB_MAX_DIM as i32 };
        let bitmap = factory
            .GetImage(size, SIIGBF_THUMBNAILONLY)
            .map_err(|e| format!("Thumbnail failed: {e}"))?;

        let mut info = BITMAP::default();
        let read = GetObjectW(bitmap.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut BITMAP as *mut _));
        let (width, height) = (info.bmWidth, info.bmHeight.abs());
        let mut pixels = vec![0u8; (width.max(0) * height * 4) as usize];
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative: rows top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = if read == 0 || width <= 0 || height <= 0 {
            0
        } else {
            let dc = GetDC(None);
            let lines = GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr() as *mut _), &mut header, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            lines
        };
        let _ = DeleteObject(bitmap.into());
        if lines == 0 {
            return Err("Reading thumbnail pixels failed".to_string());
        }

        // BGRA from GDI -> RGB for the JPEG encoder.
        let rgb: Vec<u8> = pixels.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
        let image = image::RgbImage::from_raw(width as u32, height as u32, rgb).ok_or("Failed to construct image from pixel data")?;
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .map_err(|e| format!("JPEG encode failed: {e}"))?;
        Ok(bytes)
    }
}

#[cfg(windows)]
fn heic_thumbnail_jpeg(path: &str) -> Result<Vec<u8>, String> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::GENERIC_READ,
            Graphics::Imaging::{
                CLSID_WICImagingFactory, IWICBitmapScaler, IWICFormatConverter, IWICImagingFactory,
                WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeMedianCut,
                WICDecodeMetadataCacheOnDemand, WICRect, GUID_WICPixelFormat24bppBGR,
            },
            System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
        },
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("WIC init failed: {e}"))?;
        let path_wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let decoder = factory
            .CreateDecoderFromFilename(PCWSTR::from_raw(path_wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .map_err(|e| format!("HEIC decode failed (HEIF Image Extensions required): {e}"))?;
        let frame = decoder.GetFrame(0).map_err(|e| format!("Frame read failed: {e}"))?;

        let (mut orig_w, mut orig_h) = (0u32, 0u32);
        frame.GetSize(&mut orig_w, &mut orig_h).map_err(|e| format!("GetSize failed: {e}"))?;
        let scale = (THUMB_MAX_DIM as f64 / orig_w.max(orig_h).max(1) as f64).min(1.0);
        let out_w = ((orig_w as f64 * scale).round() as u32).max(1);
        let out_h = ((orig_h as f64 * scale).round() as u32).max(1);

        let scaler: IWICBitmapScaler = factory.CreateBitmapScaler().map_err(|e| format!("Scaler create failed: {e}"))?;
        scaler.Initialize(&frame, out_w, out_h, WICBitmapInterpolationModeFant).map_err(|e| format!("Scaler init failed: {e}"))?;
        let converter: IWICFormatConverter = factory.CreateFormatConverter().map_err(|e| format!("Format converter failed: {e}"))?;
        converter
            .Initialize(&scaler, &GUID_WICPixelFormat24bppBGR, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeMedianCut)
            .map_err(|e| format!("Converter init failed: {e}"))?;

        let stride = out_w * 3;
        let mut buffer = vec![0u8; (stride * out_h) as usize];
        let rect = WICRect { X: 0, Y: 0, Width: out_w as i32, Height: out_h as i32 };
        converter.CopyPixels(&rect, stride, &mut buffer).map_err(|e| format!("CopyPixels failed: {e}"))?;
        // WIC hands back BGR; the image crate wants RGB.
        for pixel in buffer.chunks_exact_mut(3) {
            pixel.swap(0, 2);
        }

        let image = image::RgbImage::from_raw(out_w, out_h, buffer).ok_or("Failed to construct image from pixel data")?;
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .map_err(|e| format!("JPEG encode failed: {e}"))?;
        Ok(bytes)
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    if let Some(code) = elevated_helper_exit_code(std::env::args_os().skip(1)) { std::process::exit(code); }
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(WatcherState::default())
        .manage(TreeState::default())
        .manage(IconCacheState::default());
    #[cfg(not(any(windows, target_os = "macos")))]
    let builder = builder.manage(ClipboardState::default());
    builder
        .setup(|app| { watch_drives(app.handle().clone()); Ok(()) })
        .invoke_handler(tauri::generate_handler![read_directory, rename_entry, delete_entries, restore_entries,create_file, create_folder, create_entry_elevated, rename_entry_elevated, paste_entries_elevated, delete_entries_elevated, show_properties, lockhunter_info, open_in_lockhunter, platform_name, same_volume, edit_in_text_editor, compress_entries, list_open_with_apps, open_with_app, open_with_other_app, list_drives, set_watched_paths, compute_tree_stats, cancel_tree_stats, scan_file_names, stat_entries, known_folders, existing_folders, read_folders_config, write_folders_config, default_folders_config, get_extension_icon, get_heic_thumbnail, get_video_thumbnail, clipboard_write_paths, clipboard_read_paths, scan_paste_conflicts, paste_entries])
        .run(tauri::generate_context!())
        .expect("error while running Rove Explorer");
}
