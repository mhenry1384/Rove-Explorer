import './style.css';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { homeDir, sep, resolveResource } from '@tauri-apps/api/path';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { openPath, openUrl } from '@tauri-apps/plugin-opener';
import { LogicalPosition } from '@tauri-apps/api/dpi';
import { ask, message } from '@tauri-apps/plugin-dialog';
import { Menu, MenuItem, IconMenuItem, Submenu, PredefinedMenuItem } from '@tauri-apps/api/menu';
import { startDrag } from '@crabnebula/tauri-plugin-drag';
import { version as APP_VERSION } from '../package.json';

const SEP = sep();
const isWindows = SEP === '\\';
const isMac = !isWindows && /Mac/.test(navigator.platform || navigator.userAgent);
const CURRENT_PLATFORM = isWindows ? 'windows' : isMac ? 'macos' : 'linux';
const COMPUTER_LABEL = isWindows ? 'This PC' : 'This Mac';
const TRASH_NAME = isWindows ? 'Recycle Bin' : 'Trash';
const state = { panes: [{ tabs: [], activeTab: 0 }, { tabs: [], activeTab: 0 }], activePane: 0, showHidden: false, editing: null, folders: [], drives: [], clipboard: null };
const stored = JSON.parse(localStorage.getItem('rove-explorer-state') || '{}');
const session = JSON.parse(localStorage.getItem('rove-explorer-session') || '{}');
const DOUBLE_CLICK_MS = 400;
const VIEW_MODES = [
  { id: 'details', label: 'Details', icon: '<path d="M2 3.5h12M2 8h12M2 12.5h12"/>' },
  { id: 'tree', label: 'Tree View', icon: '<path d="M3 2.5v9.5h4M3 7h4"/><rect x="8" y="5" width="6" height="4"/><rect x="8" y="10" width="6" height="4"/>' },
  { id: 'thumbnails', label: 'Thumbnails', icon: '<rect x="2" y="2" width="5" height="5"/><rect x="9" y="2" width="5" height="5"/><rect x="2" y="9" width="5" height="5"/><rect x="9" y="9" width="5" height="5"/>' },
  { id: 'search', label: 'Search', icon: '<circle cx="6.5" cy="6.5" r="4.5"/><path d="M10 10l4.5 4.5"/>' },
];
let lastEntryClick = { id: null, time: 0 };
let entryClickTimer = null;
// The row press that might still turn into a drag; resolved on the next pointermove/pointerup.
let pendingPress = null;
const DRAG_THRESHOLD_PX = 5;
let lastWatchedKey = '';
let scrollSelectionIntoView = false;
const TYPE_AHEAD_TIMEOUT_MS = 1000;
let typeAheadBuffer = '';
let typeAheadTime = 0;
const fsChangeTimers = new Map();
let platformName = CURRENT_PLATFORM;
let dragIconPath = null;
let internalDragPaths = null;
let dragHoverEl = null;
function normalizePath(path) { return path.replace(/[\\/]+/g, '/').replace(/\/+$/, '').toLowerCase(); }

function currentTab(pane) { return pane.tabs[pane.activeTab]; }
function extension(name) { const dot = name.lastIndexOf('.'); return dot > 0 ? name.slice(dot) : '—'; }
function displayName(name) { const dot = name.lastIndexOf('.'); return dot > 0 ? name.slice(0, dot) : name; }
function icon(kind) { return kind === 'folder' || kind === 'drive' ? '<img class="folder-icon" src="/folder-icons/folder.svg" alt="">' : '<span class="file-icon">·</span>'; }
const IMAGE_EXTENSIONS = new Set(['.png', '.jpg', '.jpeg', '.gif', '.bmp', '.webp', '.svg', '.ico', '.avif']);
// macOS's WebKit shows HEIC and video frames directly; WebView2 can't, so on Windows the backend makes those thumbnails.
const HEIC_EXTENSIONS = new Set(['.heic', '.heif']);
const VIDEO_EXTENSIONS = new Set(['.mp4', '.m4v', '.mov']);
function isImageFile(name) { const ext = extension(name).toLowerCase(); return IMAGE_EXTENSIONS.has(ext) || (isMac && HEIC_EXTENSIONS.has(ext)); }
function backendThumbCommand(name) {
  if (!isWindows) return null;
  const ext = extension(name).toLowerCase();
  return HEIC_EXTENSIONS.has(ext) ? 'get_heic_thumbnail' : VIDEO_EXTENSIONS.has(ext) ? 'get_video_thumbnail' : null;
}
// Keyed by path and modified time so an edited file gets a fresh thumbnail. Thumbnails are made one at a time, in listing order.
const backendThumbCache = new Map();
const backendThumbQueue = [];
let backendThumbBusy = false;
function backendThumbElements(key) { return document.querySelectorAll(`[data-backend-thumb="${CSS.escape(key)}"]`); }
function pumpBackendThumbs() {
  if (backendThumbBusy) return;
  const next = backendThumbQueue.shift();
  if (!next) return;
  // Skip files that are no longer on screen (e.g. the folder changed); they're queued again if they come back.
  if (!backendThumbElements(next.key).length) { backendThumbCache.delete(next.key); pumpBackendThumbs(); return; }
  backendThumbBusy = true;
  invoke(next.command, { path: next.path }).catch(() => null).then((dataUri) => {
    backendThumbCache.set(next.key, dataUri || 'none');
    if (dataUri) backendThumbElements(next.key).forEach((el) => { el.outerHTML = `<img class="thumb-img" src="${escapeAttribute(dataUri)}" alt="">`; });
    backendThumbBusy = false;
    pumpBackendThumbs();
  });
}
function renderThumbImage(entry) {
  if (entry.kind !== 'file') return icon(entry.kind);
  const command = backendThumbCommand(entry.name);
  if (command) {
    const key = `${entry.path}|${entry.modified}`;
    const cached = backendThumbCache.get(key);
    if (cached === 'none') return icon(entry.kind);
    if (cached) return `<img class="thumb-img" src="${escapeAttribute(cached)}" alt="">`;
    // The pump runs after render() has put the placeholder in the DOM.
    if (!backendThumbCache.has(key)) { backendThumbCache.set(key, null); backendThumbQueue.push({ key, command, path: entry.path }); queueMicrotask(pumpBackendThumbs); }
    return `<span class="file-icon" data-backend-thumb="${escapeAttribute(key)}">·</span>`;
  }
  // A frame a second in rather than the very first one, which is often black.
  if (VIDEO_EXTENSIONS.has(extension(entry.name).toLowerCase())) return `<video class="thumb-img" src="${escapeAttribute(convertFileSrc(entry.path))}#t=1" preload="metadata" muted playsinline></video>`;
  return isImageFile(entry.name) ? `<img class="thumb-img" src="${escapeAttribute(convertFileSrc(entry.path))}" alt="" loading="lazy">` : icon(entry.kind);
}
const extensionIconCache = new Map();
function fetchExtensionIcon(ext) {
  extensionIconCache.set(ext, null);
  invoke('get_extension_icon', { extension: ext }).then((dataUri) => {
    extensionIconCache.set(ext, dataUri || 'none');
    if (dataUri) document.querySelectorAll(`[data-icon-ext="${ext}"]`).forEach((el) => { el.outerHTML = `<img class="file-type-icon" src="${escapeAttribute(dataUri)}" alt="">`; });
  }).catch(() => extensionIconCache.set(ext, 'none'));
}
function renderFileIcon(entry) {
  if (entry.kind !== 'file') return icon(entry.kind);
  const ext = extension(entry.name).toLowerCase();
  const cached = extensionIconCache.get(ext);
  if (cached && cached !== 'none') return `<img class="file-type-icon" src="${escapeAttribute(cached)}" alt="">`;
  if (!extensionIconCache.has(ext)) fetchExtensionIcon(ext);
  return `<span class="file-icon" data-icon-ext="${escapeAttribute(ext)}">·</span>`;
}
function escapeAttribute(value) { return String(value).replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); }
function folderIcon(path) { const name = path.replace(/[\\/]+$/, '').split(/[\\/]/).pop()?.toLowerCase() || ''; return ['pictures', 'documents', 'downloads', 'desktop', 'music', 'videos'].includes(name) ? name : name === 'movies' ? 'videos' :['gdrive', 'google drive', 'my drive'].includes(name) ? 'gdrive' : 'folder'; }
function folderLabel(path) { return path.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || 'Home'; }
function pathSegments(path) {
  const parts = path.replace(/[\\/]+$/, '').split(/[\\/]/).filter(Boolean);
  if (!isWindows && path.startsWith('/')) {
    let acc = '';
    return parts.map((part) => { acc += `/${part}`; return { label: part, path: acc }; });
  }
  let acc = '';
  return parts.map((part, i) => {
    acc = i === 0 ? (/^[A-Za-z]:$/.test(part) ? `${part}\\` : part) : (acc.endsWith('\\') ? `${acc}${part}` : `${acc}\\${part}`);
    return { label: part, path: acc };
  });
}
function joinPath(base, rest) { return [base.replace(/[\\/]+$/, ''), ...rest.split(/[\\/]+/).filter(Boolean)].join(SEP); }
async function resolveFolderPath(path) { return path.startsWith('~') ? joinPath(await homeDir(), path.slice(1)) : path; }
// Each bookmark is { path, name?, icon? }; name defaults to the folder name and icon to a guess from it.
// A path can start with a folder looked up on this computer, such as {pictures} or {onedrive}; one that
// finds several folders (a OneDrive per account) becomes a bookmark for each. Bookmarks whose folder
// doesn't exist on this computer are left out. The list is the per-user folders.json (Help > Edit Favorite
// Folders); if that doesn't parse, the bundled default is used instead.
async function configuredFolders() {
  const { contents } = await invoke('read_folders_config');
  try { return parseFoldersConfig(contents); } catch (error) {
    console.error('folders.json is invalid, using the default', error);
    return parseFoldersConfig(await invoke('default_folders_config'));
  }
}
function parseFoldersConfig(contents) {
  const folders = JSON.parse(contents);
  if (!Array.isArray(folders)) throw new Error('The file must contain a JSON array of folders.');
  const bad = folders.findIndex((folder) => typeof folder?.path !== 'string');
  if (bad !== -1) throw new Error(`Folder ${bad + 1} needs a "path" string.`);
  return folders;
}
async function loadBookmarks() {
  const [known, folders] = await Promise.all([invoke('known_folders'), configuredFolders()]);
  const bookmarks = (await Promise.all(folders.map(async ({ path, name, icon }) => {
    const [, key, rest] = path.match(/^\{(\w+)\}(.*)$/) || [];
    const paths = key ? (known[key] || []).map((base) => joinPath(base, rest)) : [await resolveFolderPath(path)];
    return paths.map((resolved) => ({ path: resolved, label: name || folderLabel(resolved), icon: icon || (key && !rest ? key : folderIcon(resolved)) }));
  }))).flat();
  const exists = await invoke('existing_folders', { paths: bookmarks.map(({ path }) => path) });
  return bookmarks.filter((_, index) => exists[index]);
}
function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}
function persistStored() { localStorage.setItem('rove-explorer-state', JSON.stringify(stored)); }
function remember(path, name) { stored[path] = { ...stored[path], name, time: Date.now() }; persistStored(); }
// Scroll events fire rapidly, so the in-memory value updates immediately and the write to storage is debounced.
let scrollPersistTimer = null;
// Positions are kept per view because Details rows and Thumbnail cards scroll to different offsets.
function rememberedScroll(path, view) { return stored[path]?.scroll?.[view]; }
function rememberScroll(path, view, scrollTop) {
  if (!path || rememberedScroll(path, view) === scrollTop) return;
  stored[path] = { ...stored[path], scroll: { ...stored[path]?.scroll, [view]: scrollTop }, time: Date.now() };
  clearTimeout(scrollPersistTimer);
  scrollPersistTimer = setTimeout(persistStored, 300);
}
function firstEntryId(entries) { return entries[0]?.id || null; }
function toSelection(id) { return id ? [id] : []; }
function rememberedOrFirst(entries, rememberedName) { const remembered = rememberedName && entries.find((entry) => entry.name === rememberedName); return toSelection(remembered ? remembered.id : firstEntryId(entries)); }
function validOrFirst(entries, currentSelected) { const ids = (Array.isArray(currentSelected) ? currentSelected : toSelection(currentSelected)).filter((id) => entries.some((entry) => entry.id === id)); return ids.length ? ids : toSelection(firstEntryId(entries)); }
// Single click / single-step arrow move: collapses to one item and resets both range endpoints.
function selectSingle(tab, id) { tab.selected = toSelection(id); tab.focusId = id || null; tab.anchorId = id || null; }
// Shift-click / shift-arrow: highlights every item between the fixed anchor and the new focus.
function selectRange(tab, entries, anchorId, focusId) {
  const anchorIndex = entries.findIndex((entry) => entry.id === anchorId);
  const focusIndex = entries.findIndex((entry) => entry.id === focusId);
  if (anchorIndex === -1 || focusIndex === -1) { selectSingle(tab, focusId); return; }
  const [start, end] = anchorIndex <= focusIndex ? [anchorIndex, focusIndex] : [focusIndex, anchorIndex];
  tab.selected = entries.slice(start, end + 1).map((entry) => entry.id);
  tab.focusId = focusId;
  tab.anchorId = anchorId;
}
function saveSession() { localStorage.setItem('rove-explorer-session', JSON.stringify({ panes: state.panes.map((pane) => ({ activeTab: pane.activeTab, tabs: pane.tabs.map(({ path, label, selected }) => ({ path, label, selected })) })) })); }
function isEditing(index, entry) { return state.editing?.pane === index && state.editing.entryId === entry.id; }

// Column sort for folder listings: folders always stay above files, and ties fall back to the name.
const sortState = (() => { try { return JSON.parse(localStorage.getItem('rove-explorer-sort')) || { key: 'name', dir: 1 }; } catch { return { key: 'name', dir: 1 }; } })();
const SORT_VALUES = { name: (entry) => entry.name.toLowerCase(), extension: (entry) => entry.extension.toLowerCase(), size: (entry) => entry.bytes ?? 0, modified: (entry) => (entry.modified === '—' ? -1 : Number(entry.modified)) };
function compareValues(left, right) { return typeof left === 'number' ? left - right : left.localeCompare(right); }
function sortEntries(entries) {
  const value = SORT_VALUES[sortState.key] || SORT_VALUES.name;
  return [...entries].sort((left, right) => (left.kind === 'folder' ? 0 : 1) - (right.kind === 'folder' ? 0 : 1) || sortState.dir * (compareValues(value(left), value(right)) || compareValues(SORT_VALUES.name(left), SORT_VALUES.name(right))));
}
function setSort(key) {
  sortState.dir = sortState.key === key ? -sortState.dir : 1; sortState.key = key;
  localStorage.setItem('rove-explorer-sort', JSON.stringify(sortState));
  for (const pane of state.panes) for (const tab of pane.tabs) if (tab.kind === 'folder') tab.entries = sortEntries(tab.entries);
  render();
}
function sortHeader(key, label) {
  const active = sortState.key === key;
  return `<button class="sort-header ${active ? 'active' : ''}" data-sort-key="${key}" title="Sort by ${label.toLowerCase()}">${label}<span class="sort-arrow" aria-hidden="true">${active ? (sortState.dir > 0 ? '▲' : '▼') : ''}</span></button>`;
}

async function load(path) {
  const entries = await invoke('read_directory', { path, showHidden: state.showHidden });
  return sortEntries(entries.map(decorateEntry));
}
function decorateEntry(entry) { return { ...entry, id: entry.path, extension: entry.kind === 'file' ? extension(entry.name) : '—', displayName: entry.kind === 'file' ? displayName(entry.name) : entry.name }; }
// The items a pane's selection and keyboard work on: the search results in Search view, else the folder listing.
function tabEntries(tab) { return tab?.view === 'search' ? (tab.searchState?.matches || []) : (tab?.entries || []); }
// Remembers the selected name for the folder, except in Search view, where the item may sit in a subfolder.
function rememberSelection(tab, entry) { if (tab.path && tab.view !== 'search') remember(tab.path, entry.name); }

// Also refreshes the drive bookmarks, so they pick up drives added or removed since the last listing.
async function loadDrives() {
  const drives = (await invoke('list_drives')).map((entry) => ({ ...entry, id: entry.path, displayName: entry.name, extension: '—' }));
  state.drives = drives.map(({ name, path }) => ({ name, path }));
  return drives;
}

function render() {
  const activeTab = currentTab(state.panes[state.activePane]);
  const pathValue = activeTab?.path || '';
  const toolbar = state.folders.map((folder) => `<button class="quick-folder" data-folder-path="${escapeAttribute(folder.path)}" title="Open ${escapeAttribute(folder.path)}"><img src="/folder-icons/${escapeAttribute(folder.icon)}.svg" alt=""><span>${escapeAttribute(folder.label)}</span></button>`).join('');
  const driveBookmarks = state.drives.length ? `<div class="drive-bookmarks">${state.drives.map((drive) => `<button class="drive-bookmark" data-folder-path="${escapeAttribute(drive.path)}" title="Open ${escapeAttribute(drive.path)}"><svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.5" y="4.5" width="13" height="7" rx="1"/><path d="M4 9.5h1.5"/><circle cx="11.5" cy="8" r=".6"/></svg><span>${escapeAttribute(drive.name)}</span></button>`).join('')}</div>` : '';
  const canGoBack = (activeTab?.historyIndex ?? 0) > 0;
  const canGoForward = !!activeTab && activeTab.historyIndex < (activeTab.history?.length ?? 1) - 1;
  const navHistory = `<button class="nav-history-button" data-go-back title="Back" aria-label="Go back" ${canGoBack ? '' : 'disabled'}>←</button><button class="nav-history-button" data-go-forward title="Forward" aria-label="Go forward" ${canGoForward ? '' : 'disabled'}>→</button><span class="toolbar-separator" aria-hidden="true"></span>`;
  const otherPane = 1 - state.activePane;
  // A file with an arrow leading away from it, mirrored when moving from the right pane to the left.
  const moveIcon = `<svg class="move-icon ${state.activePane === 1 ? 'is-leftward' : ''}" viewBox="0 0 26 16" aria-hidden="true"><path class="move-icon-file" d="M2 1.5h5.5l3 3v10h-8.5z"/><path d="M7.5 1.5v3h3"/><path d="M14 8h10M20.5 4.5 24 8l-3.5 3.5"/></svg>`;
  const moveTitle = canMoveToOtherPane() ? `Move selected items to ${currentTab(state.panes[otherPane]).path}` : 'Move selected items to the other pane';
  const moveButton = `<button class="nav-history-button move-button" data-move-to-other-pane title="${escapeAttribute(moveTitle)}" aria-label="Move selected items to the other pane" ${canMoveToOtherPane() ? '' : 'disabled'}>${moveIcon}</button><span class="toolbar-separator" aria-hidden="true"></span>`;
  const scrollPositions = new Map();
  document.querySelectorAll('.pane').forEach((el) => { const scrollable = el.querySelector('.table-wrap'); if (scrollable) scrollPositions.set(el.dataset.pane, { key: scrollable.dataset.scrollKey, top: scrollable.scrollTop }); });
  // Re-rendering replaces the rename box (e.g. when a file-watcher refresh lands right after New Folder),
  // so carry its focus, typed text and cursor position over to the new one.
  const oldRename = document.querySelector('[data-rename]');
  const renameSnapshot = oldRename && { value: oldRename.value, start: oldRename.selectionStart, end: oldRename.selectionEnd, focused: document.activeElement === oldRename };
  // WebView2 fires blur on a focused input as it is removed; mark the old box so its blur handler
  // doesn't treat being replaced as the user finishing (or abandoning) the rename.
  if (oldRename) oldRename.dataset.replaced = 'true';
  // The search box is replaced too, so carry over its focus and cursor the same way.
  const oldSearch = document.activeElement?.matches?.('[data-search-input]') ? document.activeElement : null;
  const searchSnapshot = oldSearch && { pane: oldSearch.closest('.pane')?.dataset.pane, value: oldSearch.value, start: oldSearch.selectionStart, end: oldSearch.selectionEnd };
  document.querySelector('#app').innerHTML = `<header class="topbar"><div class="brand"><span class="brand-mark">R</span><span>ROVE EXPLORER</span><span class="version-tag">v${APP_VERSION}</span></div><input class="location-input" id="location-input" value="${escapeAttribute(pathValue)}" placeholder="Enter a folder path" aria-label="Current folder path"><label class="hidden-toggle"><input type="checkbox" id="hidden-toggle" ${state.showHidden ? 'checked' : ''}><span>Show hidden</span></label><button class="help-button" data-help-menu title="Help" aria-label="Help">Help</button></header><main class="workspace"><nav class="folder-toolbar" aria-label="Favorite folders">${navHistory}${moveButton}${toolbar}${driveBookmarks}</nav><section class="panes" aria-label="File panes">${state.panes.map(renderPane).join('')}</section></main>`;
  const newRename = renameSnapshot && document.querySelector('[data-rename]');
  if (newRename) { newRename.value = renameSnapshot.value; if (renameSnapshot.focused) { newRename.focus(); newRename.setSelectionRange(renameSnapshot.start, renameSnapshot.end); } }
  bindEvents();
  const newSearch = searchSnapshot && document.querySelector(`.pane[data-pane="${searchSnapshot.pane}"] [data-search-input]`);
  if (newSearch) { newSearch.value = searchSnapshot.value; newSearch.focus(); newSearch.setSelectionRange(searchSnapshot.start, searchSnapshot.end); }
  state.panes.forEach((pane, index) => {
    const searchState = currentTab(pane)?.searchState;
    if (!searchState?.autoFocus || searchState.status !== 'done') return;
    searchState.autoFocus = false;
    const input = document.querySelector(`.pane[data-pane="${index}"] [data-search-input]`);
    if (input && !newSearch) { input.focus(); input.setSelectionRange(input.value.length, input.value.length); }
  });
  document.querySelectorAll('.pane').forEach((el) => {
    const scrollable = el.querySelector('.table-wrap');
    if (!scrollable) return;
    const previous = scrollPositions.get(el.dataset.pane);
    const folderPath = scrollable.dataset.scrollPath;
    if (previous?.key === scrollable.dataset.scrollKey) {
      // Same folder and view as before this render: keep where the user was.
      scrollable.scrollTop = previous.top;
    } else {
      // A different folder (or view) is now showing: restore that folder's remembered position,
      // or bring the remembered selection into view if the folder has none yet.
      const savedTop = folderPath ? rememberedScroll(folderPath, scrollable.dataset.scrollView) : undefined;
      if (savedTop !== undefined) scrollable.scrollTop = savedTop;
      else el.querySelector('.file-row.focused, .thumb-card.focused')?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    }
    if (folderPath) scrollable.addEventListener('scroll', () => rememberScroll(folderPath, scrollable.dataset.scrollView, scrollable.scrollTop), { passive: true });
  });
  if (scrollSelectionIntoView) {
    const selectedRow = document.querySelector(`.pane[data-pane="${state.activePane}"] .file-row.focused, .pane[data-pane="${state.activePane}"] .thumb-card.focused, .pane[data-pane="${state.activePane}"] .tree-row.selected`);
    selectedRow?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    scrollSelectionIntoView = false;
  }
  syncWatchedPaths();
  prefetchOpenWithApps();
}

function syncWatchedPaths() {
  const visibleTabs = state.panes.map(currentTab).filter((tab) => tab?.path);
  const paths = [...new Set(visibleTabs.map((tab) => tab.path))];
  // Tree view totals and search results cover every subfolder, so those roots are watched recursively.
  const recursivePaths = [...new Set(visibleTabs.filter((tab) => tab.view === 'tree' || tab.view === 'search').map((tab) => tab.path))];
  const key = `${[...paths].sort().join('|')}#${[...recursivePaths].sort().join('|')}`;
  if (key === lastWatchedKey) return;
  lastWatchedKey = key;
  invoke('set_watched_paths', { paths, recursivePaths }).catch((error) => console.error('set_watched_paths failed', error));
}

// Tree and Search views rescan automatically when anything under their root changes, but never sooner
// than TREE_RESCAN_INTERVAL_MS after the previous scan finished (scans of big trees are expensive).
const TREE_RESCAN_INTERVAL_MS = 15000;
const TREE_RESCAN_SETTLE_MS = 1000;
function isWithin(changedPath, rootPath) {
  const changed = normalizePath(changedPath); const root = normalizePath(rootPath);
  return changed === root || changed.startsWith(`${root}/`);
}

// The scan behind a tab's Tree or Search view, if it's showing one.
function scanStateOf(tab) { return tab?.view === 'tree' ? tab.treeState : tab?.view === 'search' ? tab.searchState : null; }

function scheduleRescans(changedPath) {
  state.panes.forEach((pane, index) => {
    const tab = currentTab(pane);
    const scan = scanStateOf(tab);
    if (scan && tab.path && isWithin(changedPath, tab.path)) scheduleRescan(index, scan);
  });
}

function scheduleRescan(index, treeState) {
  // A change during a scan may or may not have been seen by it, so rescan once it finishes.
  if (treeState.status === 'loading' || treeState.rescanning) { treeState.dirty = true; return; }
  if (treeState.status !== 'done' || treeState.rescanTimer) return;
  const sinceLastScan = Date.now() - (treeState.lastScanEnd || 0);
  const delay = Math.max(TREE_RESCAN_SETTLE_MS, TREE_RESCAN_INTERVAL_MS - sinceLastScan);
  treeState.rescanTimer = setTimeout(() => {
    treeState.rescanTimer = null;
    const tab = currentTab(state.panes[index]);
    // Skip if the pane has since moved on to another folder, view or tab.
    if (scanStateOf(tab) !== treeState) return;
    if (tab.view === 'tree') rescanTree(tab); else rescanSearch(tab);
  }, delay);
}

// Unlike startTreeView, keeps showing the current tree (and its expanded folders and selection)
// until the new scan arrives.
function rescanTree(tab) {
  const treeState = tab.treeState;
  const requestId = `tree-rescan-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  treeState.requestId = requestId; treeState.rescanning = true; treeState.dirty = false;
  invoke('compute_tree_stats', { requestId, path: tab.path }).catch(() => {
    if (treeState.requestId === requestId) treeState.rescanning = false;
  });
}

function treeContains(node, path) { return node.path === path || node.children.some((child) => treeContains(child, path)); }

function scheduleFolderReload(changedPath) {
  const key = normalizePath(changedPath);
  clearTimeout(fsChangeTimers.get(key));
  fsChangeTimers.set(key, setTimeout(async () => {
    fsChangeTimers.delete(key);
    let changed = false;
    for (const pane of state.panes) {
      const tab = currentTab(pane);
      if (!tab?.path || normalizePath(tab.path) !== key) continue;
      await reloadTabEntries(tab);
      changed = true;
    }
    if (changed) { saveSession(); render(); }
  }, 200));
}

// Re-reads a tab's folder, keeping the same items selected and focused by name.
async function reloadTabEntries(tab) {
  const selectedNames = new Set(tab.entries.filter((entry) => tab.selected?.includes(entry.id)).map((entry) => entry.name));
  const focusName = tab.entries.find((entry) => entry.id === tab.focusId)?.name;
  tab.entries = await load(tab.path);
  tab.selected = tab.entries.filter((entry) => selectedNames.has(entry.name)).map((entry) => entry.id);
  tab.focusId = focusName ? tab.entries.find((entry) => entry.name === focusName)?.id || null : null;
  tab.anchorId = tab.focusId;
}

// Background tabs aren't watched, so a tab brought to the front catches up on anything that changed meanwhile.
async function refreshActivatedTab(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.path) return;
  if (scanStateOf(tab)) scheduleRescan(index, scanStateOf(tab));
  try { await reloadTabEntries(tab); } catch { return; } // folder may be gone; keep what it showed
  if (currentTab(state.panes[index]) !== tab) return;
  saveSession(); render();
}

function keys(...names) { return names.map((name) => `<kbd>${name}</kbd>`).join('+'); }
const SHORTCUTS = [
  ['Navigate', isMac ? [
    [`${keys('Return')} or ${keys('⌘', '↓')}`, 'Open the selected item'],
    [`${keys('⌫')} or ${keys('⌘', '↑')}`, 'Go up a level'],
    [`${keys('⌘', '[')} ${keys('⌘', ']')}`, 'Back / forward'],
  ] : [
    [keys('Enter'), 'Open the selected item'],
    [keys('Backspace'), 'Go up a level'],
    [`${keys('Alt', '←')} ${keys('Alt', '→')}`, 'Back / forward'],
  ]],
  ['Select', [
    [`${keys('↑')} ${keys('↓')}`, 'Move the selection (in Thumbnails also ← →)'],
    [`${keys('Home')} ${keys('End')}`, 'First / last item'],
    [`${keys('Shift')} + arrow, ${keys('Home')} or ${keys('End')}`, 'Extend the selection'],
    [`${keys(isMac ? '⌘' : 'Ctrl')}+click`, 'Add or remove one item'],
    [`${keys('Shift')}+click`, 'Select a range'],
    ['Type a name', 'Jump to the item starting with it'],
  ]],
  ['Edit', [
    [keys('F2'), 'Rename'],
    [isMac ? keys('⌘', '⌫') : keys('Delete'), `Move to the ${TRASH_NAME}`],
    [keys(isMac ? '⌘' : 'Ctrl', 'Z'), 'Undo the last delete'],
    [`${keys(isMac ? '⌘' : 'Ctrl', 'C')} ${keys(isMac ? '⌘' : 'Ctrl', 'X')} ${keys(isMac ? '⌘' : 'Ctrl', 'V')}`, 'Copy / cut / paste'],
  ]],
  ['View', VIEW_MODES.map((mode, i) => [keys(isMac ? '⌥' : 'Alt', String(i + 1)), `${mode.label}`])],
  ['Tabs', [['Middle-click a tab', 'Close it']]],
  ['Help', [[keys('F1'), 'Show this list']]],
];
const APP_DESCRIPTION = 'A focused two-pane file explorer: side-by-side panes with tabs, favorite folders, thumbnails, a folder-size tree view, file-name search, and copy, cut and paste that work with Explorer and Finder.';

// Dialogs live outside #app so re-renders (e.g. a folder refreshing behind one) leave them alone.
const dialogRoot = document.body.appendChild(document.createElement('div'));
function isDialogOpen() { return !!dialogRoot.firstChild; }
function closeDialog() { dialogRoot.innerHTML = ''; }
function showDialog(title, body) {
  dialogRoot.innerHTML = `<div class="dialog-backdrop" data-dialog-backdrop><section class="dialog" role="dialog" aria-modal="true" aria-label="${escapeAttribute(title)}"><header class="dialog-head"><h2>${escapeAttribute(title)}</h2><button class="dialog-close" data-dialog-close title="Close" aria-label="Close">×</button></header><div class="dialog-body">${body}</div></section></div>`;
  dialogRoot.querySelector('[data-dialog-backdrop]').addEventListener('click', (event) => { if (event.target === event.currentTarget) closeDialog(); });
  dialogRoot.querySelector('[data-dialog-close]').addEventListener('click', closeDialog);
  dialogRoot.querySelectorAll('[data-external-link]').forEach((link) => link.addEventListener('click', (event) => { event.preventDefault(); openUrl(link.dataset.externalLink).catch((error) => console.error('openUrl failed', error)); }));
  dialogRoot.querySelector('[data-dialog-close]').focus();
}
function showShortcuts() {
  showDialog('Keyboard Shortcuts', SHORTCUTS.map(([section, rows]) => `<h3>${section}</h3><dl class="shortcut-list">${rows.map(([combo, action]) => `<dt>${combo}</dt><dd>${action}</dd>`).join('')}</dl>`).join(''));
}
function showAbout() {
  const github = 'https://github.com/mhenry1384';
  showDialog('About Rove Explorer', `<div class="about-head"><span class="brand-mark">R</span><div><strong>Rove Explorer</strong><span class="version-tag">v${APP_VERSION}</span></div></div><p>${APP_DESCRIPTION}</p><p><a href="${github}" data-external-link="${github}">${github}</a></p><p>Licensed under the GNU General Public License v3 (GPLv3).</p>`);
}
async function showFoldersEditor() {
  let config;
  try { config = await invoke('read_folders_config'); } catch (error) { await message(String(error), { title: 'Edit Favorite Folders failed', kind: 'error' }); return; }
  showDialog('Edit Favorite Folders', `<p class="folders-editor-path">${escapeAttribute(config.path)}</p><textarea class="folders-editor" data-folders-text spellcheck="false" aria-label="folders.json">${escapeAttribute(config.contents)}</textarea><p class="folders-editor-error" data-folders-error></p><div class="dialog-actions"><button data-folders-default>Restore Default</button><span></span><button data-dialog-cancel>Cancel</button><button class="primary" data-folders-save>Save</button></div>`);
  const text = dialogRoot.querySelector('[data-folders-text]');
  const errorLine = dialogRoot.querySelector('[data-folders-error]');
  dialogRoot.querySelector('[data-dialog-cancel]').addEventListener('click', closeDialog);
  // Restoring only fills in the editor; nothing changes on disk until Save.
  dialogRoot.querySelector('[data-folders-default]').addEventListener('click', async () => { text.value = await invoke('default_folders_config'); errorLine.textContent = ''; text.focus(); });
  const save = dialogRoot.querySelector('[data-folders-save]');
  text.addEventListener('keydown', (event) => { if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') { event.preventDefault(); save.click(); } });
  save.addEventListener('click', async () => {
    try { parseFoldersConfig(text.value); } catch (error) { errorLine.textContent = `Not saved: ${error.message}`; text.focus(); return; }
    try { await invoke('write_folders_config', { contents: text.value }); } catch (error) { errorLine.textContent = `Not saved: ${error}`; return; }
    closeDialog();
    state.folders = await loadBookmarks().catch((error) => { console.error('loadBookmarks failed', error); return state.folders; });
    render();
  });
  text.focus();
}
async function showHelpMenu(button) {
  const rect = button.getBoundingClientRect();
  const items = await Promise.all([MenuItem.new({ text: 'Keyboard Shortcuts', accelerator: 'F1', action: showShortcuts }), MenuItem.new({ text: 'Edit Favorite Folders…', action: showFoldersEditor }), MenuItem.new({ text: 'About Rove Explorer', action: showAbout })]);
  await (await Menu.new({ items })).popup(new LogicalPosition(rect.left, rect.bottom));
}
// Characters the OS won't accept in a file name (macOS reserves ':' because Finder displays it as '/').
const INVALID_NAME_CHARS = isWindows ? /[\\/:*?"<>|]/ : isMac ? /[/:]/ : /\//;
function textEditorLabel() { return platformName === 'macos' ? 'Edit in TextEdit' : 'Edit in Notepad'; }
const TEXT_EDITOR_EXTENSIONS = new Set(['.txt', '.md', '.lst', '.bat', '.html', '.htm']);
function canEditInTextEditor(pane) {
  const entries = selectedFileEntries(pane);
  return (platformName === 'windows' || platformName === 'macos') && entries.length > 0 && entries.every((entry) => entry.kind === 'file' && TEXT_EDITOR_EXTENSIONS.has(extension(entry.name).toLowerCase()));
}
const OPEN_WITH_OTHER_LABEL = isMac ? 'Other…' : 'Choose another app';
// The apps Open With offers depend only on the file's extension, so they're kept per extension:
// the list once it has loaded, or the pending request until then.
const openWithCache = new Map();
function openWithKey(entry) { return extension(entry.name).toLowerCase(); }
function singleSelectedFile(index) { const entries = selectedFileEntries(index); return entries.length === 1 && entries[0].kind === 'file' ? entries[0] : null; }

// Looks the apps up as soon as a single file is selected, so they're usually ready by the time
// the context menu opens (a native menu can't change once it's showing).
function prefetchOpenWithApps() {
  if (!(platformName === 'windows' || platformName === 'macos')) return;
  state.panes.forEach((_, index) => {
    const entry = singleSelectedFile(index);
    if (!entry || openWithCache.has(openWithKey(entry))) return;
    const key = openWithKey(entry);
    openWithCache.set(key, invoke('list_open_with_apps', { path: entry.path }).catch(() => []).then((apps) => { openWithCache.set(key, apps); return apps; }));
  });
}

function dataUriBytes(uri) { return Uint8Array.from(atob(uri.slice(uri.indexOf(',') + 1)), (char) => char.charCodeAt(0)); }

function openWith(path, command, args) {
  invoke(command, { path, ...args }).catch((error) => message(String(error), { title: 'Open With failed', kind: 'error' }));
}

// Apps still loading are left out; "Choose another app" is always there.
async function openWithSubmenu(entry) {
  const cached = openWithCache.get(openWithKey(entry));
  const apps = Array.isArray(cached) ? cached : [];
  const appItems = await Promise.all(apps.map((app) => {
    const action = () => openWith(entry.path, 'open_with_app', { appId: app.id });
    return app.icon ? IconMenuItem.new({ text: app.name, icon: dataUriBytes(app.icon), action }) : MenuItem.new({ text: app.name, action });
  }));
  const other = await MenuItem.new({ text: OPEN_WITH_OTHER_LABEL, action: () => openWith(entry.path, 'open_with_other_app', {}) });
  const items = appItems.length ? [...appItems, await PredefinedMenuItem.new({ item: 'Separator' }), other] : [other];
  return Submenu.new({ text: 'Open With', items });
}

// What a context menu's Properties item describes: the clicked selection, the clicked tree folder,
// or (for a right-click on empty space) the folder the pane is showing.
function propertiesPaths(menu) {
  if (menu.kind === 'tree') return [menu.path];
  if (menu.kind === 'pane') { const path = currentTab(state.panes[menu.pane])?.path; return path ? [path] : []; }
  return selectedFileEntries(menu.pane).map((entry) => entry.path);
}

// Filled in at startup when LockHunter is installed ({ icon }), since a native menu can't wait on it.
let lockHunter = null;

// The one item LockHunter would inspect from this menu, and whether it's a folder.
function lockHunterTarget(menu) {
  if (menu.kind === 'tree') return { path: menu.path, folder: true };
  if (menu.kind === 'pane') { const path = currentTab(state.panes[menu.pane])?.path; return path ? { path, folder: true } : null; }
  const entries = selectedFileEntries(menu.pane);
  return entries.length === 1 && entries[0].kind !== 'drive' ? { path: entries[0].path, folder: entries[0].kind === 'folder' } : null;
}

function lockHunterMenuItem(menu) {
  const target = lockHunter && lockHunterTarget(menu);
  if (!target) return null;
  const text = `What is Locking this ${target.folder ? 'Folder' : 'File'}?`;
  const action = () => invoke('open_in_lockhunter', { path: target.path }).catch((error) => message(String(error), { title: 'LockHunter failed', kind: 'error' }));
  return lockHunter.icon ? IconMenuItem.new({ text, icon: dataUriBytes(lockHunter.icon), action }) : MenuItem.new({ text, action });
}

function showProperties(menu) {
  const paths = propertiesPaths(menu);
  if (paths.length) invoke('show_properties', { paths }).catch((error) => console.error('show_properties failed', error));
}

// The right-click menu is the OS's own, so it can extend past the window and the OS keeps it on screen.
// `menu` is { pane, kind: 'pane' | 'file' | 'tree', entryId?, path? }; 'file' also covers search results.
async function showContextMenu(menu) {
  const { kind, pane } = menu;
  const tab = currentTab(state.panes[pane]);
  const item = (text, action) => MenuItem.new({ text, action });
  const separator = () => PredefinedMenuItem.new({ item: 'Separator' });
  const items = [];
  if (kind === 'pane') {
    // New items start out being renamed in the folder listing, which Search view doesn't show.
    if (tab?.path && tab.view !== 'search') items.push(item('New Folder', () => createNewEntry(pane, 'create_folder')), item('New File', () => createNewEntry(pane, 'create_file')));
    items.push(item('Paste', () => pasteIntoPane(pane)));
  } else {
    if (kind === 'file') {
      const clicked = tab?.view === 'search' && tabEntries(tab).find((candidate) => candidate.id === menu.entryId);
      if (clicked) items.push(item('Open File Location', () => openFileLocation(pane, clicked)), separator());
      const single = singleSelectedFile(pane);
      if (single && (platformName === 'windows' || platformName === 'macos')) items.push(openWithSubmenu(single), separator());
      items.push(item('Copy', () => copySelected(pane)), item('Cut', () => cutSelected(pane)));
      const entry = tabEntries(tab).find((candidate) => candidate.id === menu.entryId);
      if (entry && entry.kind !== 'drive') items.push(item('Rename', () => startRename(pane, entry)));
      if (canEditInTextEditor(pane)) items.push(item(textEditorLabel(), () => editSelectedInTextEditor(pane)));
      if (tab?.path) items.push(item('Compress', () => compressSelected(pane, menu.entryId)));
      items.push(separator());
    }
    items.push(item('Delete', () => (kind === 'tree' ? deleteTreeNode(pane, menu.path) : deleteSelected(pane))));
  }
  const lockHunterItem = lockHunterMenuItem(menu);
  if (lockHunterItem) items.push(separator(), lockHunterItem);
  if ((platformName === 'windows' || platformName === 'macos') && propertiesPaths(menu).length) items.push(separator(), item(platformName === 'macos' ? 'Get Info' : 'Properties', () => showProperties(menu)));
  const nativeMenu = await Menu.new({ items: await Promise.all(items) });
  await nativeMenu.popup();
}

function renderTreeNode(node, depth, treeState, isRoot) {
  const hasChildren = node.children.length > 0;
  const isExpanded = treeState.expanded.has(node.path);
  const isSelected = treeState.selected === node.path;
  const row = `<div class="tree-row ${isSelected ? 'selected' : ''}" data-tree-node="${escapeAttribute(node.path)}" style="padding-left:${depth * 18 + 12}px"><span class="tree-caret ${hasChildren ? 'has-children' : ''}" data-tree-caret>${hasChildren ? (isExpanded ? '▾' : '▸') : ''}</span><span class="tree-name">${escapeAttribute(isRoot ? node.path : node.name)}</span><span class="tree-stat">${node.fileCount.toLocaleString()} file${node.fileCount === 1 ? '' : 's'}</span><span class="tree-stat">${formatBytes(node.totalSize)}</span></div>`;
  const childrenHtml = hasChildren && isExpanded ? node.children.map((child) => renderTreeNode(child, depth + 1, treeState, false)).join('') : '';
  return row + childrenHtml;
}

// Must match TREE_MAX_FILES in src-tauri/src/lib.rs.
const TREE_MAX_FILES = 200000;

function treeProgressText(treeState) {
  return `Scanning… ${treeState.scannedFolders.toLocaleString()} folders, ${treeState.scannedFiles.toLocaleString()} files so far`;
}

function renderTreeBody(tab) {
  const treeState = tab.treeState;
  if (!treeState) return '';
  if (treeState.status === 'loading') {
    return `<div class="tree-progress"><div class="spinner" aria-hidden="true"></div><p data-tree-progress="${escapeAttribute(treeState.requestId)}">${treeProgressText(treeState)}</p><button class="tree-cancel" data-tree-cancel>Cancel</button></div>`;
  }
  if (treeState.status === 'too-many') {
    return `<div class="tree-progress"><p>Too many files. Tree view is limited to ${TREE_MAX_FILES.toLocaleString()} files, and this folder has more than that.</p><button class="tree-cancel" data-tree-cancel>Close</button></div>`;
  }
  if (treeState.status === 'error') {
    return `<div class="tree-progress"><p>Couldn't compute folder stats.</p><button class="tree-cancel" data-tree-cancel>Close</button></div>`;
  }
  if (treeState.status === 'done' && treeState.tree) {
    return `<div class="tree-rows">${renderTreeNode(treeState.tree, 0, treeState, true)}</div>`;
  }
  return '';
}

function formatModified(entry) { return entry.modified === '—' ? '—' : new Date(Number(entry.modified) * 1000).toLocaleDateString('en', { month: 'short', day: 'numeric', year: 'numeric' }); }

function searchCountLabel(tab) {
  const searchState = tab?.searchState;
  if (searchState?.status !== 'done') return 'SEARCH';
  return `${searchState.total.toLocaleString()} ${searchState.total === 1 ? 'MATCH' : 'MATCHES'}`;
}

function renderSearchBody(tab, index, scrollAttrs) {
  const searchState = tab?.searchState;
  const message = (content) => `<div class="table-wrap tree-wrap" ${scrollAttrs}><div class="tree-progress">${content}</div></div>`;
  if (!searchState) return message('');
  if (searchState.status === 'loading') return message(`<div class="spinner" aria-hidden="true"></div><p data-tree-progress="${escapeAttribute(searchState.requestId)}">${treeProgressText(searchState)}</p><button class="tree-cancel" data-search-cancel>Cancel</button>`);
  if (searchState.status === 'too-many') return message(`<p>Too many files. Search is limited to ${TREE_MAX_FILES.toLocaleString()} files, and this folder has more than that.</p><button class="tree-cancel" data-search-cancel>Close</button>`);
  if (searchState.status === 'error') return message(`<p>Couldn't list the files in this folder.</p><button class="tree-cancel" data-search-cancel>Close</button>`);
  const { total, matches } = searchState;
  const summary = total === 0 ? 'No matching files'
    : total > SEARCH_MAX_RESULTS ? `${total.toLocaleString()} matching files · showing the first ${SEARCH_MAX_RESULTS}`
      : `${total.toLocaleString()} matching file${total === 1 ? '' : 's'}`;
  const bar = `<div class="search-bar"><input class="search-input" data-search-input value="${escapeAttribute(searchState.query)}" placeholder="Find files whose name contains… (* matches anything, e.g. P*.mov)" aria-label="Search file names" spellcheck="false" autocomplete="off"><span class="search-summary">${summary}</span></div>`;
  const root = tab.path.replace(/[\\/]+$/, '');
  const selected = tab.selected || [];
  const rows = matches.map((entry, entryIndex) => {
    const folder = parentPath(entry.path);
    const relative = folder.replace(/[\\/]+$/, '').slice(root.length).replace(/^[\\/]+/, '');
    const name = isEditing(index, entry) ? `<input class="rename-input" data-rename value="${escapeAttribute(entry.name)}" aria-label="Rename">` : `<strong>${escapeAttribute(entry.name)}</strong>`;
    return `<button class="file-row ${selected.includes(entry.id) ? 'selected' : ''} ${entry.id === tab.focusId ? 'focused' : ''} ${isCutEntry(entry) ? 'cut' : ''}" data-entry="${escapeAttribute(entry.id)}" data-index="${entryIndex}"><span class="file-name">${renderFileIcon(entry)}${name}</span><span class="search-folder" title="${escapeAttribute(folder)}">${relative ? escapeAttribute(relative) : '<em>this folder</em>'}</span><span>${entry.size}</span><span>${formatModified(entry)}</span></button>`;
  }).join('');
  const empty = searchState.paths.length ? '<div class="empty-state"><span>⌁</span><strong>No matching files</strong><small>Try different text.</small></div>' : '<div class="empty-state"><span>⌁</span><strong>No files in this folder</strong><small>There\'s nothing to search here.</small></div>';
  return `${bar}<div class="table-wrap details-wrap search-wrap" ${scrollAttrs}><div class="table-head"><span>NAME</span><span>FOLDER</span><span>SIZE</span><span>MODIFIED</span></div><div class="rows">${rows || empty}</div></div>`;
}

function isCutEntry(entry) { return !!(state.clipboard?.cut && state.clipboard.paths.includes(entry.path)); }

function renderThumbnails(entries, selected, focusId, index) {
  if (!entries.length) return '<div class="empty-state"><span>⌁</span><strong>This folder is empty</strong><small>Choose another location to keep moving.</small></div>';
  const cards = entries.map((entry, entryIndex) => {
    const editing = isEditing(index, entry);
    const thumb = renderThumbImage(entry);
    const label = editing
      ? `<input class="rename-input thumb-rename" data-rename value="${escapeAttribute(entry.name)}" aria-label="Rename">`
      : `<span class="thumb-name">${escapeAttribute(entry.displayName)}</span>`;
    return `<button class="thumb-card ${selected.includes(entry.id) ? 'selected' : ''} ${entry.id === focusId ? 'focused' : ''} ${isCutEntry(entry) ? 'cut' : ''}" data-entry="${entry.id}" data-index="${entryIndex}"><div class="thumb-box">${thumb}</div>${label}</button>`;
  }).join('');
  return `<div class="thumb-grid">${cards}</div>`;
}

function renderPane(pane, index) {
  const tab = currentTab(pane); const entries = tabEntries(tab); const selected = tab?.selected || []; const focusId = tab?.focusId; const isDriveRoot = tab?.kind === 'drives';
  const view = tab?.view || 'details';
  const headers = isDriveRoot ? '<span>NAME</span><span>TOTAL SIZE</span><span>FREE SPACE</span><span>FILE SYSTEM</span>' : `${sortHeader('name', 'NAME')}${sortHeader('extension', 'EXTENSION')}${sortHeader('size', 'SIZE')}${sortHeader('modified', 'MODIFIED')}`;
  const rows = entries.map((entry, entryIndex) => { const editing = isEditing(index, entry); const name = editing ? `<input class="rename-input" data-rename value="${escapeAttribute(entry.name)}" aria-label="Rename">` : `<strong>${entry.displayName}</strong>`; return `<button class="file-row ${isDriveRoot ? 'drive-row' : ''} ${selected.includes(entry.id) ? 'selected' : ''} ${entry.id === focusId ? 'focused' : ''} ${isCutEntry(entry) ? 'cut' : ''}" data-entry="${entry.id}" data-index="${entryIndex}"><span class="file-name">${renderFileIcon(entry)}${name}</span><span>${isDriveRoot ? entry.total : (editing && entry.kind === 'file' ? '' : entry.extension)}</span><span>${isDriveRoot ? entry.free : entry.size}</span><span>${isDriveRoot ? entry.fileSystem : (entry.modified === '—' ? '—' : new Date(Number(entry.modified) * 1000).toLocaleDateString('en', { month: 'short', day: 'numeric', year: 'numeric' }))}</span></button>`; }).join('') || '<div class="empty-state"><span>⌁</span><strong>This folder is empty</strong><small>Choose another location to keep moving.</small></div>';
  const breadcrumb = tab?.path
    ? pathSegments(tab.path).map((segment, segIndex) => `${segIndex > 0 ? '<span class="path-sep">/</span>' : ''}<button class="path-segment" data-breadcrumb="${escapeAttribute(segment.path)}">${escapeAttribute(segment.label)}</button>`).join('')
    : `<span class="path-segment path-segment-static">${COMPUTER_LABEL}</span>`;
  // The scroll key tells render() whether this is still the same listing; the scroll path is where a folder listing's position is remembered.
  const scrollKey = escapeAttribute(`${view}|${tab?.path ?? ''}`);
  const scrollPath = tab?.path && view !== 'tree' && view !== 'search' ? ` data-scroll-path="${escapeAttribute(tab.path)}"` : '';
  const scrollAttrs = `data-scroll-key="${scrollKey}" data-scroll-view="${view}"${scrollPath}`;
  const body = view === 'tree'
    ? `<div class="table-wrap tree-wrap" ${scrollAttrs}>${renderTreeBody(tab)}</div>`
    : view === 'search'
      ? renderSearchBody(tab, index, scrollAttrs)
    : view === 'thumbnails'
      ? `<div class="table-wrap thumb-wrap" ${scrollAttrs}>${renderThumbnails(entries, selected, focusId, index)}</div>`
      : `<div class="table-wrap details-wrap" ${scrollAttrs}><div class="table-head ${isDriveRoot ? 'drive-head' : ''}">${headers}</div><div class="rows">${rows}</div></div>`;
  const undoAction = state.activePane === index && undoStack[undoStack.length - 1];
  const undoLink = undoAction ? `<button class="undo-link" data-undo-delete title="${escapeAttribute(undoDescription(undoAction))}">UNDO DELETE${undoStack.length > 1 ? ` (${undoStack.length})` : ''}</button>` : '';
  const viewSelect = tab?.path ? `<div class="view-switch" role="group" aria-label="Pane view"><span class="view-label">${VIEW_MODES.find((mode) => mode.id === view)?.label ?? ''}</span>${VIEW_MODES.map((mode, modeIndex) => `<button class="view-button ${mode.id === view ? 'active' : ''}" data-view-mode="${mode.id}" title="${mode.label} (${isMac ? '⌥' : 'Alt+'}${modeIndex + 1})" aria-label="${mode.label}" aria-pressed="${mode.id === view}"><svg viewBox="0 0 16 16" aria-hidden="true">${mode.icon}</svg></button>`).join('')}</div>` : '';
  return `<article class="pane ${state.activePane === index ? 'is-active' : ''}" data-pane="${index}"><div class="tabs">${pane.tabs.map((item, tabIndex) => `<button class="tab ${tabIndex === pane.activeTab ? 'active' : ''}" data-tab="${tabIndex}"><span class="tab-dot"></span>${item.label}${pane.tabs.length > 1 ? '<span class="tab-close" data-tab-close title="Close tab">×</span>' : ''}</button>`).join('')}<button class="new-tab" data-new-tab title="New tab">＋</button></div><div class="pathbar"><button class="nav-button" data-up title="Go up one level" aria-label="Go up one level">↑</button><div class="path-text">${breadcrumb}</div></div>${body}<div class="pane-footer"><div class="pane-status"><span class="selection-label"><span class="item-count">${view === 'search' ? searchCountLabel(tab) : `${entries.length} ${entries.length === 1 ? 'ITEM' : 'ITEMS'}`}</span> · ${selected.length ? `${selected.length} SELECTED` : 'NOTHING SELECTED'}</span>${undoLink}</div>${viewSelect}</div></article>`;
}

const HISTORY_LIMIT = 25;
function pushHistory(tab, path) {
  if (!tab.history) { tab.history = [path]; tab.historyIndex = 0; return; }
  if (tab.history[tab.historyIndex] === path) return;
  tab.history = tab.history.slice(0, tab.historyIndex + 1);
  tab.history.push(path);
  if (tab.history.length > HISTORY_LIMIT) tab.history.shift();
  tab.historyIndex = tab.history.length - 1;
}

async function navigateTabTo(index, path, { recordHistory = true, historyIndex } = {}) {
  const pane = state.panes[index]; const tab = currentTab(pane);
  if (!tab) return;
  state.activePane = index;
  const wasTreeView = tab.view === 'tree';
  const wasSearchView = tab.view === 'search';
  const searchQuery = tab.searchState?.query || '';
  if (wasTreeView || wasSearchView) cancelScan(tab);
  const entries = path ? await load(path) : await loadDrives();
  tab.path = path; tab.kind = path ? 'folder' : 'drives'; tab.label = path ? (path.split(/[\\/]/).filter(Boolean).pop() || path) : COMPUTER_LABEL; tab.entries = entries;
  const selection = rememberedOrFirst(entries, path ? stored[path]?.name : null);
  tab.selected = selection; tab.focusId = selection[0] || null; tab.anchorId = tab.focusId;
  if (recordHistory) pushHistory(tab, path); else if (historyIndex !== undefined) tab.historyIndex = historyIndex;
  saveSession();
  if (wasTreeView) {
    if (path) { tab.treeState = null; startTreeView(index); return; }
    tab.view = 'details'; tab.treeState = null;
  }
  // Search view stays on and searches the new folder for the same text.
  if (wasSearchView) {
    tab.searchState = null;
    if (path) { startSearchView(index, searchQuery); return; }
    tab.view = 'details';
  }
  render();
}

function goBack(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.history || tab.historyIndex <= 0) return;
  navigateTabTo(index, tab.history[tab.historyIndex - 1], { recordHistory: false, historyIndex: tab.historyIndex - 1 });
}

function goForward(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.history || tab.historyIndex >= tab.history.length - 1) return;
  navigateTabTo(index, tab.history[tab.historyIndex + 1], { recordHistory: false, historyIndex: tab.historyIndex + 1 });
}

async function navigatePath(path) {
  const normalized = path.trim();
  if (!normalized) return;
  const pane = state.panes[state.activePane];
  if (!currentTab(pane)) { pane.tabs = [drivesTab(await loadDrives())]; pane.activeTab = 0; }
  await navigateTabTo(state.activePane, normalized);
}

function startRename(index, entry) { state.activePane = index; state.editing = { pane: index, entryId: entry.id, original: entry.name }; render(); const input = document.querySelector('[data-rename]'); input?.focus(); input?.select(); }

function cancelRename() { state.editing = null; render(); }

// Pressing anywhere outside the rename box finishes the rename. Row presses call preventDefault (so focus
// never leaves the box and its blur handler never runs), so this listens in the capture phase instead.
// A press elsewhere on the row being renamed is swallowed, so it doesn't select the row or restart the rename.
let swallowClickAfterRename = false;
document.addEventListener('pointerdown', (event) => {
  swallowClickAfterRename = false;
  const input = document.querySelector('[data-rename]');
  if (!input || !state.editing || event.target.closest?.('[data-rename]')) return;
  const sameRow = !!input.closest('[data-entry]') && event.target.closest?.('[data-entry]') === input.closest('[data-entry]');
  const { pane: index, entryId } = state.editing;
  const entry = tabEntries(currentTab(state.panes[index])).find((item) => item.id === entryId);
  input.dataset.replaced = 'true'; // its blur handler would otherwise commit a second time
  finishRename(index, entry, input.value).catch(() => cancelRename());
  if (sameRow) { event.preventDefault(); event.stopPropagation(); swallowClickAfterRename = true; }
}, true);
document.addEventListener('click', (event) => {
  if (!swallowClickAfterRename) return;
  swallowClickAfterRename = false; event.preventDefault(); event.stopPropagation();
}, true);

async function finishRename(index, entry, value) {
  if (!entry) { cancelRename(); return; }
  const newName = value.trim();
  if (!newName || newName === entry.name || INVALID_NAME_CHARS.test(newName)) { cancelRename(); return; }
  state.editing = null;
  try {
    await invoke('rename_entry', { path: entry.path, newName });
  } catch (error) {
    render();
    const kind = entry.kind === 'folder' ? 'folder' : 'file';
    const text = /being used/i.test(String(error))
      ? `Can't rename "${entry.name}" because the ${kind} is in use by another program.\n\nClose it and try again.`
      : `Couldn't rename "${entry.name}".\n\n${error}`;
    await message(text, { title: 'Rename failed', kind: 'error' });
    return;
  }
  const pane = state.panes[index]; const tab = currentTab(pane);
  if (tab.view === 'search' && tab.searchState) {
    const newPath = joinPath(parentPath(entry.path), newName);
    replaceInSearchIndex(tab.searchState, entry.path, newPath);
    selectSingle(tab, newPath);
    await updateSearchMatches(tab);
    tab.entries = await load(tab.path);
    scrollSelectionIntoView = true; render();
    return;
  }
  tab.entries = await load(tab.path);
  selectSingle(tab, tab.entries.find((item) => item.name === newName)?.id || null);
  saveSession(); scrollSelectionIntoView = true; render();
}

// Deletes that Undo can still bring back, newest last. Kept only while the app is running.
const UNDO_LIMIT = 5;
const undoStack = [];
let undoBusy = false;
function itemName(path) { return path.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || path; }

// `trashed` is what delete_entries reports; items it couldn't find in the trash can't be undone.
function rememberDelete(folder, trashed) {
  const entries = trashed.filter((entry) => entry.items.length);
  if (!folder || !entries.length) return;
  undoStack.push({ folder, entries });
  if (undoStack.length > UNDO_LIMIT) undoStack.shift();
}

function undoDescription(action) {
  const names = action.entries.map((entry) => `"${itemName(entry.path)}"`);
  const what = names.length === 1 ? names[0] : `${names.length} items: ${names.slice(0, 5).join(', ')}${names.length > 5 ? ', …' : ''}`;
  const earlier = undoStack.length - 1;
  return `Restore ${what} from the ${TRASH_NAME} to ${action.folder}${earlier ? `\n(${earlier} earlier delete${earlier === 1 ? '' : 's'} can also be undone)` : ''}`;
}

async function undoDelete() {
  const action = undoStack[undoStack.length - 1];
  if (!action || undoBusy) return;
  undoBusy = true;
  undoStack.pop();
  let error = null;
  try { await invoke('restore_entries', { entries: action.entries }); } catch (caught) { error = caught; }
  undoBusy = false;
  await showRestored(state.activePane, action).catch(() => render());
  if (error) await message(`Couldn't restore everything.\n\n${error}`, { title: 'Undo failed', kind: 'error' });
}

// Opens the folder the items went back to and selects them.
async function showRestored(index, action) {
  const tab = currentTab(state.panes[index]);
  if (!tab) return;
  if (tab.view === 'tree' || tab.view === 'search') { cancelScan(tab); leaveSearch(tab); tab.view = 'details'; tab.treeState = null; }
  await navigateTabTo(index, action.folder);
  const restored = new Set(action.entries.map((entry) => normalizePath(entry.path)));
  const ids = tab.entries.filter((entry) => restored.has(normalizePath(entry.path))).map((entry) => entry.id);
  if (!ids.length) return;
  tab.selected = ids; tab.focusId = ids[ids.length - 1]; tab.anchorId = ids[0];
  saveSession(); scrollSelectionIntoView = true; render();
}

async function deleteEntries(index, entries) {
  const targets = entries.filter((entry) => entry && entry.kind !== 'drive');
  if (!targets.length) return;
  const pane = state.panes[index]; const tab = currentTab(pane);
  const { trashed, failures } = await invoke('delete_entries', { paths: targets.map((entry) => entry.path) });
  // Search results can come from any subfolder; Undo reopens the folder the first one was in.
  rememberDelete(tab.view === 'search' ? parentPath(targets[0].path) : tab.path, trashed);
  const failedPaths = new Set(failures.map((failure) => failure.path));
  const deletedIds = new Set(targets.filter((entry) => !failedPaths.has(entry.path)).map((entry) => entry.id));
  if (tab.view === 'search' && tab.searchState) {
    removeFromSearchIndex(tab.searchState, deletedIds);
    await updateSearchMatches(tab);
  }
  tab.entries = await load(tab.path);
  tab.selected = (tab.selected || []).filter((id) => !deletedIds.has(id));
  if (deletedIds.has(tab.focusId)) tab.focusId = null;
  if (deletedIds.has(tab.anchorId)) tab.anchorId = tab.focusId;
  saveSession(); render();
  if (failures.length) await showDeleteFailures(failures.map(({ path, error }) => ({ name: itemName(path), error })));
}

async function showDeleteFailures(failures) {
  const details = failures.map(({ name, error }) => `${name}: ${error}`).join('\n\n');
  const intro = failures.length === 1 ? `Couldn't delete "${failures[0].name}".` : `Couldn't delete ${failures.length} items.`;
  const hint = failures.some(({ error }) => /used by another process|being used|locked/i.test(String(error))) ? '\n\nA file is open in another program. Close it and try again.' : '';
  await message(`${intro}\n\n${details}${hint}`, { title: 'Delete failed', kind: 'error' });
}

function deleteSelected(index) {
  const pane = state.panes[index]; const tab = currentTab(pane);
  if (tab?.view === 'tree') { if (tab.treeState?.selected) deleteTreeNode(index, tab.treeState.selected); return; }
  const entries = (tab?.selected || []).map((id) => tabEntries(tab).find((item) => item.id === id)).filter(Boolean);
  if (entries.length) deleteEntries(index, entries);
}

function selectedFileEntries(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab) return [];
  const ids = new Set(tab.selected || []);
  return tabEntries(tab).filter((entry) => ids.has(entry.id) && entry.kind !== 'drive');
}

async function copySelected(index) {
  const entries = selectedFileEntries(index);
  if (!entries.length) return;
  const paths = entries.map((entry) => entry.path);
  await invoke('clipboard_write_paths', { paths, cut: false });
  state.clipboard = { paths, cut: false };
  render();
}

async function cutSelected(index) {
  const entries = selectedFileEntries(index);
  if (!entries.length) return;
  const paths = entries.map((entry) => entry.path);
  await invoke('clipboard_write_paths', { paths, cut: true });
  state.clipboard = { paths, cut: true };
  render();
}

async function pasteIntoPane(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.path) return;
  let payload;
  try { payload = await invoke('clipboard_read_paths'); } catch { return; }
  if (!payload?.paths.length) return;
  const conflicts = await invoke('scan_paste_conflicts', { paths: payload.paths, destDir: tab.path });
  let conflictResolution;
  if (conflicts.length) {
    const noun = conflicts.length === 1 ? 'file' : 'files';
    const replaceAll = await ask(`${conflicts.length} ${noun} in the destination folder already exist. Replace them with the pasted version, or skip them and keep the existing ones?`, { title: 'Files already exist', okLabel: 'Replace All', cancelLabel: 'Skip' });
    conflictResolution = replaceAll ? 'replace' : 'skip';
  }
  const pastedPaths = await invoke('paste_entries', { paths: payload.paths, destDir: tab.path, cut: payload.cut, conflictResolution });
  if (payload.cut) state.clipboard = null;
  tab.entries = await load(tab.path);
  const pastedNames = new Set(pastedPaths.map((path) => path.split(/[\\/]/).pop()));
  const pastedIds = tab.entries.filter((entry) => pastedNames.has(entry.name)).map((entry) => entry.id);
  if (pastedIds.length) { tab.selected = pastedIds; tab.focusId = pastedIds[pastedIds.length - 1]; tab.anchorId = pastedIds[0]; }
  saveSession();
  scrollSelectionIntoView = true;
  render();
}

async function createNewEntry(index, command) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.path) { render(); return; }
  const path = await invoke(command, { destDir: tab.path });
  tab.entries = await load(tab.path);
  const entry = tab.entries.find((item) => item.path === path);
  if (!entry) { render(); return; }
  selectSingle(tab, entry.id);
  saveSession();
  scrollSelectionIntoView = true;
  startRename(index, entry);
}

async function editSelectedInTextEditor(index) {
  const entries = selectedFileEntries(index).filter((entry) => entry.kind === 'file');
  if (!entries.length) return;
  await invoke('edit_in_text_editor', { paths: entries.map((entry) => entry.path) }).catch((error) => console.error('edit_in_text_editor failed', error));
}

// Zips the selection into a new archive in the same folder, then starts renaming it as Explorer does.
async function compressSelected(index, entryId) {
  const tab = currentTab(state.panes[index]);
  const entries = selectedFileEntries(index);
  if (!tab?.path || !entries.length) return;
  const folderPath = tab.path;
  // The archive is named after the first path, so put the right-clicked item first.
  const ordered = [...entries.filter((entry) => entry.id === entryId), ...entries.filter((entry) => entry.id !== entryId)];
  let archivePath;
  try { archivePath = await invoke('compress_entries', { paths: ordered.map((entry) => entry.path) }); } catch (error) { await message(String(error), { title: 'Compress failed', kind: 'error' }); return; }
  // Skip selecting the archive if the pane has moved on while it was being written.
  if (currentTab(state.panes[index]) !== tab || tab.path !== folderPath) return;
  // In Search view the archive turns up in the results once the rescan sees it.
  if (tab.view === 'search') { render(); return; }
  tab.entries = await load(tab.path);
  const entry = tab.entries.find((item) => item.path === archivePath);
  if (!entry) { render(); return; }
  selectSingle(tab, entry.id);
  saveSession();
  scrollSelectionIntoView = true;
  startRename(index, entry);
}

async function startEntryDrag(index, entry) {
  const tab = currentTab(state.panes[index]);
  const selected = new Set(tab?.selected || []);
  const dragEntries = selected.has(entry.id) ? tabEntries(tab).filter((item) => selected.has(item.id)) : [entry];
  const paths = dragEntries.map((item) => item.path);
  if (!paths.length || !dragIconPath) return;
  internalDragPaths = paths;
  try { await startDrag({ item: paths, icon: dragIconPath }); } catch (error) { console.error('startDrag failed', error); }
  // The webview's drop event can arrive after startDrag resolves, so keep the marker briefly.
  setTimeout(() => { if (internalDragPaths === paths) internalDragPaths = null; }, 1000);
}

function isInternalDrag(paths) {
  if (!internalDragPaths || internalDragPaths.length !== paths.length) return false;
  const dragged = new Set(internalDragPaths.map(normalizePath));
  return paths.every((path) => dragged.has(normalizePath(path)));
}

function driveOf(path) {
  const match = /^[A-Za-z]:/.exec(path);
  return match ? match[0].toLowerCase() : '';
}
function sameDrive(a, b) { return driveOf(a) === driveOf(b); }

function dropTargetAt(position) {
  const scale = window.devicePixelRatio || 1;
  const el = document.elementFromPoint(position.x / scale, position.y / scale);
  const paneEl = el?.closest('.pane');
  if (!paneEl) return null;
  const paneIndex = Number(paneEl.dataset.pane);
  const tab = currentTab(state.panes[paneIndex]);
  if (!tab) return null;
  const rowEl = el.closest('[data-entry]');
  const row = rowEl && tabEntries(tab)[Number(rowEl.dataset.index)];
  const destDir = row && (row.kind === 'folder' || row.kind === 'drive') ? row.path : tab.path;
  if (!destDir) return null;
  return { paneIndex, destDir, hoverEl: row ? rowEl : paneEl };
}

function setDragHover(el) {
  if (dragHoverEl === el) return;
  dragHoverEl?.classList.remove('drop-target');
  dragHoverEl = el || null;
  dragHoverEl?.classList.add('drop-target');
}

// Drags between our own panes always move; drops from other apps follow Explorer's (and Finder's)
// rule of moving within a drive and copying across drives.
async function dropFilesIntoPane(paneIndex, destDir, paths) {
  const cut = isInternalDrag(paths) || (isWindows ? paths.every((path) => sameDrive(path, destDir)) : await invoke('same_volume', { paths, destDir }));
  return transferIntoPane(paneIndex, destDir, paths, { cut, verb: 'dropped' });
}

async function transferIntoPane(paneIndex, destDir, paths, { cut, verb }) {
  const tab = currentTab(state.panes[paneIndex]);
  if (!tab) return;
  const conflicts = await invoke('scan_paste_conflicts', { paths, destDir });
  let conflictResolution;
  if (conflicts.length) {
    const noun = conflicts.length === 1 ? 'file' : 'files';
    const replaceAll = await ask(`${conflicts.length} ${noun} in the destination folder already exist. Replace them with the version being ${verb}, or skip them and keep the existing ones?`, { title: 'Files already exist', okLabel: 'Replace All', cancelLabel: 'Skip' });
    conflictResolution = replaceAll ? 'replace' : 'skip';
  }
  const droppedPaths = await invoke('paste_entries', { paths, destDir, cut, conflictResolution });
  if (normalizePath(tab.path) === normalizePath(destDir)) {
    tab.entries = await load(tab.path);
    const droppedNames = new Set(droppedPaths.map((path) => path.split(/[\\/]/).pop()));
    const droppedIds = tab.entries.filter((entry) => droppedNames.has(entry.name)).map((entry) => entry.id);
    if (droppedIds.length) { tab.selected = droppedIds; tab.focusId = droppedIds[droppedIds.length - 1]; tab.anchorId = droppedIds[0]; }
    saveSession();
    scrollSelectionIntoView = true;
  }
  render();
}

function isSameOrInside(path, folder) { const p = normalizePath(path); const f = normalizePath(folder); return p === f || p.startsWith(`${f}/`); }

function canMoveToOtherPane() {
  const tab = currentTab(state.panes[state.activePane]);
  const destDir = currentTab(state.panes[1 - state.activePane])?.path;
  if (!tab?.path || !destDir || tab.view === 'tree' || normalizePath(tab.path) === normalizePath(destDir)) return false;
  const entries = selectedFileEntries(state.activePane);
  // A folder can't be moved into itself or one of its own subfolders.
  return entries.length > 0 && !entries.some((entry) => isSameOrInside(destDir, entry.path));
}

async function moveSelectedToOtherPane() {
  if (!canMoveToOtherPane()) return;
  const otherIndex = 1 - state.activePane;
  const destDir = currentTab(state.panes[otherIndex]).path;
  const paths = selectedFileEntries(state.activePane).map((entry) => entry.path);
  await transferIntoPane(otherIndex, destDir, paths, { cut: true, verb: 'moved' });
}

function setupExternalDragDrop() {
  getCurrentWebview().onDragDropEvent((event) => {
    const payload = event.payload;
    if (payload.type === 'over' || payload.type === 'enter') {
      setDragHover(dropTargetAt(payload.position)?.hoverEl || null);
      return;
    }
    setDragHover(null);
    if (payload.type !== 'drop') return;
    const target = dropTargetAt(payload.position);
    if (!target || !payload.paths?.length) return;
    dropFilesIntoPane(target.paneIndex, target.destDir, payload.paths).catch((error) => console.error('drop failed', error));
  });
}

function handleTypeAhead(index, char) {
  const tab = currentTab(state.panes[index]);
  // In Search view, typing goes into the search box.
  if (tab?.view === 'search') {
    const input = document.querySelector(`.pane[data-pane="${index}"] [data-search-input]`);
    if (!input) return;
    input.focus();
    input.value += char;
    input.dispatchEvent(new Event('input'));
    return;
  }
  const entries = tab?.entries;
  if (!entries?.length || tab.view === 'tree') return;
  const now = Date.now();
  if (now - typeAheadTime > TYPE_AHEAD_TIMEOUT_MS) typeAheadBuffer = '';
  typeAheadTime = now;
  const nameOf = (entry) => (entry.displayName ?? entry.name ?? '').toLowerCase();
  const findFrom = (term, startIndex) => {
    for (let offset = 0; offset < entries.length; offset += 1) {
      const idx = (startIndex + offset) % entries.length;
      if (nameOf(entries[idx]).startsWith(term)) return idx;
    }
    return -1;
  };
  const currentIndex = entries.findIndex((entry) => entry.id === tab.focusId);
  const lowerChar = char.toLowerCase();
  const isRepeatSingle = typeAheadBuffer.length > 0 && [...typeAheadBuffer].every((letter) => letter.toLowerCase() === lowerChar);
  const query = (isRepeatSingle ? lowerChar : (typeAheadBuffer + char).toLowerCase());
  let matchIndex = findFrom(query, currentIndex + 1);
  if (matchIndex === -1 && query.length > 1) {
    // Extending the search matched nothing: restart the buffer with just this new character.
    matchIndex = findFrom(lowerChar, currentIndex + 1);
    typeAheadBuffer = matchIndex === -1 ? '' : char;
  } else {
    typeAheadBuffer = matchIndex === -1 ? '' : typeAheadBuffer + char;
  }
  if (matchIndex === -1) return;
  selectSingle(tab, entries[matchIndex].id);
  if (tab.path) remember(tab.path, entries[matchIndex].name);
  saveSession();
  scrollSelectionIntoView = true;
  render();
}

function removeTreeNode(node, targetPath) {
  const childIndex = node.children.findIndex((child) => child.path === targetPath);
  if (childIndex !== -1) {
    const [removed] = node.children.splice(childIndex, 1);
    node.fileCount -= removed.fileCount;
    node.totalSize -= removed.totalSize;
    return removed;
  }
  for (const child of node.children) {
    const removed = removeTreeNode(child, targetPath);
    if (removed) {
      node.fileCount -= removed.fileCount;
      node.totalSize -= removed.totalSize;
      return removed;
    }
  }
  return null;
}

async function deleteTreeNode(index, path) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.treeState?.tree || path === tab.treeState.tree.path) return;
  const { trashed, failures } = await invoke('delete_entries', { paths: [path] });
  if (failures.length) { render(); await showDeleteFailures([{ name: itemName(path), error: failures[0].error }]); return; }
  rememberDelete(parentPath(path), trashed);
  removeTreeNode(tab.treeState.tree, path);
  if (tab.treeState.selected === path) tab.treeState.selected = null;
  render();
}

async function navigateToPath(index, path) {
  await navigateTabTo(index, path);
}

async function enter(paneIndex, entry) {
  if (!entry) return;
  const tab = currentTab(state.panes[paneIndex]); rememberSelection(tab, entry);
  if (entry.kind === 'drive' || entry.kind === 'folder') { await navigateTabTo(paneIndex, entry.path); } else { try { await openPath(entry.path); } catch (error) { console.error('openPath failed', entry.path, error); } }
}

// The folder above `path`; '' (the drive list) above a drive root.
function parentPath(path) {
  const trimmed = path.replace(/[\\/]+$/, '');
  if (!isWindows && path.startsWith('/')) {
    if (trimmed === '') return '';
    const lastSlash = trimmed.lastIndexOf('/'); return lastSlash <= 0 ? '/' : trimmed.slice(0, lastSlash);
  }
  const parent = /^[A-Za-z]:$/.test(trimmed) || trimmed === '' ? '' : trimmed.replace(/[\\/][^\\/]+$/, '') || '';
  return /^[A-Za-z]:$/.test(parent) ? `${parent}\\` : parent;
}

async function goUp(index) {
  const tab = currentTab(state.panes[index]); if (!tab || !tab.path) return;
  await navigateTabTo(index, parentPath(tab.path));
}

function moveSelection(index, direction, extend) {
  const pane = state.panes[index]; const tab = currentTab(pane); const entries = tabEntries(tab);
  if (!entries.length) return;
  const current = entries.findIndex((entry) => entry.id === tab.focusId);
  const next = current < 0 ? (direction > 0 ? 0 : entries.length - 1) : Math.max(0, Math.min(entries.length - 1, current + direction));
  const nextEntry = entries[next];
  if (extend) {
    if (!tab.anchorId) tab.anchorId = tab.focusId || nextEntry.id;
    selectRange(tab, entries, tab.anchorId, nextEntry.id);
  } else {
    selectSingle(tab, nextEntry.id);
  }
  rememberSelection(tab, nextEntry);
  saveSession(); scrollSelectionIntoView = true; render();
}

// Arrow keys in the thumbnail grid move the way it looks: up/down by a row, left/right by one card,
// stopping at the edges of the grid rather than wrapping around.
function thumbnailStep(index, key) {
  const tab = currentTab(state.panes[index]); const entries = tab?.entries || [];
  const current = entries.findIndex((entry) => entry.id === tab?.focusId);
  if (current < 0) return key === 'ArrowUp' || key === 'ArrowLeft' ? -1 : 1;
  const cards = [...document.querySelectorAll(`.pane[data-pane="${index}"] .thumb-card`)];
  const columns = Math.max(1, cards.filter((card) => card.offsetTop === cards[0]?.offsetTop).length);
  const last = entries.length - 1;
  if (key === 'ArrowLeft') return current > 0 ? -1 : 0;
  if (key === 'ArrowRight') return current < last ? 1 : 0;
  if (key === 'ArrowUp') return current >= columns ? -columns : 0;
  // Down from a row with nothing directly below lands on the last card, as in Explorer.
  if (current + columns <= last) return columns;
  return Math.floor(current / columns) < Math.floor(last / columns) ? last - current : 0;
}

function startTreeView(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab?.path) return;
  const requestId = `tree-${index}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  tab.view = 'tree';
  tab.treeState = { requestId, status: 'loading', scannedFiles: 0, scannedFolders: 0, tree: null, expanded: new Set([tab.path]), selected: null };
  render();
  invoke('compute_tree_stats', { requestId, path: tab.path }).catch((error) => {
    if (tab.treeState?.requestId !== requestId) return;
    tab.treeState = { ...tab.treeState, status: 'error', error: String(error) };
    render();
  });
}

// A pane always keeps at least one tab, so the last tab has no close button.
function closeTab(index, tabIndex) {
  const pane = state.panes[index];
  const tab = pane.tabs[tabIndex];
  if (!tab || pane.tabs.length <= 1) return;
  cancelScan(tab);
  const closingActive = tabIndex === pane.activeTab;
  if (state.editing?.pane === index && closingActive) state.editing = null;
  pane.tabs.splice(tabIndex, 1);
  if (tabIndex < pane.activeTab) pane.activeTab -= 1;
  else pane.activeTab = Math.min(pane.activeTab, pane.tabs.length - 1);
  state.activePane = index;
  saveSession(); render();
  if (closingActive) refreshActivatedTab(index);
}

function cancelTreeView(index) {
  const tab = currentTab(state.panes[index]);
  if (tab?.treeState?.requestId) invoke('cancel_tree_stats', { requestId: tab.treeState.requestId }).catch(() => {});
  if (tab) { tab.view = 'details'; tab.treeState = null; }
  render();
}

// Stops a tab's Tree or Search scan, including a rescan that's waiting to start.
function cancelScan(tab) {
  for (const scan of [tab?.treeState, tab?.searchState]) {
    if (!scan) continue;
    if (scan.requestId) invoke('cancel_tree_stats', { requestId: scan.requestId }).catch(() => {});
    clearTimeout(scan.rescanTimer); scan.rescanTimer = null;
  }
}

// Search view: lists every file under the tab's folder once (the same limit as Tree view), keeps the
// names in memory, and shows the files whose name contains the search text, at most SEARCH_MAX_RESULTS.
const SEARCH_MAX_RESULTS = 100;

function startSearchView(index, query = '') {
  const tab = currentTab(state.panes[index]);
  if (!tab?.path) return;
  const requestId = `search-${index}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  // The folder listing's selection comes back when the tab leaves Search view.
  const saved = { selected: tab.selected, focusId: tab.focusId, anchorId: tab.anchorId };
  tab.view = 'search';
  tab.searchState = { requestId, status: 'loading', scannedFiles: 0, scannedFolders: 0, paths: [], names: [], query, total: 0, matches: [], saved, querySeq: 0, autoFocus: true };
  tab.selected = []; tab.focusId = null; tab.anchorId = null;
  render();
  runSearchScan(tab, tab.searchState, requestId);
}

// Unlike startSearchView, keeps showing the current results until the new scan arrives.
function rescanSearch(tab) {
  const searchState = tab.searchState;
  const requestId = `search-rescan-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  searchState.requestId = requestId; searchState.rescanning = true; searchState.dirty = false;
  runSearchScan(tab, searchState, requestId);
}

async function runSearchScan(tab, searchState, requestId) {
  const isCurrent = () => tab.searchState === searchState && searchState.requestId === requestId;
  let result;
  try {
    result = await invoke('scan_file_names', { requestId, path: tab.path, showHidden: state.showHidden });
  } catch {
    if (!isCurrent()) return;
    if (searchState.rescanning) { searchState.rescanning = false; return; } // keep the results already showing
    searchState.status = 'error';
    render();
    return;
  }
  if (!isCurrent() || result.status === 'cancelled') return;
  searchState.rescanning = false;
  if (result.status === 'too-many') {
    Object.assign(searchState, { status: 'too-many', paths: [], names: [], matches: [], total: 0 });
    selectSingle(tab, null);
    render();
    return;
  }
  searchState.paths = result.paths;
  searchState.names = result.paths.map((path) => itemName(path).toLowerCase());
  searchState.status = 'done';
  searchState.lastScanEnd = Date.now();
  if (searchState.dirty) {
    searchState.dirty = false;
    const paneIndex = state.panes.findIndex((pane) => currentTab(pane) === tab);
    if (paneIndex !== -1) scheduleRescan(paneIndex, searchState);
  }
  await updateSearchMatches(tab);
}

// Plain text matches anywhere in a (lowercased) name. Text with a * is a wildcard pattern for the whole
// name, * standing for any run of characters: "p*.mov" finds names that start with p and end with .mov.
function searchMatcher(query) {
  const text = query.toLowerCase();
  if (!text.includes('*')) return (name) => name.includes(text);
  const pattern = new RegExp(`^${text.split('*').map((part) => part.replace(/[.+?^${}()|[\]\\]/g, '\\$&')).join('.*')}$`, 's');
  return (name) => pattern.test(name);
}

// Re-runs the search text against the names in memory, then looks up size and date for the files shown.
async function updateSearchMatches(tab) {
  const searchState = tab.searchState;
  if (searchState?.status !== 'done') return;
  const matches = searchMatcher(searchState.query);
  const shown = [];
  let total = 0;
  searchState.names.forEach((name, i) => {
    if (!matches(name)) return;
    total += 1;
    if (shown.length < SEARCH_MAX_RESULTS) shown.push(searchState.paths[i]);
  });
  // Typing fires a search per keystroke; only the latest one gets to update the view.
  const seq = ++searchState.querySeq;
  let entries = [];
  try { entries = await invoke('stat_entries', { paths: shown }); } catch (error) { console.error('stat_entries failed', error); }
  if (tab.searchState !== searchState || searchState.querySeq !== seq) return;
  searchState.total = total;
  searchState.matches = entries.map(decorateEntry);
  const ids = new Set(searchState.matches.map((entry) => entry.id));
  const kept = (tab.selected || []).filter((id) => ids.has(id));
  if (kept.length) {
    tab.selected = kept;
    if (!ids.has(tab.focusId)) tab.focusId = kept[kept.length - 1];
    if (!ids.has(tab.anchorId)) tab.anchorId = tab.focusId;
  } else {
    selectSingle(tab, searchState.matches[0]?.id || null);
  }
  render();
}

function replaceInSearchIndex(searchState, oldPath, newPath) {
  const i = searchState.paths.indexOf(oldPath);
  if (i === -1) return;
  searchState.paths[i] = newPath;
  searchState.names[i] = itemName(newPath).toLowerCase();
}

function removeFromSearchIndex(searchState, removedPaths) {
  const keep = searchState.paths.map((path) => !removedPaths.has(path));
  searchState.paths = searchState.paths.filter((_, i) => keep[i]);
  searchState.names = searchState.names.filter((_, i) => keep[i]);
}

// Puts back the folder listing's selection from before Search view.
function leaveSearch(tab) {
  const saved = tab?.searchState?.saved;
  if (!tab?.searchState) return;
  tab.searchState = null;
  const ids = (saved?.selected || []).filter((id) => tab.entries.some((entry) => entry.id === id));
  if (ids.length) { tab.selected = ids; tab.focusId = ids.includes(saved.focusId) ? saved.focusId : ids[ids.length - 1]; tab.anchorId = ids.includes(saved.anchorId) ? saved.anchorId : tab.focusId; }
  else selectSingle(tab, firstEntryId(tab.entries));
}

function cancelSearchView(index) {
  const tab = currentTab(state.panes[index]);
  if (!tab) return;
  cancelScan(tab); leaveSearch(tab); tab.view = 'details';
  render();
}

// Shows the folder a search result is in, in Details view, with the file selected.
async function openFileLocation(index, entry) {
  const tab = currentTab(state.panes[index]);
  if (!tab) return;
  cancelScan(tab); tab.searchState = null; tab.view = 'details';
  await navigateTabTo(index, parentPath(entry.path));
  if (currentTab(state.panes[index]) !== tab) return;
  const target = tab.entries.find((item) => normalizePath(item.path) === normalizePath(entry.path));
  if (target) { selectSingle(tab, target.id); remember(tab.path, target.name); }
  saveSession(); scrollSelectionIntoView = true; render();
}

function findTabBySearchRequest(requestId) {
  for (const pane of state.panes) for (const tab of pane.tabs) if (tab.searchState?.requestId === requestId) return tab;
  return null;
}

function setPaneView(index, view) {
  const tab = currentTab(state.panes[index]);
  if (!tab) return;
  cancelScan(tab);
  tab.treeState = null;
  leaveSearch(tab);
  if (view === 'tree') { startTreeView(index); return; }
  if (view === 'search') { startSearchView(index); return; }
  tab.view = view;
  render();
}

function findTreeNode(node, targetPath) {
  if (!node) return null;
  if (node.path === targetPath) return node;
  for (const child of node.children) { const found = findTreeNode(child, targetPath); if (found) return found; }
  return null;
}

// navigateTabTo keeps the tab in tree view and rescans the new folder, unless it has no subfolders.
function openTreeNode(index, path) {
  const tab = currentTab(state.panes[index]);
  if (!tab || tab.view !== 'tree') return;
  if (findTreeNode(tab.treeState?.tree, path)?.children.length === 0) {
    if (tab.treeState?.requestId) invoke('cancel_tree_stats', { requestId: tab.treeState.requestId }).catch(() => {});
    tab.view = 'details'; tab.treeState = null;
  }
  navigateTabTo(index, path);
}

function toggleTreeNode(index, path) {
  const expanded = currentTab(state.panes[index])?.treeState?.expanded;
  if (!expanded) return;
  if (expanded.has(path)) expanded.delete(path); else expanded.add(path);
  render();
}

// The rows currently on screen, top to bottom: the root plus the children of every expanded node.
function visibleTreeNodes(node, expanded, parent = null, out = []) {
  out.push({ node, parent });
  if (expanded.has(node.path)) node.children.forEach((child) => visibleTreeNodes(child, expanded, node, out));
  return out;
}

// Up/Down/Home/End move through the visible rows. Right expands a collapsed node (or steps into an
// expanded one); Left collapses an expanded node (or steps out to its parent), as in Explorer's tree.
function handleTreeKey(index, key) {
  const treeState = currentTab(state.panes[index])?.treeState;
  if (treeState?.status !== 'done' || !treeState.tree) return;
  const rows = visibleTreeNodes(treeState.tree, treeState.expanded);
  const current = rows.findIndex((row) => row.node.path === treeState.selected);
  const { node, parent } = rows[current] || {};
  let target = null;
  if (key === 'Home') target = rows[0].node;
  else if (key === 'End') target = rows[rows.length - 1].node;
  else if (current < 0) target = key === 'ArrowUp' ? rows[rows.length - 1].node : rows[0].node;
  else if (key === 'ArrowUp') target = rows[Math.max(0, current - 1)].node;
  else if (key === 'ArrowDown') target = rows[Math.min(rows.length - 1, current + 1)].node;
  else if (key === 'ArrowRight' && node.children.length) {
    if (treeState.expanded.has(node.path)) target = node.children[0];
    else treeState.expanded.add(node.path);
  } else if (key === 'ArrowLeft') {
    if (node.children.length && treeState.expanded.has(node.path)) treeState.expanded.delete(node.path);
    else if (parent) target = parent;
  }
  if (target) treeState.selected = target.path;
  scrollSelectionIntoView = true; render();
}

function findTabByTreeRequest(requestId) {
  for (const pane of state.panes) for (const tab of pane.tabs) if (tab.treeState?.requestId === requestId) return tab;
  return null;
}

function bindEvents() {
  document.querySelectorAll('[data-sort-key]').forEach((button) => button.addEventListener('click', () => setSort(button.dataset.sortKey)));
  document.querySelectorAll('[data-folder-path]').forEach((button) => button.addEventListener('click', async () => { try { await navigatePath(button.dataset.folderPath); } catch { button.classList.add('is-unavailable'); } }));
  document.querySelectorAll('.pane').forEach((element) => { const index = Number(element.dataset.pane); const pane = state.panes[index]; element.addEventListener('click', () => { state.activePane = index; render(); });
    element.addEventListener('contextmenu', (event) => {
      if (event.target.closest('[data-entry]') || event.target.closest('[data-tree-node]')) return;
      event.preventDefault();
      if (state.editing) return;
      state.activePane = index;
      render();
      showContextMenu({ pane: index, kind: 'pane' });
    });
    element.querySelectorAll('[data-entry]').forEach((row) => {
      let suppressNextClick = false;
      const activate = (event, fromPointer) => {
        event.stopPropagation();
        const tab = currentTab(pane);
        const entry = tabEntries(tab)[Number(row.dataset.index)];
        state.activePane = index;
        if (state.editing) return;
        // On a Mac, Ctrl+click is a right-click, so only Command toggles the selection.
        const isToggle = (isMac ? event.metaKey : event.ctrlKey || event.metaKey) && entry.kind !== 'drive';
        if (event.shiftKey && entry.kind !== 'drive') {
          if (!tab.anchorId) tab.anchorId = tab.focusId || entry.id;
          // Ctrl+Shift adds the range to what's already selected instead of replacing it.
          const previous = isToggle ? tab.selected : [];
          selectRange(tab, tabEntries(tab), tab.anchorId, entry.id);
          if (isToggle) { const ids = new Set([...previous, ...tab.selected]); tab.selected = tabEntries(tab).filter((item) => ids.has(item.id)).map((item) => item.id); }
          rememberSelection(tab, entry);
          saveSession(); render();
          return;
        }
        if (isToggle) {
          // Ctrl+click adds or removes one item, leaving the rest of the selection alone.
          clearTimeout(entryClickTimer);
          lastEntryClick = { id: null, time: 0 };
          const ids = new Set(tab.selected);
          if (ids.has(entry.id)) ids.delete(entry.id); else ids.add(entry.id);
          tab.selected = tabEntries(tab).filter((item) => ids.has(item.id)).map((item) => item.id);
          tab.focusId = entry.id; tab.anchorId = entry.id;
          if (ids.has(entry.id)) rememberSelection(tab, entry);
          saveSession(); render();
          return;
        }
        const now = Date.now();
        const isDoubleClick = lastEntryClick.id === entry.id && now - lastEntryClick.time < DOUBLE_CLICK_MS;
        lastEntryClick = { id: entry.id, time: now };
        clearTimeout(entryClickTimer);
        if (isDoubleClick) { lastEntryClick = { id: null, time: 0 }; enter(index, entry); return; }
        const isSoleSelection = tab.selected.length === 1 && tab.selected[0] === entry.id;
        if (fromPointer && entry.kind !== 'drive') {
          // Like Explorer, pressing an already-selected item keeps the selection intact so it can be
          // dragged; rename (or collapsing a multi-selection) only happens once the button is released.
          const wasSelected = tab.selected.includes(entry.id);
          pendingPress = { index, entry, x: event.clientX, y: event.clientY, renameOnRelease: isSoleSelection, selectOnRelease: wasSelected && !isSoleSelection };
          if (wasSelected) return;
        } else if (entry.kind !== 'drive' && isSoleSelection) {
          entryClickTimer = setTimeout(() => startRename(index, entry), DOUBLE_CLICK_MS);
          return;
        }
        selectSingle(tab, entry.id); rememberSelection(tab, entry); saveSession(); render();
      };
      // pointerdown fires immediately; the derived `click` event can lag ~200ms behind it on
      // Windows precision touchpads/touchscreens while the OS gesture recognizer rules out a
      // scroll/pan. Act on pointerdown for real pointer input, and keep `click` only as a
      // fallback for keyboard-triggered activation (Tab + Enter/Space), which never fires pointerdown.
      row.addEventListener('pointerdown', (event) => {
        // Let clicks inside the rename box place the cursor instead of re-selecting the row.
        if (event.button !== 0 || event.target.closest('[data-rename]')) return;
        // Prevent the browser's default focus-on-mousedown behavior: focusing the button
        // inside the scrollable list triggers a native scroll-into-view, which is what was
        // causing the clicked row to jump to the bottom of the pane.
        event.preventDefault();
        suppressNextClick = true;
        activate(event, true);
      });
      row.addEventListener('click', (event) => {
        if (suppressNextClick) { suppressNextClick = false; return; }
        // The rename box sits inside the row's <button>, so typing a space in it makes the browser
        // "press" the row; that click would otherwise schedule a fresh startRename mid-typing.
        if (state.editing) return;
        activate(event);
      });
      row.addEventListener('contextmenu', (event) => {
        event.preventDefault(); event.stopPropagation();
        if (state.editing) return;
        const tab = currentTab(pane);
        const entry = tabEntries(tab)[Number(row.dataset.index)];
        if (entry.kind === 'drive') return;
        state.activePane = index;
        if (!(tab.selected || []).includes(entry.id)) { selectSingle(tab, entry.id); rememberSelection(tab, entry); }
        render();
        showContextMenu({ pane: index, kind: 'file', entryId: entry.id });
      });
      // Drags are started from pointermove (see pendingPress); keep the browser's own drag (e.g. of a thumbnail image) out of the way.
      row.addEventListener('dragstart', (event) => event.preventDefault());
    });
    element.querySelectorAll('[data-tab]').forEach((button) => {
      button.addEventListener('click', (event) => {
        event.stopPropagation();
        if (event.target.closest('[data-tab-close]')) { closeTab(index, Number(button.dataset.tab)); return; }
        const tabIndex = Number(button.dataset.tab);
        const switched = tabIndex !== pane.activeTab;
        state.activePane = index; pane.activeTab = tabIndex; saveSession(); render();
        if (switched) refreshActivatedTab(index);
      });
      // Middle-click closes a tab, as in browsers.
      button.addEventListener('auxclick', (event) => { if (event.button === 1) { event.preventDefault(); event.stopPropagation(); closeTab(index, Number(button.dataset.tab)); } });
    });
    element.querySelector('[data-new-tab]').addEventListener('click', async (event) => { event.stopPropagation(); const current = currentTab(pane); const path = current?.path || ''; const entries = path ? await load(path) : await loadDrives(); const selected = rememberedOrFirst(entries, path ? stored[path]?.name : null); const focusId = selected[0] || null; pane.tabs.push(path ? { path, kind: 'folder', label: path.split(/[\\/]/).pop(), entries, selected, focusId, anchorId: focusId, history: [path], historyIndex: 0 } : { path: '', kind: 'drives', label: COMPUTER_LABEL, entries, selected, focusId, anchorId: focusId, history: [''], historyIndex: 0 }); pane.activeTab = pane.tabs.length - 1; saveSession(); render(); });
    element.querySelector('[data-up]').addEventListener('click', (event) => { event.stopPropagation(); goUp(index); });
    element.querySelector('[data-undo-delete]')?.addEventListener('click', (event) => { event.stopPropagation(); undoDelete(); });
    element.querySelectorAll('[data-breadcrumb]').forEach((button) => button.addEventListener('click', (event) => { event.stopPropagation(); navigateToPath(index, button.dataset.breadcrumb); }));
    element.querySelectorAll('[data-view-mode]').forEach((button) => button.addEventListener('click', (event) => {
      event.stopPropagation();
      state.activePane = index;
      if (currentTab(pane)?.view !== button.dataset.viewMode) setPaneView(index, button.dataset.viewMode); else render();
    }));
    element.querySelector('[data-tree-cancel]')?.addEventListener('click', (event) => { event.stopPropagation(); cancelTreeView(index); });
    element.querySelector('[data-search-cancel]')?.addEventListener('click', (event) => { event.stopPropagation(); cancelSearchView(index); });
    const searchInput = element.querySelector('[data-search-input]');
    searchInput?.addEventListener('click', (event) => { event.stopPropagation(); if (state.activePane !== index) { state.activePane = index; render(); } });
    searchInput?.addEventListener('input', () => {
      const tab = currentTab(pane);
      if (!tab?.searchState) return;
      tab.searchState.query = searchInput.value;
      updateSearchMatches(tab);
    });
    // Up/Down leave the box for the results; Enter opens the selected result.
    searchInput?.addEventListener('keydown', (event) => {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') { event.preventDefault(); searchInput.blur(); state.activePane = index; moveSelection(index, event.key === 'ArrowUp' ? -1 : 1, event.shiftKey); return; }
      if (event.key === 'Enter') { event.preventDefault(); const tab = currentTab(pane); enter(index, tabEntries(tab).find((entry) => entry.id === tab.focusId)); }
    });
    element.querySelectorAll('[data-tree-node]').forEach((row) => {
      row.addEventListener('click', (event) => {
        event.stopPropagation();
        state.activePane = index;
        const path = row.dataset.treeNode;
        if (event.target.closest('[data-tree-caret]')) { toggleTreeNode(index, path); return; }
        const tab = currentTab(pane);
        if (tab?.treeState) tab.treeState.selected = path;
        render();
      });
      row.addEventListener('dblclick', (event) => {
        if (event.target.closest('[data-tree-caret]')) return;
        event.stopPropagation();
        openTreeNode(index, row.dataset.treeNode);
      });
      row.addEventListener('contextmenu', (event) => {
        event.preventDefault(); event.stopPropagation();
        const tab = currentTab(pane);
        if (!tab?.treeState?.tree) return;
        const path = row.dataset.treeNode;
        if (path === tab.treeState.tree.path) return;
        state.activePane = index;
        tab.treeState.selected = path;
        render();
        showContextMenu({ pane: index, kind: 'tree', path });
      });
    });
  });
  document.querySelector('[data-go-back]')?.addEventListener('click', () => goBack(state.activePane));
  document.querySelector('[data-go-forward]')?.addEventListener('click', () => goForward(state.activePane));
  document.querySelector('[data-move-to-other-pane]')?.addEventListener('click', () => moveSelectedToOtherPane().catch((error) => console.error('move to other pane failed', error)));
  document.querySelector('[data-help-menu]').addEventListener('click', (event) => showHelpMenu(event.currentTarget));
  document.querySelector('#hidden-toggle').addEventListener('change', async (event) => { state.showHidden = event.currentTarget.checked; await Promise.all(state.panes.flatMap((pane) => pane.tabs.filter((tab) => tab.path).map(async (tab) => { tab.entries = await load(tab.path); }))); state.panes.forEach((pane) => { const tab = currentTab(pane); if (tab?.view === 'search' && tab.searchState?.status === 'done') rescanSearch(tab); }); render(); });
  const locationInput = document.querySelector('#location-input');
  // Select everything on focus; the mouseup that follows a click-to-focus would otherwise drop the selection to a caret.
  let justFocusedLocation = false;
  locationInput.addEventListener('focus', (event) => { event.currentTarget.select(); justFocusedLocation = true; });
  locationInput.addEventListener('mouseup', (event) => { if (justFocusedLocation) event.preventDefault(); justFocusedLocation = false; });
  locationInput.addEventListener('blur', () => { justFocusedLocation = false; });
  locationInput.addEventListener('keydown', (event) => { if (event.key !== 'Enter') return; event.preventDefault(); const input = event.currentTarget; navigatePath(input.value).catch(() => input.select()); });
  const renameInput = document.querySelector('[data-rename]'); renameInput?.addEventListener('click', (event) => event.stopPropagation()); renameInput?.addEventListener('keyup', (event) => { if (event.key === ' ') event.preventDefault(); /* else releasing Space "presses" the row button the box sits in */ }); renameInput?.addEventListener('keydown', (event) => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); cancelRename(); } if (event.key === 'Enter') { event.preventDefault(); event.stopPropagation(); const entry = tabEntries(currentTab(state.panes[state.activePane])).find((item) => item.id === state.editing?.entryId); finishRename(state.activePane, entry, event.currentTarget.value).catch(() => cancelRename()); } }); renameInput?.addEventListener('blur', (event) => { if (event.currentTarget.dataset.replaced) return; const entry = tabEntries(currentTab(state.panes[state.activePane])).find((item) => item.id === state.editing?.entryId); if (entry) finishRename(state.activePane, entry, event.currentTarget.value).catch(() => cancelRename()); });
}

document.addEventListener('keydown', (event) => {
  // An open dialog takes the keyboard; Tab and Enter still work on its buttons and link.
  if (isDialogOpen()) { if (event.key === 'Escape') { event.preventDefault(); closeDialog(); } return; }
  if (event.key === 'F1') { event.preventDefault(); showShortcuts(); return; }
  const isTyping = event.target.tagName === 'INPUT' || event.target.tagName === 'TEXTAREA';
  const pane = state.panes[state.activePane]; const tab = currentTab(pane);
  const isClipboardCombo = (event.ctrlKey || event.metaKey) && !event.altKey;
  if (!isTyping && isClipboardCombo && !event.shiftKey && event.key.toLowerCase() === 'z') { event.preventDefault(); undoDelete(); return; }
  if (!isTyping && isClipboardCombo && event.key.toLowerCase() === 'c') { event.preventDefault(); copySelected(state.activePane); return; }
  if (!isTyping && isClipboardCombo && event.key.toLowerCase() === 'x') { event.preventDefault(); cutSelected(state.activePane); return; }
  if (!isTyping && isClipboardCombo && event.key.toLowerCase() === 'v') { event.preventDefault(); pasteIntoPane(state.activePane); return; }
  if (event.key === 'Delete' && !isTyping) { event.preventDefault(); deleteSelected(state.activePane); return; }
  // In a Mac text field Option+arrow moves by word, so there it only navigates outside of one.
  // Alt+1…4 switch the active pane's view, even from the search box. event.code, since on a Mac
  // Option+digit types a symbol rather than the digit.
  const viewDigit = /^Digit([1-9])$/.exec(event.code);
  const viewMode = event.altKey && !event.ctrlKey && !event.metaKey && !event.shiftKey && viewDigit && VIEW_MODES[Number(viewDigit[1]) - 1];
  if (viewMode) {
    event.preventDefault();
    if (tab?.path && tab.view !== viewMode.id) setPaneView(state.activePane, viewMode.id);
    return;
  }
  if (event.altKey && event.key === 'ArrowLeft' && !(isMac && isTyping)) { event.preventDefault(); goBack(state.activePane); return; }
  if (event.altKey && event.key === 'ArrowRight' && !(isMac && isTyping)) { event.preventDefault(); goForward(state.activePane); return; }
  if (isTyping) return;
  // Finder's shortcuts: ⌘⌫ to Trash (Mac laptops have no forward-Delete key), ⌘[ / ⌘] back and
  // forward, ⌘↑ to the enclosing folder, ⌘↓ to open.
  if (isMac && event.metaKey && !event.altKey) {
    const action = { Backspace: () => deleteSelected(state.activePane), '[': () => goBack(state.activePane), ']': () => goForward(state.activePane), ArrowUp: () => goUp(state.activePane), ArrowDown: () => enter(state.activePane, tab?.view !== 'tree' && tabEntries(tab).find((entry) => entry.id === tab.focusId)) }[event.key];
    if (action) { event.preventDefault(); action(); return; }
  }
  if (event.key === 'F2') {
    event.preventDefault();
    const entry = tab?.view !== 'tree' && tabEntries(tab).find((item) => item.id === tab.focusId);
    if (entry && entry.kind !== 'drive') { clearTimeout(entryClickTimer); startRename(state.activePane, entry); }
    return;
  }
  if (tab?.view === 'thumbnails' && ['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].includes(event.key)) {
    event.preventDefault();
    const step = thumbnailStep(state.activePane, event.key);
    if (step) moveSelection(state.activePane, step, event.shiftKey);
    return;
  }
  if (tab?.view === 'tree' && ['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) { event.preventDefault(); handleTreeKey(state.activePane, event.key); return; }
  if (event.key === 'ArrowUp' || event.key === 'ArrowDown') { event.preventDefault(); moveSelection(state.activePane, event.key === 'ArrowUp' ? -1 : 1, event.shiftKey); return; }
  if ((event.key === 'Home' || event.key === 'End') && tab?.view !== 'tree') { event.preventDefault(); moveSelection(state.activePane, event.key === 'Home' ? -Infinity : Infinity, event.shiftKey); return; }
  if (event.key === 'Enter' && tab?.view === 'tree') { event.preventDefault(); if (tab.treeState?.selected) openTreeNode(state.activePane, tab.treeState.selected); return; }
  if (event.key === 'Enter') { event.preventDefault(); enter(state.activePane, tabEntries(tab).find((entry) => entry.id === tab.focusId)); return; }
  if (event.key === 'Backspace') { event.preventDefault(); goUp(state.activePane); return; }
  if (!event.ctrlKey && !event.altKey && !event.metaKey && event.key.length === 1) { event.preventDefault(); handleTypeAhead(state.activePane, event.key); }
});
document.addEventListener('pointermove', (event) => {
  if (!pendingPress) return;
  if (!(event.buttons & 1)) { pendingPress = null; return; }
  if (Math.hypot(event.clientX - pendingPress.x, event.clientY - pendingPress.y) < DRAG_THRESHOLD_PX) return;
  const { index, entry } = pendingPress;
  pendingPress = null;
  clearTimeout(entryClickTimer);
  lastEntryClick = { id: null, time: 0 };
  startEntryDrag(index, entry);
});
document.addEventListener('pointerup', () => {
  if (!pendingPress) return;
  const { index, entry, renameOnRelease, selectOnRelease } = pendingPress;
  pendingPress = null;
  if (renameOnRelease) { entryClickTimer = setTimeout(() => startRename(index, entry), DOUBLE_CLICK_MS); return; }
  if (selectOnRelease) {
    const tab = currentTab(state.panes[index]);
    selectSingle(tab, entry.id); rememberSelection(tab, entry); saveSession(); render();
  }
});
document.addEventListener('pointercancel', () => { pendingPress = null; });

function drivesTab(drives, savedSelected) {
  const selected = validOrFirst(drives, savedSelected);
  return { path: '', kind: 'drives', label: COMPUTER_LABEL, entries: drives, selected, focusId: selected[selected.length - 1] || null, anchorId: selected[0] || null, history: [''], historyIndex: 0 };
}

async function start() {
  const bookmarks = loadBookmarks().catch((error) => { console.error('loadBookmarks failed', error); return []; });
  invoke('platform_name').then((name) => { platformName = name; }).catch(() => {});
  invoke('lockhunter_info').then((info) => { lockHunter = info; }).catch(() => {});
  resolveResource('icons/32x32.png').then((path) => { dragIconPath = path; }).catch((error) => console.error('resolveResource failed', error));
  setupExternalDragDrop();
  const drives = await loadDrives();
  const savedPanes = Array.isArray(session.panes) ? session.panes : [];
  for (let index = 0; index < state.panes.length; index += 1) {
    const saved = savedPanes[index];
    const tabs = saved?.tabs?.length ? await Promise.all(saved.tabs.map(async (item) => {
      if (!item.path) return drivesTab(drives, item.selected);
      // A saved folder may have since been moved or deleted; fall back to the drive list rather than failing startup.
      let entries;
      try { entries = await load(item.path); } catch { return drivesTab(drives); }
      const selected = validOrFirst(entries, item.selected);
      return { ...item, kind: 'folder', entries, selected, focusId: selected[selected.length - 1] || null, anchorId: selected[0] || null, history: [item.path], historyIndex: 0 };
    })) : [drivesTab(drives)];
    state.panes[index] = { tabs, activeTab: Math.min(saved?.activeTab || 0, tabs.length - 1) };
  }
  state.folders = await bookmarks;
  render();
}

// Names that don't fit their column are cut off with an ellipsis, so show the whole name on hover.
document.addEventListener('mouseover', (event) => {
  const name = event.target.closest('.file-name strong, .tree-name, .thumb-name');
  if (!name) return;
  if (name.scrollWidth > name.clientWidth) name.title = name.textContent;
  else name.removeAttribute('title');
});

start().catch(() => render());
// A drive was mounted or unmounted: refresh the drive bookmarks and any tab showing the drive list.
listen('drives-changed', async () => {
  const drives = await loadDrives();
  for (const pane of state.panes) {
    for (const tab of pane.tabs) {
      if (tab.kind !== 'drives') continue;
      tab.entries = drives;
      tab.selected = (tab.selected || []).filter((id) => drives.some((drive) => drive.id === id));
      if (!drives.some((drive) => drive.id === tab.focusId)) tab.focusId = tab.selected[0] || null;
      tab.anchorId = tab.focusId;
    }
  }
  render();
});
listen('fs-changed', (event) => { scheduleFolderReload(event.payload); scheduleRescans(event.payload); });
listen('tree-progress', (event) => {
  const tab = findTabByTreeRequest(event.payload.requestId);
  if (!tab || tab.treeState.status !== 'loading') return;
  tab.treeState.scannedFiles = event.payload.scannedFiles;
  tab.treeState.scannedFolders = event.payload.scannedFolders;
  // Update the counter in place: a full render() replaces the Cancel button several times a
  // second, so a click on it would land on a button that's already gone and never fire.
  document.querySelectorAll('[data-tree-progress]').forEach((element) => {
    if (element.dataset.treeProgress === event.payload.requestId) element.textContent = treeProgressText(tab.treeState);
  });
});
listen('search-progress', (event) => {
  const tab = findTabBySearchRequest(event.payload.requestId);
  if (!tab || tab.searchState.status !== 'loading') return;
  tab.searchState.scannedFiles = event.payload.scannedFiles;
  tab.searchState.scannedFolders = event.payload.scannedFolders;
  document.querySelectorAll('[data-tree-progress]').forEach((element) => {
    if (element.dataset.treeProgress === event.payload.requestId) element.textContent = treeProgressText(tab.searchState);
  });
});
listen('tree-too-many', (event) => {
  const tab = findTabByTreeRequest(event.payload.requestId);
  if (!tab) return;
  clearTimeout(tab.treeState.rescanTimer);
  tab.treeState = { ...tab.treeState, status: 'too-many', tree: null, rescanning: false, rescanTimer: null };
  render();
});
listen('tree-done', (event) => {
  const tab = findTabByTreeRequest(event.payload.requestId);
  if (!tab) return;
  const treeState = tab.treeState;
  treeState.status = 'done';
  treeState.tree = event.payload.tree;
  treeState.rescanning = false;
  treeState.lastScanEnd = Date.now();
  if (treeState.selected && !treeContains(treeState.tree, treeState.selected)) treeState.selected = null;
  if (treeState.dirty) {
    treeState.dirty = false;
    const paneIndex = state.panes.findIndex((pane) => currentTab(pane) === tab);
    if (paneIndex !== -1) scheduleRescan(paneIndex, treeState);
  }
  render();
});
