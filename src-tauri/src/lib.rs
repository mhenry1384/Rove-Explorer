use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, State};

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

#[tauri::command]
fn read_directory(path: String, show_hidden: bool) -> Result<Vec<Entry>, String> {
    let mut entries = fs::read_dir(&path).map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter_map(|item| {
            let metadata = item.metadata().ok()?;
            let name = item.file_name().to_string_lossy().to_string();
            if !show_hidden && is_hidden(&item.path(), &name) { return None; }
            let is_dir = metadata.is_dir();
            let extension = if is_dir { "—".to_string() } else {
                Path::new(&name).extension().map(|value| format!(".{}", value.to_string_lossy())).unwrap_or_else(|| "—".to_string())
            };
            Some(Entry {
                kind: if is_dir { "folder" } else { "file" }.to_string(),
                name,
                extension,
                size: if is_dir { "—".to_string() } else { format_size(metadata.len()) },
                bytes: if is_dir { 0 } else { metadata.len() },
                modified: metadata.modified().ok().and_then(|date| date.duration_since(std::time::UNIX_EPOCH).ok()).map(|date| date.as_secs().to_string()).unwrap_or_else(|| "—".to_string()),
                path: item.path().to_string_lossy().to_string(),
            })
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

#[tauri::command]
fn rename_entry(path: String, new_name: String) -> Result<(), String> {
    if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains(INVALID_NAME_CHARS) {
        return Err("Invalid folder name".to_string());
    }
    let source = Path::new(&path);
    let target = source.parent().ok_or_else(|| "Invalid folder path".to_string())?.join(&new_name);
    // fs::rename silently replaces an existing file, so refuse instead. On a case-insensitive drive a
    // case-only rename ("photo" -> "Photo") also "exists", but both paths resolve to the same item.
    if target.exists() && fs::canonicalize(source).ok() != fs::canonicalize(&target).ok() {
        return Err(format!("An item named \"{new_name}\" already exists in this folder."));
    }
    fs::rename(source, &target).map_err(|error| readable_rename_error(&error, source.is_dir()))
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

#[tauri::command]
fn delete_entry(path: String) -> Result<(), String> {
    trash::delete(&path).map_err(readable_trash_error)?;
    // The shell recycles silently: an item that is in use is skipped yet the operation still
    // reports success, so check what is left behind. Whatever survived inside a folder is what
    // the other program has open (everything else already went to the Recycle Bin).
    let target = Path::new(&path);
    if !target.exists() { return Ok(()); }
    let mut remaining = Vec::new();
    if target.is_dir() { collect_files(target, target, &mut remaining); }
    if remaining.is_empty() {
        return Err("It is being used by another program.".to_string());
    }
    let shown = remaining.iter().take(5).map(|name| format!("  {name}")).collect::<Vec<_>>().join("\n");
    let more = if remaining.len() > 5 { format!("\n  ...and {} more", remaining.len() - 5) } else { String::new() };
    Err(format!("These items inside it are being used by another program:\n{shown}{more}"))
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
    fs::File::create(&candidate).map_err(|error| error.to_string())?;
    Ok(candidate.to_string_lossy().to_string())
}

#[tauri::command]
fn create_folder(dest_dir: String) -> Result<String, String> {
    let candidate = unused_name(Path::new(&dest_dir), "New Folder", "");
    fs::create_dir(&candidate).map_err(|error| error.to_string())?;
    Ok(candidate.to_string_lossy().to_string())
}

#[tauri::command]
fn platform_name() -> &'static str {
    std::env::consts::OS
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

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
        fn SHParseDisplayName(name: *const u16, bind_ctx: *mut c_void, pidl: *mut *mut c_void, sfgao_in: u32, sfgao_out: *mut u32) -> i32;
        fn SHCreateDataObject(pidl_folder: *const c_void, count: u32, pidls: *const *mut c_void, inner: *mut c_void, riid: *const Guid, out: *mut *mut c_void) -> i32;
        fn SHMultiFileProperties(data_object: *mut c_void, flags: u32) -> i32;
        fn SHAssocEnumHandlers(extension: *const u16, filter: u32, enumerator: *mut *mut c_void) -> i32;
        fn ExtractIconExW(file: *const u16, index: i32, large: *mut isize, small: *mut isize, count: u32) -> u32;
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(ptr: *mut c_void);
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    }
    extern "system" {
        fn DestroyIcon(icon: isize) -> i32;
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
        let location = take_co_string(location);
        let mut icon = 0;
        if ExtractIconExW(wide(&location).as_ptr(), index, std::ptr::null_mut(), &mut icon, 1) == 0 || icon == 0 {
            return None;
        }
        let png = super::icon_rgba(icon).and_then(|(width, height, rgba)| super::rgba_png(width, height, rgba));
        DestroyIcon(icon);
        png.map(|bytes| format!("data:image/png;base64,{}", super::base64_encode(&bytes)))
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
            result = with_data_object(&[path.to_string()], |data_object| {
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

#[tauri::command]
fn paste_entries(paths: Vec<String>, dest_dir: String, cut: bool, conflict_resolution: Option<String>) -> Result<Vec<String>, String> {
    let resolution = if conflict_resolution.as_deref() == Some("replace") { ConflictResolution::Replace } else { ConflictResolution::Skip };
    let dest = Path::new(&dest_dir);
    let mut results = Vec::with_capacity(paths.len());
    for source_path in paths {
        let source = Path::new(&source_path);
        let Some(name) = source.file_name().map(|value| value.to_string_lossy().to_string()) else { continue };
        if cut && source.parent() == Some(dest) {
            // Cutting and pasting back into the same folder is a no-op, like Explorer.
            results.push(source_path);
            continue;
        }
        if is_same_or_inside(dest, source) {
            // A folder can't go into itself or one of its own subfolders (e.g. dropped onto its own row).
            continue;
        }
        let existing_target = dest.join(&name);
        if source.is_dir() && existing_target.is_dir() {
            // A same-named folder already lives here: merge into it instead of renaming a copy alongside it.
            merge_dir_recursive(source, &existing_target, cut, resolution).map_err(|error| error.to_string())?;
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
                    copy_dir_recursive(source, &target).map_err(|error| error.to_string())?;
                    fs::remove_dir_all(source).map_err(|error| error.to_string())?;
                } else {
                    fs::copy(source, &target).map_err(|error| error.to_string())?;
                    fs::remove_file(source).map_err(|error| error.to_string())?;
                }
            }
        } else if source.is_dir() {
            copy_dir_recursive(source, &target).map_err(|error| error.to_string())?;
        } else {
            fs::copy(source, &target).map_err(|error| error.to_string())?;
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

fn walk_folder(
    path: &Path,
    cancel: &AtomicBool,
    scanned_files: &mut u64,
    scanned_folders: &mut u64,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Option<FolderNode> {
    if cancel.load(Ordering::Relaxed) {
        return None;
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
                return None;
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
                on_progress(*scanned_files, *scanned_folders);
            }
        }
    }
    children.sort_by(|left, right| right.total_size.cmp(&left.total_size));
    Some(FolderNode { name, path: path.to_string_lossy().to_string(), file_count, total_size, children })
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
            match result {
                Some(tree) => {
                    let _ = app.emit("tree-done", TreeDone { request_id: request_id.clone(), tree });
                }
                None => {
                    let _ = app.emit("tree-cancelled", TreeCancelled { request_id: request_id.clone() });
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_drag::init())
        .manage(WatcherState::default())
        .manage(TreeState::default())
        .manage(IconCacheState::default());
    #[cfg(not(any(windows, target_os = "macos")))]
    let builder = builder.manage(ClipboardState::default());
    builder
        .setup(|app| { watch_drives(app.handle().clone()); Ok(()) })
        .invoke_handler(tauri::generate_handler![read_directory, rename_entry, delete_entry, create_file, create_folder, show_properties, platform_name, same_volume, edit_in_text_editor, compress_entries, list_open_with_apps, open_with_app, open_with_other_app, list_drives, set_watched_paths, compute_tree_stats, cancel_tree_stats, get_extension_icon, clipboard_write_paths, clipboard_read_paths, scan_paste_conflicts, paste_entries])
        .run(tauri::generate_context!())
        .expect("error while running Rove Explorer");
}
