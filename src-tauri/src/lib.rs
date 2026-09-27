use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, State};

#[derive(Default)]
struct WatcherState {
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct TreeState {
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

#[derive(Default)]
struct IconCacheState {
    cache: Mutex<HashMap<String, Option<String>>>,
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
#[tauri::command]
fn list_drives() -> Vec<Drive> {
    let mut drives: Vec<Drive> = sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
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
                modified: metadata.modified().ok().and_then(|date| date.duration_since(std::time::UNIX_EPOCH).ok()).map(|date| date.as_secs().to_string()).unwrap_or_else(|| "—".to_string()),
                path: item.path().to_string_lossy().to_string(),
            })
        }).collect::<Vec<_>>();
    entries.sort_by(|left, right| entry_kind_rank(&left.kind).cmp(&entry_kind_rank(&right.kind)).then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())));
    Ok(entries)
}

#[tauri::command]
fn rename_entry(path: String, new_name: String) -> Result<(), String> {
    if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err("Invalid folder name".to_string());
    }
    let source = Path::new(&path);
    let target = source.parent().ok_or_else(|| "Invalid folder path".to_string())?.join(new_name);
    fs::rename(source, target).map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_entry(path: String) -> Result<(), String> {
    trash::delete(&path).map_err(|error| error.to_string())
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

#[tauri::command]
fn show_properties(paths: Vec<String>) -> Result<(), String> {
    #[cfg(windows)]
    {
        win_properties::show(&paths)
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
        Err("The properties dialog is only available on Windows".to_string())
    }
}

// Opens Explorer's own Properties dialog: a single item goes through the "properties" shell verb,
// several items through SHMultiFileProperties, which shows the combined "N files" sheet Explorer does.
#[cfg(windows)]
mod win_properties {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;

    const SEE_MASK_INVOKEIDLIST: u32 = 0x0000_000C;
    const SW_SHOW: i32 = 5;

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
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(ptr: *mut c_void);
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    pub fn show(paths: &[String]) -> Result<(), String> {
        match paths {
            [] => Ok(()),
            [path] => show_single(path),
            _ => show_multiple(paths),
        }
    }

    fn show_single(path: &str) -> Result<(), String> {
        let verb = wide("properties");
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
            let hr = unsafe { SHMultiFileProperties(data_object, 0) };
            unsafe { release(data_object) };
            if hr < 0 {
                return Err(format!("SHMultiFileProperties failed (HRESULT {hr:#x})"));
            }
            Ok(())
        })();
        for pidl in pidls {
            unsafe { CoTaskMemFree(pidl) };
        }
        result
    }

    // IUnknown::Release is the third vtable slot of every COM interface.
    unsafe fn release(object: *mut c_void) {
        let vtable = *(object as *const *const usize);
        let release: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtable.add(2));
        release(object);
    }
}

// Copies the OS clipboard's file-list format (CF_HDROP) plus the "Preferred DropEffect" format
// that Explorer uses to mark a copy vs. a cut, so cutting/copying here is interchangeable with
// Explorer: paths copied in Rove can be pasted in Explorer and vice versa.
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

#[cfg(not(windows))]
#[tauri::command]
fn clipboard_write_paths(state: State<ClipboardState>, paths: Vec<String>, cut: bool) -> Result<(), String> {
    *state.files.lock().map_err(|_| "clipboard lock poisoned".to_string())? = Some((paths, cut));
    Ok(())
}

#[cfg(not(windows))]
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
            let dir = std::env::temp_dir().join(format!("rove-test-{name}-{}", std::process::id()));
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
fn set_watched_paths(app: AppHandle, state: State<WatcherState>, paths: Vec<String>) -> Result<(), String> {
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
    let desired: HashSet<String> = paths.into_iter().collect();
    for old in watched.iter() {
        if !desired.contains(old) {
            let _ = watcher.unwatch(Path::new(old));
        }
    }
    for new in desired.iter() {
        if !watched.contains(new) {
            let _ = watcher.watch(Path::new(new), RecursiveMode::NonRecursive);
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
    const DIB_RGB_COLORS: u32 = 0;
    const DI_NORMAL: u32 = 0x3;
    const BI_RGB: u32 = 0;
    const ICON_SIZE: i32 = 16;

    #[repr(C)]
    struct ShFileInfoW {
        h_icon: isize,
        i_icon: i32,
        dw_attributes: u32,
        sz_display_name: [u16; 260],
        sz_type_name: [u16; 80],
    }
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
        fn SHGetFileInfoW(path: *const u16, attrs: u32, info: *mut ShFileInfoW, size: u32, flags: u32) -> isize;
        fn DestroyIcon(icon: isize) -> i32;
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn DeleteDC(hdc: isize) -> i32;
        fn CreateDIBSection(hdc: isize, info: *const BitmapInfo, usage: u32, bits: *mut *mut u8, section: isize, offset: u32) -> isize;
        fn SelectObject(hdc: isize, obj: isize) -> isize;
        fn DeleteObject(obj: isize) -> i32;
        fn DrawIconEx(hdc: isize, x: i32, y: i32, icon: isize, width: i32, height: i32, frame: u32, flicker_brush: isize, flags: u32) -> i32;
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

    let hdc = unsafe { CreateCompatibleDC(0) };
    if hdc == 0 {
        unsafe { DestroyIcon(info.h_icon) };
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
        unsafe {
            DeleteDC(hdc);
            DestroyIcon(info.h_icon);
        }
        return None;
    }

    let buffer_len = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let previous = unsafe { SelectObject(hdc, bitmap) };
    unsafe {
        std::ptr::write_bytes(bits_ptr, 0, buffer_len);
        DrawIconEx(hdc, 0, 0, info.h_icon, ICON_SIZE, ICON_SIZE, 0, 0, DI_NORMAL);
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
        DestroyIcon(info.h_icon);
    }

    Some((ICON_SIZE as u32, ICON_SIZE as u32, rgba))
}

#[cfg(windows)]
#[tauri::command]
fn get_extension_icon(state: State<IconCacheState>, extension: String) -> Option<String> {
    let key = extension.to_lowercase();
    if let Ok(cache) = state.cache.lock() {
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }
    let result = extract_extension_icon_rgba(&key).and_then(|(width, height, rgba)| {
        let image = image::RgbaImage::from_raw(width, height, rgba)?;
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .ok()?;
        Some(format!("data:image/png;base64,{}", base64_encode(&bytes)))
    });
    if let Ok(mut cache) = state.cache.lock() {
        cache.insert(key, result.clone());
    }
    result
}

#[cfg(not(windows))]
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
    #[cfg(not(windows))]
    let builder = builder.manage(ClipboardState::default());
    builder
        .invoke_handler(tauri::generate_handler![read_directory, rename_entry, delete_entry, create_file, create_folder, show_properties, platform_name, edit_in_text_editor, list_drives, set_watched_paths, compute_tree_stats, cancel_tree_stats, get_extension_icon, clipboard_write_paths, clipboard_read_paths, scan_paste_conflicts, paste_entries])
        .run(tauri::generate_context!())
        .expect("error while running Rove");
}
